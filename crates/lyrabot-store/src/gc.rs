//! 会话清理与统计（/sessions、/gc、每周自动清理）。
//!
//! 可清理：已结束（closed）的主对话角色历史会话、已归档（archived）的后台任务与专员任务会话，且最后更新早于截止时间。
//! 永不清理：调用方指定保留的会话（当前会话）、进行中的会话。
//! 会话之间可能共享历史（fork 的条目指向父会话条目、parent_session 外键），
//! 只有不再被任何保留会话引用的候选才会删除，反复收缩到不动点。

use crate::{Result, Store, now_ms};
use lyrabot_core::SessionId;
use rusqlite::params;
use std::collections::HashSet;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GcReport {
    pub sessions: usize,
    pub entries: usize,
    /// 因仍被保留会话引用而跳过的候选数
    pub kept_referenced: usize,
    pub bytes_before: i64,
    pub bytes_after: i64,
    pub deleted_ids: Vec<SessionId>,
    /// 删除已提交、但之后的整理（检查点与 VACUUM）失败时的错误；`deleted_ids` 仍然有效
    pub maintenance_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: SessionId,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub messages: i64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoreStats {
    /// (kind, status, 数量)
    pub sessions: Vec<(String, String, i64)>,
    pub entries: i64,
    pub bytes: i64,
}

fn db_bytes(c: &rusqlite::Connection) -> rusqlite::Result<i64> {
    let pages: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let size: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    Ok(pages * size)
}

impl Store {
    /// 清理早于 `older_than_days` 天的可清理会话，并压实数据库。
    pub async fn gc_sessions(&self, older_than_days: u32, keep: Vec<SessionId>) -> Result<GcReport> {
        let cutoff = now_ms() - older_than_days as i64 * 86_400_000;
        let report = self
            .call(move |c| {
                let bytes_before = db_bytes(c)?;
                let tx = c.transaction()?;
                let mut cand: HashSet<SessionId> = {
                    let mut st = tx.prepare(
                        "SELECT id FROM sessions WHERE updated_at < ?1 AND run_owner IS NULL AND (
                            (kind='persona' AND status='closed') OR (kind IN ('worker', 'specialist') AND status='archived'))
                         -- 还有未确认汇报的两端、运行中任务的收件会话、或有插话队列残留的会话都要保留
                         AND id NOT IN (SELECT to_session FROM mailbox UNION SELECT from_session FROM mailbox
                                        UNION SELECT run_owner FROM sessions WHERE run_owner IS NOT NULL
                                        UNION SELECT worker FROM run_queue UNION SELECT owner FROM run_queue)",
                    )?;
                    st.query_map([cutoff], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
                };
                for k in &keep {
                    cand.remove(k);
                }
                let total = cand.len();
                // 引用关系：(引用方会话, 被引用会话)
                let refs: Vec<(SessionId, SessionId)> = {
                    let mut st = tx.prepare(
                        "SELECT DISTINCT e.session_id, p.session_id FROM entries e JOIN entries p ON e.parent_id = p.id
                         WHERE e.session_id != p.session_id
                         UNION SELECT id, parent_session FROM sessions WHERE parent_session IS NOT NULL",
                    )?;
                    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
                };
                loop {
                    let blocked: Vec<SessionId> =
                        refs.iter().filter(|(from, to)| !cand.contains(from) && cand.contains(to)).map(|(_, to)| *to).collect();
                    if blocked.is_empty() {
                        break;
                    }
                    for b in blocked {
                        cand.remove(&b);
                    }
                }
                let ids: Vec<SessionId> = {
                    let mut v: Vec<_> = cand.into_iter().collect();
                    v.sort_unstable();
                    v
                };
                let mut entries = 0;
                if !ids.is_empty() {
                    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
                    tx.execute(&format!("DELETE FROM mailbox WHERE to_session IN ({list}) OR from_session IN ({list})"), [])?;
                    tx.execute(&format!("DELETE FROM run_queue WHERE worker IN ({list})"), [])?;
                    tx.execute(&format!("DELETE FROM recall_fts WHERE session IN ({list})"), [])?;
                    // 同一语句内删除整组条目；SQLite 在语句结束时检查外键
                    entries = tx.execute(&format!("DELETE FROM entries WHERE session_id IN ({list})"), [])?;
                    tx.execute(&format!("DELETE FROM sessions WHERE id IN ({list})"), [])?;
                    // 条目编号可能被复用（删掉了编号最大的条目）：检查点回调到现存最大编号，之后的新条目照常入索引
                    tx.execute(
                        "UPDATE kv SET value = CAST(MIN(CAST(value AS INTEGER), (SELECT IFNULL(MAX(id), 0) FROM entries)) AS TEXT) WHERE key = ?1",
                        [crate::recall::KV_RECALL_CKPT],
                    )?;
                    for id in &ids {
                        tx.execute("DELETE FROM kv WHERE key IN (?1, ?2, ?3)", params![format!("persona_s2:{id}"), format!("learn_ckpt:{id}"), format!("learn_fail:{id}")])?;
                    }
                }
                tx.commit()?;
                Ok(GcReport {
                    sessions: ids.len(),
                    entries,
                    kept_referenced: total - ids.len(),
                    bytes_before,
                    bytes_after: 0,
                    deleted_ids: ids,
                    maintenance_error: None,
                })
            })
            .await?;
        // VACUUM 不能在事务内执行；删得少时跳过以免无谓的整库重写。
        // 删除已经提交：整理失败只记在报告里，调用方仍需按 deleted_ids 清理内存状态
        let mut report = report;
        let maintained = self
            .call(move |c| {
                if report_needs_vacuum(c)? {
                    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
                }
                Ok(db_bytes(c)?)
            })
            .await;
        match maintained {
            Ok(bytes) => report.bytes_after = bytes,
            Err(e) => {
                tracing::warn!(error = %e, "会话已删除，但数据库整理失败");
                report.bytes_after = report.bytes_before;
                report.maintenance_error = Some(e.to_string());
            }
        }
        Ok(report)
    }

    /// 主对话角色的会话列表（最近的在前）。
    pub async fn persona_sessions(&self, limit: u32) -> Result<Vec<SessionSummary>> {
        self.call(move |c| {
            let mut st = c.prepare_cached(
                "SELECT s.id, s.status, s.created_at, s.updated_at,
                        (SELECT COUNT(*) FROM entries e WHERE e.session_id = s.id AND e.kind='message')
                 FROM sessions s WHERE s.kind='persona' ORDER BY s.updated_at DESC LIMIT ?1",
            )?;
            let rows = st.query_map([limit], |r| {
                Ok(SessionSummary { id: r.get(0)?, status: r.get(1)?, created_at: r.get(2)?, updated_at: r.get(3)?, messages: r.get(4)? })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    pub async fn stats(&self) -> Result<StoreStats> {
        self.call(|c| {
            let mut st = c.prepare("SELECT kind, status, COUNT(*) FROM sessions GROUP BY kind, status ORDER BY kind, status")?;
            let sessions = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
            let entries = c.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
            Ok(StoreStats { sessions, entries, bytes: db_bytes(c)? })
        })
        .await
    }
}

/// 空闲页超过 1/4 时才 VACUUM。
fn report_needs_vacuum(c: &rusqlite::Connection) -> rusqlite::Result<bool> {
    let free: i64 = c.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let pages: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    Ok(pages > 0 && free * 4 > pages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EntryKind, NewSession, SessionKind, SessionStatus};
    use serde_json::json;

    async fn age(s: &Store, id: SessionId, days: i64) {
        s.call(move |c| {
            c.execute("UPDATE sessions SET updated_at = updated_at - ?2 WHERE id=?1", params![id, days * 86_400_000])?;
            Ok(())
        })
        .await
        .unwrap();
    }

    fn sess(kind: SessionKind) -> NewSession {
        NewSession { kind: Some(kind), model: "m".into(), effort: "low".into(), ..Default::default() }
    }

    #[tokio::test]
    async fn gc_rewinds_recall_checkpoint_when_ids_are_reused() {
        let s = Store::in_memory().unwrap();
        let cur = s.create_session(sess(SessionKind::Persona)).await.unwrap();
        s.append(cur, EntryKind::Message, json!({"role":"user","content":"早上好"}), 1).await.unwrap();
        // 编号最大的条目属于一个已结束的旧会话
        let old = s.create_session(sess(SessionKind::Persona)).await.unwrap();
        s.append(old, EntryKind::Message, json!({"role":"user","content":"旧会话里说过的年糕"}), 1).await.unwrap();
        s.update_session(old, None, None, Some(SessionStatus::Closed)).await.unwrap();
        age(&s, old, 40).await;
        assert_eq!(s.index_recall().await.unwrap(), 2);
        let r = s.gc_sessions(30, vec![cur]).await.unwrap();
        assert_eq!(r.deleted_ids, vec![old]);
        // 删除后新条目会复用旧编号；检查点已回调，仍能入索引
        s.append(cur, EntryKind::Message, json!({"role":"user","content":"新说的年糕近况"}), 1).await.unwrap();
        assert_eq!(s.index_recall().await.unwrap(), 1);
        let hits = s.recall_search("年糕", 0, 10).await.unwrap();
        assert_eq!(hits.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(), vec!["新说的年糕近况"], "旧会话索引已随 GC 删除");
    }

    #[tokio::test]
    async fn gc_keeps_pending_mail_and_run_owners() {
        let s = Store::in_memory().unwrap();
        let (old_persona, owner, w_mail, w_run) = (
            s.create_session(sess(SessionKind::Persona)).await.unwrap(),
            s.create_session(sess(SessionKind::Persona)).await.unwrap(),
            s.create_session(sess(SessionKind::Worker)).await.unwrap(),
            s.create_session(sess(SessionKind::Specialist)).await.unwrap(),
        );
        s.post_report(w_mail, old_persona, json!({"summary": "未确认"})).await.unwrap();
        // 已归档的专员任务被续接（begin_run）时，即使调用方 keep 快照未包含它，GC 也不会误删
        s.update_session(w_run, None, None, Some(SessionStatus::Archived)).await.unwrap();
        s.begin_run(w_run, owner, vec![]).await.unwrap();
        for id in [old_persona, owner] {
            s.update_session(id, None, None, Some(SessionStatus::Closed)).await.unwrap();
        }
        s.update_session(w_mail, None, None, Some(SessionStatus::Archived)).await.unwrap();
        for id in [old_persona, owner, w_mail, w_run] {
            age(&s, id, 40).await;
        }
        let r = s.gc_sessions(30, vec![]).await.unwrap();
        assert!(r.deleted_ids.is_empty(), "{:?}", r.deleted_ids);
        // 汇报确认后即可清理
        let ids: Vec<i64> = s.pending_mail().await.unwrap().iter().map(|m| m.id).collect();
        s.ack_mail(ids).await.unwrap();
        s.post_report(w_run, owner, json!({})).await.unwrap();
        let ids: Vec<i64> = s.pending_mail().await.unwrap().iter().map(|m| m.id).collect();
        s.ack_mail(ids).await.unwrap();
        let r = s.gc_sessions(30, vec![]).await.unwrap();
        assert_eq!(r.deleted_ids, vec![old_persona, owner, w_mail]);
    }

    #[tokio::test]
    async fn gc_respects_age_status_keep_and_references() {
        let s = Store::in_memory().unwrap();
        // 旧主对话角色会话 p1（已结束），fork 出旧 worker w1（已归档）与新 worker w2（已归档但未过期）
        let p1 = s.create_session(sess(SessionKind::Persona)).await.unwrap();
        let e = s.append(p1, EntryKind::Message, json!({"role":"user","content":[]}), 1).await.unwrap();
        let w1 = s.create_session(NewSession { parent_session: Some(p1), fork_entry: Some(e), ..sess(SessionKind::Worker) }).await.unwrap();
        s.append(w1, EntryKind::Message, json!({"role":"user","content":[]}), 1).await.unwrap();
        let w2 = s.create_session(NewSession { parent_session: Some(p1), fork_entry: Some(e), ..sess(SessionKind::Worker) }).await.unwrap();
        s.append(w2, EntryKind::Message, json!({"role":"user","content":[]}), 1).await.unwrap();
        let cur = s.create_session(sess(SessionKind::Persona)).await.unwrap();
        let spec = s.create_session(sess(SessionKind::Specialist)).await.unwrap();
        let spec_done = s.create_session(sess(SessionKind::Specialist)).await.unwrap();
        s.update_session(p1, None, None, Some(SessionStatus::Closed)).await.unwrap();
        s.update_session(w1, None, None, Some(SessionStatus::Archived)).await.unwrap();
        s.update_session(w2, None, None, Some(SessionStatus::Archived)).await.unwrap();
        s.update_session(spec_done, None, None, Some(SessionStatus::Archived)).await.unwrap();
        s.kv_set(&format!("persona_s2:{p1}"), "x").await.unwrap();
        for id in [p1, w1, cur, spec, spec_done] {
            age(&s, id, 40).await;
        }
        // w2 仍新、仍引用 p1 → p1 不能删；w1 与已归档的 spec_done 可删；进行中的 spec 保留
        let r = s.gc_sessions(30, vec![cur]).await.unwrap();
        assert_eq!(r.deleted_ids, vec![w1, spec_done]);
        assert_eq!(r.kept_referenced, 1);
        assert!(s.session(w2).await.is_ok() && s.session(p1).await.is_ok() && s.session(spec).await.is_ok());
        assert_eq!(s.branch(w2).await.unwrap().len(), 2, "保留会话的分叉历史完整");
        // w2 过期后，p1 与 w2 一起删除
        age(&s, w2, 40).await;
        let r = s.gc_sessions(30, vec![cur]).await.unwrap();
        assert_eq!(r.deleted_ids, vec![p1, w2]);
        assert_eq!(s.kv_get(&format!("persona_s2:{p1}")).await.unwrap(), None);
        assert!(s.session(cur).await.is_ok(), "当前会话保留");
        let stats = s.stats().await.unwrap();
        assert_eq!(stats.entries, 0);
        assert_eq!(s.persona_sessions(10).await.unwrap().len(), 1);
    }
}
