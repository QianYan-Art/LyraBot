//! 会话树：条目以 parent_id 串成树，会话只记当前叶子。
//! 分叉 = 新会话的叶子直接指向父会话的某个条目，不复制任何条目。

use crate::{Result, Store, StoreError, now_ms};
use lyrabot_core::{EntryId, SessionId};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

macro_rules! str_enum {
    ($name:ident { $($v:ident => $s:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name { $($v),+ }
        impl $name {
            pub fn as_str(self) -> &'static str { match self { $(Self::$v => $s),+ } }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
        }
        impl FromStr for $name {
            type Err = StoreError;
            fn from_str(s: &str) -> std::result::Result<Self, StoreError> {
                match s { $($s => Ok(Self::$v),)+ o => Err(StoreError::Invalid(format!(concat!("未知 ", stringify!($name), "：{}"), o))) }
            }
        }
    };
}

str_enum!(SessionKind { Persona => "persona", Worker => "worker", Specialist => "specialist" });
str_enum!(SessionStatus { Active => "active", Archived => "archived", Closed => "closed" });
str_enum!(EntryKind { Message => "message", Compaction => "compaction", Fold => "fold" });

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub id: SessionId,
    pub kind: SessionKind,
    pub name: Option<String>,
    pub parent_session: Option<SessionId>,
    pub fork_entry: Option<EntryId>,
    pub leaf_id: Option<EntryId>,
    pub model: String,
    pub effort: String,
    pub status: SessionStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: EntryId,
    pub session_id: SessionId,
    pub parent_id: Option<EntryId>,
    pub kind: EntryKind,
    pub payload: serde_json::Value,
    pub tokens: i64,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default)]
pub struct NewSession {
    pub kind: Option<SessionKind>,
    pub name: Option<String>,
    pub parent_session: Option<SessionId>,
    pub fork_entry: Option<EntryId>,
    pub model: String,
    pub effort: String,
}

fn session_from_row(r: &Row<'_>) -> rusqlite::Result<SessionRow> {
    let parse_err = |i, e: StoreError| rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e));
    Ok(SessionRow {
        id: r.get(0)?,
        kind: r.get::<_, String>(1)?.parse().map_err(|e| parse_err(1, e))?,
        name: r.get(2)?,
        parent_session: r.get(3)?,
        fork_entry: r.get(4)?,
        leaf_id: r.get(5)?,
        model: r.get(6)?,
        effort: r.get(7)?,
        status: r.get::<_, String>(8)?.parse().map_err(|e| parse_err(8, e))?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

const SESSION_COLS: &str = "id,kind,name,parent_session,fork_entry,leaf_id,model,effort,status,created_at,updated_at";

impl Store {
    pub async fn create_session(&self, s: NewSession) -> Result<SessionId> {
        self.call(move |c| {
            let now = now_ms();
            let kind = s.kind.unwrap_or(SessionKind::Persona);
            if let Some(fe) = s.fork_entry {
                let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM entries WHERE id=?1)", [fe], |r| r.get(0))?;
                if !exists {
                    return Err(StoreError::NotFound(format!("分叉条目 {fe}")));
                }
            }
            c.execute(
                "INSERT INTO sessions(kind,name,parent_session,fork_entry,leaf_id,model,effort,status,created_at,updated_at)
                 VALUES(?1,?2,?3,?4,?4,?5,?6,'active',?7,?7)",
                params![kind.as_str(), s.name, s.parent_session, s.fork_entry, s.model, s.effort, now],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
    }

    pub async fn session(&self, id: SessionId) -> Result<SessionRow> {
        self.call(move |c| {
            c.query_row(&format!("SELECT {SESSION_COLS} FROM sessions WHERE id=?1"), [id], session_from_row)
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("会话 {id}")))
        })
        .await
    }

    pub async fn list_sessions(&self, kind: SessionKind, status: Option<SessionStatus>, limit: u32) -> Result<Vec<SessionRow>> {
        self.call(move |c| {
            let sql = format!(
                "SELECT {SESSION_COLS} FROM sessions WHERE kind=?1 AND (?2 IS NULL OR status=?2) ORDER BY updated_at DESC LIMIT ?3"
            );
            let mut st = c.prepare_cached(&sql)?;
            let rows = st.query_map(params![kind.as_str(), status.map(|s| s.as_str()), limit], session_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    pub async fn update_session(&self, id: SessionId, model: Option<String>, effort: Option<String>, status: Option<SessionStatus>) -> Result<()> {
        self.call(move |c| {
            let n = c.execute(
                "UPDATE sessions SET model=COALESCE(?2,model), effort=COALESCE(?3,effort), status=COALESCE(?4,status), updated_at=?5 WHERE id=?1",
                params![id, model, effort, status.map(|s| s.as_str()), now_ms()],
            )?;
            if n == 0 {
                return Err(StoreError::NotFound(format!("会话 {id}")));
            }
            Ok(())
        })
        .await
    }

    /// 在会话当前叶子后追加条目并推进叶子；同一事务内完成。
    pub async fn append(&self, session: SessionId, kind: EntryKind, payload: serde_json::Value, tokens: i64) -> Result<EntryId> {
        let payload = serde_json::to_string(&payload)?;
        self.call(move |c| {
            let tx = c.transaction()?;
            let leaf: Option<EntryId> = tx
                .query_row("SELECT leaf_id FROM sessions WHERE id=?1", [session], |r| r.get(0))
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("会话 {session}")))?;
            let now = now_ms();
            tx.execute(
                "INSERT INTO entries(session_id,parent_id,kind,payload,tokens,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
                params![session, leaf, kind.as_str(), payload, tokens, now],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute("UPDATE sessions SET leaf_id=?2, updated_at=?3 WHERE id=?1", params![session, id, now])?;
            tx.commit()?;
            Ok(id)
        })
        .await
    }

    /// 会话自身条目中，id 大于 `after` 的用户消息数（表达学习的触发计数）。
    pub async fn count_user_messages_since(&self, session: SessionId, after: EntryId) -> Result<i64> {
        self.call(move |c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM entries WHERE session_id=?1 AND id>?2 AND kind='message'
                 AND json_extract(payload,'$.role')='user'",
                params![session, after],
                |r| r.get(0),
            )?)
        })
        .await
    }

    /// 会话自身 id 大于 `after` 的消息条目（按 id 升序，至多 `limit` 条，取最新的）。
    pub async fn messages_since(&self, session: SessionId, after: EntryId, limit: u32) -> Result<Vec<Entry>> {
        self.call(move |c| {
            let mut st = c.prepare_cached(
                "SELECT id,session_id,parent_id,kind,payload,tokens,created_at FROM (
                    SELECT * FROM entries WHERE session_id=?1 AND id>?2 AND kind='message' ORDER BY id DESC LIMIT ?3
                 ) ORDER BY id",
            )?;
            let rows = st.query_map(params![session, after, limit], |r| {
                Ok((
                    r.get::<_, EntryId>(0)?,
                    r.get::<_, SessionId>(1)?,
                    r.get::<_, Option<EntryId>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, session_id, parent_id, kind, payload, tokens, created_at) = row?;
                out.push(Entry { id, session_id, parent_id, kind: kind.parse()?, payload: serde_json::from_str(&payload)?, tokens, created_at });
            }
            Ok(out)
        })
        .await
    }

    /// 从根到叶子的完整分支（可跨越父会话，即分叉继承的前缀）。
    pub async fn branch(&self, session: SessionId) -> Result<Vec<Entry>> {
        self.call(move |c| {
            let leaf: Option<EntryId> = c
                .query_row("SELECT leaf_id FROM sessions WHERE id=?1", [session], |r| r.get(0))
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("会话 {session}")))?;
            let Some(leaf) = leaf else { return Ok(Vec::new()) };
            let mut st = c.prepare_cached(
                "WITH RECURSIVE chain(id, depth) AS (
                    SELECT id, 0 FROM entries WHERE id=?1
                    UNION ALL
                    SELECT e.parent_id, chain.depth+1 FROM entries e JOIN chain ON e.id=chain.id WHERE e.parent_id IS NOT NULL
                 )
                 SELECT e.id,e.session_id,e.parent_id,e.kind,e.payload,e.tokens,e.created_at
                 FROM chain JOIN entries e ON e.id=chain.id ORDER BY chain.depth DESC",
            )?;
            let rows = st.query_map([leaf], |r| {
                Ok((
                    r.get::<_, EntryId>(0)?,
                    r.get::<_, SessionId>(1)?,
                    r.get::<_, Option<EntryId>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, session_id, parent_id, kind, payload, tokens, created_at) = row?;
                out.push(Entry {
                    id,
                    session_id,
                    parent_id,
                    kind: kind.parse()?,
                    payload: serde_json::from_str(&payload)?,
                    tokens,
                    created_at,
                });
            }
            Ok(out)
        })
        .await
    }

    /// 读取构建模型上下文所需的有效分支后缀：从叶子回溯到最近一次压缩条目的 `first_kept` 即停，
    /// 避免在长寿命会话（如常驻专员）中每次都读取并反序列化早已被压缩覆盖的远古条目。
    pub async fn context_branch(&self, session: SessionId) -> Result<Vec<Entry>> {
        self.call(move |c| {
            let leaf: Option<EntryId> = c
                .query_row("SELECT leaf_id FROM sessions WHERE id=?1", [session], |r| r.get(0))
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("会话 {session}")))?;
            let Some(mut cur) = leaf else { return Ok(Vec::new()) };
            let mut st = c.prepare_cached("SELECT id,session_id,parent_id,kind,payload,tokens,created_at FROM entries WHERE id=?1")?;
            let mut out = Vec::new();
            let mut stop_after: Option<EntryId> = None;
            let mut seen_compaction = false;
            loop {
                let row = st
                    .query_row([cur], |r| {
                        Ok((
                            r.get::<_, EntryId>(0)?,
                            r.get::<_, SessionId>(1)?,
                            r.get::<_, Option<EntryId>>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, i64>(5)?,
                            r.get::<_, i64>(6)?,
                        ))
                    })
                    .optional()?;
                let Some((id, session_id, parent_id, kind_s, payload_s, tokens, created_at)) = row else { break };
                let kind: EntryKind = kind_s.parse()?;
                let payload: serde_json::Value = serde_json::from_str(&payload_s)?;
                if kind == EntryKind::Compaction {
                    if seen_compaction {
                        break;
                    }
                    seen_compaction = true;
                    match payload.get("first_kept").and_then(serde_json::Value::as_i64) {
                        Some(fk) => stop_after = Some(fk),
                        None => {
                            out.push(Entry { id, session_id, parent_id, kind, payload, tokens, created_at });
                            break;
                        }
                    }
                }
                out.push(Entry { id, session_id, parent_id, kind, payload, tokens, created_at });
                if stop_after == Some(id) {
                    break;
                }
                match parent_id {
                    Some(p) => cur = p,
                    None => break,
                }
            }
            out.reverse();
            Ok(out)
        })
        .await
    }

    /// 原子切换主对话角色的当前会话：在同一事务内校验目标会话存在且为 `persona`、将旧会话标为 `closed`、
    /// 目标会话标为 `active`、确保目标会话具有冻结的记忆快照（已有则保留）、并更新 `persona_session`。
    pub async fn switch_persona_session(&self, old: SessionId, target: SessionId, default_s2: String) -> Result<SessionRow> {
        self.call(move |c| {
            let tx = c.transaction()?;
            let mut row = tx
                .query_row(&format!("SELECT {SESSION_COLS} FROM sessions WHERE id=?1"), [target], session_from_row)
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("会话 {target}")))?;
            if row.kind != SessionKind::Persona {
                return Err(StoreError::Invalid(format!("#{target} 不是主对话会话，无法切换")));
            }
            let now = now_ms();
            if old != target {
                tx.execute("UPDATE sessions SET status='closed', updated_at=?2 WHERE id=?1", params![old, now])?;
            }
            tx.execute("UPDATE sessions SET status='active', updated_at=?2 WHERE id=?1", params![target, now])?;
            tx.execute(
                "INSERT INTO kv(key,value) VALUES(?1,?2) ON CONFLICT(key) DO NOTHING",
                params![format!("persona_s2:{target}"), default_s2],
            )?;
            tx.execute(
                "INSERT INTO kv(key,value) VALUES('persona_session',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [target.to_string()],
            )?;
            tx.commit()?;
            row.status = SessionStatus::Active;
            row.updated_at = now;
            Ok(row)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn new(model: &str) -> NewSession {
        NewSession { model: model.into(), effort: "low".into(), ..Default::default() }
    }

    #[tokio::test]
    async fn fork_is_zero_copy_prefix() {
        let s = Store::in_memory().unwrap();
        let p = s.create_session(new("m")).await.unwrap();
        let e1 = s.append(p, EntryKind::Message, json!({"n":1}), 1).await.unwrap();
        let e2 = s.append(p, EntryKind::Message, json!({"n":2}), 1).await.unwrap();
        s.append(p, EntryKind::Message, json!({"n":3}), 1).await.unwrap();

        let w = s
            .create_session(NewSession { kind: Some(SessionKind::Worker), parent_session: Some(p), fork_entry: Some(e2), ..new("m") })
            .await
            .unwrap();
        s.append(w, EntryKind::Message, json!({"n":"w"}), 1).await.unwrap();

        let wb: Vec<_> = s.branch(w).await.unwrap().into_iter().map(|e| e.payload).collect();
        assert_eq!(wb, vec![json!({"n":1}), json!({"n":2}), json!({"n":"w"})]);
        let pb = s.branch(p).await.unwrap();
        assert_eq!(pb.len(), 3);
        assert_eq!(pb[0].id, e1);
        let total: i64 = s.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?)).await.unwrap();
        assert_eq!(total, 4, "分叉不应复制条目");
    }

    #[tokio::test]
    async fn empty_and_missing() {
        let s = Store::in_memory().unwrap();
        let p = s.create_session(new("m")).await.unwrap();
        assert!(s.branch(p).await.unwrap().is_empty());
        assert!(matches!(s.branch(999).await, Err(StoreError::NotFound(_))));
        assert!(matches!(
            s.create_session(NewSession { fork_entry: Some(42), ..new("m") }).await,
            Err(StoreError::NotFound(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_on_disk() {
        let dir = std::env::temp_dir().join(format!("lyrabot-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::open(&dir.join("t.db")).unwrap();
        let mut ids = Vec::new();
        for _ in 0..4 {
            ids.push(s.create_session(new("m")).await.unwrap());
        }
        let mut handles = Vec::new();
        for i in 0..1000 {
            let s = s.clone();
            let sid = ids[i % 4];
            handles.push(tokio::spawn(async move { s.append(sid, EntryKind::Message, json!({"i": i}), 1).await }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        for sid in ids {
            assert_eq!(s.branch(sid).await.unwrap().len(), 250);
        }
        let mode: String = s.call(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?)).await.unwrap();
        assert_eq!(mode, "wal");
        s.kv_set("a", "1").await.unwrap();
        s.kv_set("a", "2").await.unwrap();
        assert_eq!(s.kv_get("a").await.unwrap().as_deref(), Some("2"));
        drop(s);
        for _ in 0..20 {
            if std::fs::remove_dir_all(&dir).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[tokio::test]
    async fn counts_and_reads_since() {
        let s = Store::in_memory().unwrap();
        let p = s.create_session(new("m")).await.unwrap();
        let u = |t: &str| json!({"role":"user","content":[{"type":"text","text":t}]});
        let a = json!({"role":"assistant","content":[{"type":"text","text":"ok"}]});
        let e1 = s.append(p, EntryKind::Message, u("1"), 1).await.unwrap();
        s.append(p, EntryKind::Message, a.clone(), 1).await.unwrap();
        s.append(p, EntryKind::Message, u("2"), 1).await.unwrap();
        s.append(p, EntryKind::Compaction, json!({"summary":"s"}), 0).await.unwrap();
        assert_eq!(s.count_user_messages_since(p, 0).await.unwrap(), 2);
        assert_eq!(s.count_user_messages_since(p, e1).await.unwrap(), 1);
        let msgs = s.messages_since(p, 0, 2).await.unwrap();
        assert_eq!(msgs.len(), 2, "只取最新 2 条消息，排除压缩条目");
        assert_eq!(msgs[1].payload, u("2"));
    }

    #[tokio::test]
    async fn update_and_list() {
        let s = Store::in_memory().unwrap();
        let id = s.create_session(new("m")).await.unwrap();
        s.update_session(id, Some("k3".into()), None, Some(SessionStatus::Archived)).await.unwrap();
        let row = s.session(id).await.unwrap();
        assert_eq!((row.model.as_str(), row.status), ("k3", SessionStatus::Archived));
        assert_eq!(s.list_sessions(SessionKind::Persona, Some(SessionStatus::Archived), 10).await.unwrap().len(), 1);
        assert!(s.list_sessions(SessionKind::Persona, Some(SessionStatus::Active), 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn context_branch_stops_at_compaction_anchor() {
        let s = Store::in_memory().unwrap();
        let p = s.create_session(new("m")).await.unwrap();
        let _e1 = s.append(p, EntryKind::Message, json!({"n": 1}), 1).await.unwrap();
        let _e2 = s.append(p, EntryKind::Message, json!({"n": 2}), 1).await.unwrap();
        let e3 = s.append(p, EntryKind::Message, json!({"n": 3}), 1).await.unwrap();
        let e4 = s.append(p, EntryKind::Message, json!({"n": 4}), 1).await.unwrap();
        let c1 = s.append(p, EntryKind::Compaction, json!({"summary": "S1", "first_kept": e3, "tokens_before": 10}), 0).await.unwrap();
        let e5 = s.append(p, EntryKind::Message, json!({"n": 5}), 1).await.unwrap();

        // 完整分支有 6 条，而 context_branch 应在 e3 停下，只读 [e3, e4, c1, e5] 共 4 条
        assert_eq!(s.branch(p).await.unwrap().len(), 6);
        let ids: Vec<_> = s.context_branch(p).await.unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![e3, e4, c1, e5]);

        // 再次压缩且 first_kept 为 null 时，直接在最新 compaction 停下
        let c2 = s.append(p, EntryKind::Compaction, json!({"summary": "S2", "first_kept": null, "tokens_before": 20}), 0).await.unwrap();
        let e6 = s.append(p, EntryKind::Message, json!({"n": 6}), 1).await.unwrap();
        let ids2: Vec<_> = s.context_branch(p).await.unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(ids2, vec![c2, e6]);
    }
}
