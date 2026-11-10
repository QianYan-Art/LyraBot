//! 执行者汇报信箱：先存后做。
//!
//! 派出（或续接）执行者时，任务正文与运行标记（`sessions.run_owner`，即汇报收件会话）在同一事务写入；
//! 结束时汇报写入 `mailbox`，同一事务清掉标记。运行时把汇报交给唐语歌、那一轮输入落库后才确认删除。
//! 因此进程在任何时刻退出，重启后要么还能读到未确认的汇报，要么能从残留标记得知哪些任务被中断。
//! 交给运行中执行者的插话/追问先写入 `run_queue`，下一次 `begin_run` 时清掉（它们要么已被内核写进会话，
//! 要么作为下一轮输入写入）。重启恢复时把会话里还没有的写回去，续接时看得到。

use crate::{Result, Store, StoreError, now_ms};
use qya_core::SessionId;
use rusqlite::{OptionalExtension, params};

/// 一条未确认的汇报。
#[derive(Debug, Clone, PartialEq)]
pub struct Mail {
    pub id: i64,
    pub from: SessionId,
    pub payload: serde_json::Value,
}

/// 仍带运行标记的执行者（启动时即为上次退出时被中断的任务）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interrupted {
    pub worker: SessionId,
    pub owner: SessionId,
    /// 带运行标记（否则只是有插话残留）
    pub marked: bool,
}

/// 按顺序把消息接在会话叶子后（调用方负责事务）。
fn append_messages(tx: &rusqlite::Transaction<'_>, worker: SessionId, rows: &[(String, i64)]) -> Result<()> {
    let mut leaf: Option<i64> = tx
        .query_row("SELECT leaf_id FROM sessions WHERE id=?1", [worker], |r| r.get(0))
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("会话 {worker}")))?;
    let now = now_ms();
    for (payload, tokens) in rows {
        tx.execute(
            "INSERT INTO entries(session_id,parent_id,kind,payload,tokens,created_at) VALUES(?1,?2,'message',?3,?4,?5)",
            params![worker, leaf, payload, tokens, now],
        )?;
        leaf = Some(tx.last_insert_rowid());
    }
    tx.execute("UPDATE sessions SET leaf_id=?2, updated_at=?3 WHERE id=?1", params![worker, leaf, now])?;
    Ok(())
}

fn encode(input: Vec<(serde_json::Value, i64)>) -> Result<Vec<(String, i64)>> {
    input.into_iter().map(|(v, t)| Ok((serde_json::to_string(&v)?, t))).collect()
}

impl Store {
    /// 追加任务输入（`(消息 JSON, token 估算)`，按顺序接在当前叶子后）、打上运行标记（同时切回 active）并清空插话队列，同一事务。
    pub async fn begin_run(&self, worker: SessionId, owner: SessionId, input: Vec<(serde_json::Value, i64)>) -> Result<()> {
        let rows = encode(input)?;
        self.call(move |c| {
            let tx = c.transaction()?;
            append_messages(&tx, worker, &rows)?;
            tx.execute("UPDATE sessions SET run_owner=?2, status='active' WHERE id=?1", params![worker, owner])?;
            tx.execute("DELETE FROM run_queue WHERE worker=?1", [worker])?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// 启动恢复收尾时调用：把所有不带运行标记但仍处于 active 状态的后台/专员会话归档，并清理旧版 specialist:* kv 键。
    pub async fn archive_idle_workers(&self) -> Result<usize> {
        self.call(|c| {
            let tx = c.transaction()?;
            let n = tx.execute(
                "UPDATE sessions SET status='archived' WHERE kind IN ('worker', 'specialist') AND status='active' AND run_owner IS NULL",
                [],
            )?;
            tx.execute("DELETE FROM kv WHERE key LIKE 'specialist:%'", [])?;
            tx.commit()?;
            Ok(n)
        })
        .await
    }

    /// 记下一条交给运行中执行者的插话/追问（运行结束前若进程退出，恢复时写回会话）。
    pub async fn queue_input(&self, worker: SessionId, owner: SessionId, payload: serde_json::Value, tokens: i64) -> Result<()> {
        let text = serde_json::to_string(&payload)?;
        self.call(move |c| {
            c.execute(
                "INSERT INTO run_queue(worker, owner, payload, tokens, created_at) VALUES(?1,?2,?3,?4,?5)",
                params![worker, owner, text, tokens, now_ms()],
            )?;
            Ok(())
        })
        .await
    }

    /// 把残留的插话/追问写回执行者会话并清空，返回实际写回的条数（启动恢复时调用）。
    /// 入队之后会话里出现的同内容条目说明内核已经处理过；按一对一配对抵消，同文的多条插话不会被一条记录全部抵掉。
    pub async fn restore_queue(&self, worker: SessionId) -> Result<usize> {
        self.call(move |c| {
            let tx = c.transaction()?;
            let queued: Vec<(String, i64, i64)> = {
                let mut st = tx.prepare("SELECT payload, tokens, created_at FROM run_queue WHERE worker=?1 ORDER BY id")?;
                st.query_map([worker], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
            };
            let rows: Vec<(String, i64)> = match queued.iter().map(|q| q.2).min() {
                None => Vec::new(),
                Some(since) => {
                    let mut seen: Vec<(String, i64)> = {
                        let mut st = tx.prepare("SELECT payload, created_at FROM entries WHERE session_id=?1 AND created_at>=?2 ORDER BY id")?;
                        st.query_map(params![worker, since], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
                    };
                    queued
                        .into_iter()
                        .filter(|(payload, _, at)| match seen.iter().position(|(p, t)| p == payload && t >= at) {
                            Some(i) => {
                                seen.swap_remove(i);
                                false
                            }
                            None => true,
                        })
                        .map(|(payload, tokens, _)| (payload, tokens))
                        .collect()
                }
            };
            if !rows.is_empty() {
                append_messages(&tx, worker, &rows)?;
            }
            tx.execute("DELETE FROM run_queue WHERE worker=?1", [worker])?;
            tx.commit()?;
            Ok(rows.len())
        })
        .await
    }

    /// 写入汇报并清除运行标记（同一事务），返回信件 id。
    pub async fn post_report(&self, worker: SessionId, owner: SessionId, payload: serde_json::Value) -> Result<i64> {
        let text = serde_json::to_string(&payload)?;
        self.call(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO mailbox(to_session, from_session, payload, created_at) VALUES(?1,?2,?3,?4)",
                params![owner, worker, text, now_ms()],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute("UPDATE sessions SET run_owner=NULL WHERE id=?1", [worker])?;
            tx.commit()?;
            Ok(id)
        })
        .await
    }

    /// 仍带运行标记、或还有插话残留的执行者。只读：调用方为每一项写入汇报时（[`Store::post_report`]）
    /// 才清除标记，处理到一半退出也不会漏掉剩下的。
    pub async fn running_markers(&self) -> Result<Vec<Interrupted>> {
        self.call(|c| {
            let mut st = c.prepare(
                "SELECT id, run_owner, 1 FROM sessions WHERE run_owner IS NOT NULL
                 UNION SELECT worker, MAX(owner), 0 FROM run_queue WHERE worker NOT IN
                     (SELECT id FROM sessions WHERE run_owner IS NOT NULL) GROUP BY worker
                 ORDER BY 1",
            )?;
            Ok(st.query_map([], |r| Ok(Interrupted { worker: r.get(0)?, owner: r.get(1)?, marked: r.get(2)? }))?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// 尚未确认的汇报（先到的在前）。
    pub async fn pending_mail(&self) -> Result<Vec<Mail>> {
        self.call(|c| {
            let mut st = c.prepare_cached("SELECT id, from_session, payload FROM mailbox ORDER BY id")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, SessionId>(1)?, r.get::<_, String>(2)?)))?;
            let mut out = Vec::new();
            for row in rows {
                let (id, from, text) = row?;
                out.push(Mail { id, from, payload: serde_json::from_str(&text)? });
            }
            Ok(out)
        })
        .await
    }

    /// 确认汇报已进入唐语歌的会话，删除对应信件。
    pub async fn ack_mail(&self, ids: Vec<i64>) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.call(move |c| {
            let tx = c.transaction()?;
            for id in ids {
                tx.execute("DELETE FROM mailbox WHERE id=?1", [id])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::{Interrupted, NewSession, Store};
    use serde_json::json;

    async fn session(s: &Store) -> i64 {
        s.create_session(NewSession { model: "m".into(), effort: "low".into(), ..Default::default() }).await.unwrap()
    }

    #[tokio::test]
    async fn run_marker_input_and_report_lifecycle() {
        let s = Store::in_memory().unwrap();
        let (owner, w1, w2) = (session(&s).await, session(&s).await, session(&s).await);
        s.begin_run(w1, owner, vec![(json!({"role": "user", "content": "甲"}), 1)]).await.unwrap();
        s.begin_run(w2, owner, vec![(json!({"role": "user", "content": "乙"}), 1), (json!({"role": "user", "content": "丙"}), 1)]).await.unwrap();
        // 任务正文与标记一起写入，按顺序接在叶子后
        let branch = s.branch(w2).await.unwrap();
        assert_eq!(branch.iter().map(|e| e.payload["content"].as_str().unwrap()).collect::<Vec<_>>(), vec!["乙", "丙"]);
        let id = s.post_report(w1, owner, json!({"summary": "好了"})).await.unwrap();
        // w1 已汇报；w2 的标记一直保留到为它写入汇报
        assert_eq!(s.running_markers().await.unwrap(), vec![Interrupted { worker: w2, owner, marked: true }]);
        assert_eq!(s.running_markers().await.unwrap().len(), 1, "只读查询不清标记");
        // w2 运行中收到三条插话（两条同文），其中一条“丁”已被内核写进会话，随后进程退出：
        // 恢复时只抵消一条“丁”，另一条“丁”和“戊”写回
        s.queue_input(w2, owner, json!({"role": "user", "content": "丁"}), 1).await.unwrap();
        s.queue_input(w2, owner, json!({"role": "user", "content": "丁"}), 1).await.unwrap();
        s.queue_input(w2, owner, json!({"role": "user", "content": "戊"}), 1).await.unwrap();
        s.append(w2, crate::EntryKind::Message, json!({"role": "user", "content": "丁"}), 1).await.unwrap();
        assert_eq!(s.restore_queue(w2).await.unwrap(), 2);
        assert_eq!(s.restore_queue(w2).await.unwrap(), 0);
        let tail: Vec<String> = s.branch(w2).await.unwrap().iter().rev().take(3).map(|e| e.payload["content"].as_str().unwrap().to_owned()).collect();
        assert_eq!(tail, vec!["戊", "丁", "丁"]);
        s.post_report(w2, owner, json!({"summary": "被中断"})).await.unwrap();
        assert!(s.running_markers().await.unwrap().is_empty());
        // 汇报已写、下一轮输入尚未保存时退出：标记已清，但插话残留仍能被发现
        s.queue_input(w1, owner, json!({"role": "user", "content": "己"}), 1).await.unwrap();
        assert_eq!(s.running_markers().await.unwrap(), vec![Interrupted { worker: w1, owner, marked: false }]);
        assert_eq!(s.restore_queue(w1).await.unwrap(), 1);
        assert!(s.running_markers().await.unwrap().is_empty());
        let mail = s.pending_mail().await.unwrap();
        assert_eq!((mail.len(), mail[0].id, mail[0].from), (2, id, w1));
        s.ack_mail(vec![id]).await.unwrap();
        assert_eq!(s.pending_mail().await.unwrap().len(), 1);
    }
}
