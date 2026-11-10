//! 过往对话检索（`recall` 工具）：主对话角色会话里用户与角色的文字、以及压缩摘要，增量写入 FTS5 trigram 索引。
//! trigram 能匹配任意 3 字以上的中文片段；更短的查询退回子串扫描。GC 删除会话时一并删除索引并回调检查点。
//! 索引与检查点在同一事务里推进（存储线程串行执行），并发调用不会重复入索引。

use crate::{Result, Store};
use rusqlite::OptionalExtension;
use lyrabot_core::{EntryId, SessionId};
use rusqlite::params;

/// 一条检索命中。
#[derive(Debug, Clone, PartialEq)]
pub struct RecallHit {
    pub session: SessionId,
    pub entry: EntryId,
    /// user / assistant / summary
    pub role: String,
    pub at: i64,
    pub text: String,
}

/// 索引进度检查点（kv 键）。GC 删除最高编号的条目后会把它回调到现存最大编号，避免编号复用后漏索引。
pub(crate) const KV_RECALL_CKPT: &str = "recall:ckpt";

/// 单条入索引文本上限（字），避免长工具式回复撑大索引。
const MAX_INDEX_CHARS: usize = 2000;

/// 从条目载荷提取可检索文本与角色：消息取 user/assistant 的文字，压缩条目取摘要。
fn extract(kind: &str, payload: &serde_json::Value) -> Option<(String, String)> {
    let (role, text) = if kind == "compaction" {
        ("summary".to_owned(), payload["summary"].as_str()?.to_owned())
    } else {
        let role = payload["role"].as_str()?;
        if role != "user" && role != "assistant" {
            return None;
        }
        let text = match &payload["content"] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(parts) => {
                parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")
            }
            _ => return None,
        };
        (role.to_owned(), text)
    };
    let text = text.trim();
    (!text.is_empty()).then(|| (role, text.chars().take(MAX_INDEX_CHARS).collect()))
}

impl Store {
    /// 把检查点之后的主对话角色会话条目写入索引并推进检查点（同一事务），返回本次入索引的条目数。
    pub async fn index_recall(&self) -> Result<usize> {
        self.call(move |c| {
            let tx = c.transaction()?;
            let after: EntryId = tx
                .query_row("SELECT value FROM kv WHERE key=?1", [KV_RECALL_CKPT], |r| r.get::<_, String>(0))
                .optional()?
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let rows: Vec<(EntryId, SessionId, String, String, i64)> = {
                let mut st = tx.prepare_cached(
                    "SELECT e.id, e.session_id, e.kind, e.payload, e.created_at FROM entries e
                     JOIN sessions s ON s.id = e.session_id
                     WHERE e.id > ?1 AND s.kind = 'persona' ORDER BY e.id LIMIT 2000",
                )?;
                st.query_map([after], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<rusqlite::Result<_>>()?
            };
            let Some(last) = rows.last().map(|r| r.0) else { return Ok(0) };
            let mut n = 0;
            for (id, session, kind, payload, at) in rows {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) else { continue };
                if let Some((role, text)) = extract(&kind, &v) {
                    tx.execute(
                        "INSERT INTO recall_fts(text, session, entry, role, at) VALUES(?1,?2,?3,?4,?5)",
                        params![text, session, id, role, at],
                    )?;
                    n += 1;
                }
            }
            tx.execute(
                "INSERT INTO kv(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![KV_RECALL_CKPT, last.to_string()],
            )?;
            tx.commit()?;
            Ok(n)
        })
        .await
    }

    /// 检索过往对话（新的在前），排除 `exclude` 会话（当前会话已在上下文里）。
    /// 查询按空白与常见分隔符拆成多个词，分别匹配后合并去重；不少于 3 字的词走 trigram 索引，更短的走子串扫描。
    pub async fn recall_search(&self, query: &str, exclude: SessionId, limit: u32) -> Result<Vec<RecallHit>> {
        let mut terms: Vec<String> = Vec::new();
        for t in query.split(|c: char| c.is_whitespace() || "，,、；;|".contains(c)).map(str::trim).filter(|t| !t.is_empty()) {
            if !terms.iter().any(|x| x == t) {
                terms.push(t.to_owned());
            }
        }
        let mut hits: Vec<RecallHit> = Vec::new();
        for term in terms.into_iter().take(6) {
            for h in self.recall_term(term, exclude, limit).await? {
                if !hits.iter().any(|x| x.entry == h.entry) {
                    hits.push(h);
                }
            }
        }
        hits.sort_by(|a, b| b.at.cmp(&a.at).then(b.entry.cmp(&a.entry)));
        hits.truncate(limit as usize);
        Ok(hits)
    }

    /// 之前会话里最近的若干条对话（新的在前），用于“上次聊了什么”而想不出关键词的情况。
    pub async fn recall_recent(&self, exclude: SessionId, limit: u32) -> Result<Vec<RecallHit>> {
        self.call(move |c| {
            let mut st = c.prepare_cached(
                "SELECT session, entry, role, at, text FROM recall_fts WHERE CAST(session AS INTEGER) != ?1
                 ORDER BY CAST(entry AS INTEGER) DESC LIMIT ?2",
            )?;
            Ok(st
                .query_map(params![exclude, limit], |r| {
                    Ok(RecallHit { session: r.get(0)?, entry: r.get(1)?, role: r.get(2)?, at: r.get(3)?, text: r.get(4)? })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    async fn recall_term(&self, q: String, exclude: SessionId, limit: u32) -> Result<Vec<RecallHit>> {
        self.call(move |c| {
            let map = |r: &rusqlite::Row<'_>| {
                Ok(RecallHit { session: r.get(0)?, entry: r.get(1)?, role: r.get(2)?, at: r.get(3)?, text: r.get(4)? })
            };
            let hits = if q.chars().count() >= 3 {
                // 整体作为短语匹配，双引号转义
                let phrase = format!("\"{}\"", q.replace('"', "\"\""));
                let mut st = c.prepare_cached(
                    "SELECT session, entry, role, at, text FROM recall_fts WHERE recall_fts MATCH ?1 AND CAST(session AS INTEGER) != ?3
                     ORDER BY CAST(at AS INTEGER) DESC, CAST(entry AS INTEGER) DESC LIMIT ?2",
                )?;
                st.query_map(params![phrase, limit, exclude], map)?.collect::<rusqlite::Result<Vec<_>>>()?
            } else {
                let like = format!("%{}%", q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
                let mut st = c.prepare_cached(
                    "SELECT session, entry, role, at, text FROM recall_fts WHERE text LIKE ?1 ESCAPE '\\' AND CAST(session AS INTEGER) != ?3
                     ORDER BY CAST(at AS INTEGER) DESC, CAST(entry AS INTEGER) DESC LIMIT ?2",
                )?;
                st.query_map(params![like, limit, exclude], map)?.collect::<rusqlite::Result<Vec<_>>>()?
            };
            Ok(hits)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::{EntryKind, NewSession, SessionKind, Store};
    use serde_json::json;

    #[tokio::test]
    async fn indexes_persona_text_and_searches_cjk() {
        let s = Store::in_memory().unwrap();
        let p = s.create_session(NewSession { kind: Some(SessionKind::Persona), model: "m".into(), effort: "low".into(), ..Default::default() }).await.unwrap();
        let w = s.create_session(NewSession { kind: Some(SessionKind::Worker), model: "m".into(), effort: "low".into(), ..Default::default() }).await.unwrap();
        s.append(p, EntryKind::Message, json!({"role": "user", "content": [{"type": "text", "text": "我家猫叫年糕，最近不爱吃饭"}]}), 1).await.unwrap();
        s.append(p, EntryKind::Message, json!({"role": "assistant", "content": "年糕可能是换季没胃口"}), 1).await.unwrap();
        s.append(p, EntryKind::Message, json!({"role": "tool", "content": "工具输出 年糕"}), 1).await.unwrap();
        s.append(w, EntryKind::Message, json!({"role": "user", "content": "执行者的年糕任务"}), 1).await.unwrap();
        s.append(p, EntryKind::Compaction, json!({"summary": "聊过猫咪年糕的饮食", "first_kept": null, "tokens_before": 1}), 0).await.unwrap();
        assert_eq!(s.index_recall().await.unwrap(), 3, "三条可检索文本入索引");
        assert_eq!(s.index_recall().await.unwrap(), 0, "检查点已推进，不重复入索引");
        let hits = s.recall_search("不爱吃饭", 0, 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].session, hits[0].role.as_str()), (p, "user"));
        // 两字查询走子串扫描；只包含主对话角色会话、不含工具输出与执行者会话
        let short = s.recall_search("年糕", 0, 10).await.unwrap();
        assert_eq!(short.iter().map(|h| h.role.as_str()).collect::<Vec<_>>(), vec!["summary", "assistant", "user"]);
        assert!(s.recall_search("年糕", p, 10).await.unwrap().is_empty(), "排除指定会话在查询内完成");
        assert!(s.recall_search("100%", 0, 10).await.unwrap().is_empty());
        // 多个词分别匹配后合并去重：一个词命中即可
        let multi = s.recall_search("旅游 不爱吃饭，饮食", 0, 10).await.unwrap();
        assert_eq!(multi.iter().map(|h| h.role.as_str()).collect::<Vec<_>>(), vec!["summary", "user"]);
        // 最近对话：不需要关键词，排除指定会话
        let recent = s.recall_recent(0, 2).await.unwrap();
        assert_eq!(recent.iter().map(|h| h.role.as_str()).collect::<Vec<_>>(), vec!["summary", "assistant"]);
        assert!(s.recall_recent(p, 10).await.unwrap().is_empty());
    }
}
