//! SQLite 存储：单连接专用线程 + WAL。
//!
//! 单用户场景下所有读写串行经过一个线程即可满足吞吐，同时彻底避免 `SQLITE_BUSY`；
//! 异步侧通过 [`Store::call`] 投递闭包并等待结果。

mod cron;
mod expressions;
mod gc;
mod learning;
mod mailbox;
mod migrations;
mod outbox;
mod quota;
mod recall;
mod sessions;

pub use cron::CronJob;
pub use expressions::{Expression, LearnedExpression};
pub use gc::{GcReport, SessionSummary, StoreStats};
pub use learning::{MemoryCandidate, MemoryRevision};
pub use mailbox::{Interrupted, Mail};
pub use outbox::Queued;
pub use recall::RecallHit;
pub use sessions::{Entry, EntryKind, NewSession, SessionKind, SessionRow, SessionStatus};

use lyrabot_core::SessionId;
use rusqlite::Connection;
use std::path::Path;
use std::sync::mpsc as std_mpsc;
use tokio::sync::oneshot;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite：{0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("序列化：{0}")]
    Json(#[from] serde_json::Error),
    #[error("存储线程已关闭")]
    Closed,
    #[error("未找到：{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

#[derive(Clone)]
pub struct Store {
    tx: std_mpsc::Sender<Job>,
}

impl Store {
    /// 打开（或创建）数据库并执行迁移。`path` 为 `:memory:` 时用内存库（测试）。
    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = if path.as_os_str() == ":memory:" {
            Connection::open_in_memory()?
        } else {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| StoreError::Invalid(format!("创建数据目录失败：{e}")))?;
            }
            Connection::open(path)?
        };
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;
             PRAGMA temp_store=MEMORY;",
        )?;
        migrations::run(&mut conn)?;
        let (tx, rx) = std_mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("lyrabot-store".into())
            .spawn(move || {
                // 发送端全部丢弃后循环结束，连接随线程关闭
                for job in rx {
                    job(&mut conn);
                }
            })
            .map_err(|e| StoreError::Invalid(format!("启动存储线程失败：{e}")))?;
        Ok(Self { tx })
    }

    pub fn in_memory() -> Result<Self> {
        Self::open(Path::new(":memory:"))
    }

    /// 在存储线程上执行闭包。
    pub async fn call<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Box::new(move |conn| {
                let _ = tx.send(f(conn));
            }))
            .map_err(|_| StoreError::Closed)?;
        rx.await.map_err(|_| StoreError::Closed)?
    }

    pub async fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_owned();
        self.call(move |c| {
            use rusqlite::OptionalExtension;
            Ok(c.query_row("SELECT value FROM kv WHERE key=?1", [key], |r| r.get(0)).optional()?)
        })
        .await
    }

    /// 会话仍存在时才写入（在存储线程内一次完成，不会与 GC 交错），避免会话被清理后又被后台任务写回它的键。
    /// 返回是否写入。
    pub async fn kv_set_for_session(&self, session: SessionId, key: &str, value: &str) -> Result<bool> {
        let (key, value) = (key.to_owned(), value.to_owned());
        self.call(move |c| {
            let n = c.execute(
                "INSERT INTO kv(key,value) SELECT ?1, ?2 WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ?3)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                rusqlite::params![key, value, session],
            )?;
            Ok(n > 0)
        })
        .await
    }

    pub async fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        let (key, value) = (key.to_owned(), value.to_owned());
        self.call(move |c| {
            c.execute(
                "INSERT INTO kv(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [key, value],
            )?;
            Ok(())
        })
        .await
    }
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod kv_tests {
    use crate::{NewSession, Store};

    #[tokio::test]
    async fn kv_set_for_session_skips_deleted_sessions() {
        let s = Store::in_memory().unwrap();
        let sid = s.create_session(NewSession { model: "m".into(), effort: "low".into(), ..Default::default() }).await.unwrap();
        assert!(s.kv_set_for_session(sid, "learn_ckpt:x", "1").await.unwrap());
        assert!(s.kv_set_for_session(sid, "learn_ckpt:x", "2").await.unwrap(), "已存在时覆盖");
        assert_eq!(s.kv_get("learn_ckpt:x").await.unwrap().as_deref(), Some("2"));
        assert!(!s.kv_set_for_session(sid + 999, "learn_fail:gone", "3:1").await.unwrap(), "会话不存在时不写");
        assert_eq!(s.kv_get("learn_fail:gone").await.unwrap(), None);
    }
}

#[cfg(test)]
mod range_tests {
    use crate::{EntryKind, NewSession, Store};
    use serde_json::json;

    #[tokio::test]
    async fn messages_between_keeps_frozen_batch_when_newer_messages_flood() {
        let s = Store::in_memory().unwrap();
        let sid = s.create_session(NewSession { model: "m".into(), effort: "low".into(), ..Default::default() }).await.unwrap();
        let mut ids = Vec::new();
        for i in 0..5 {
            ids.push(s.append(sid, EntryKind::Message, json!({"role": "user", "content": format!("旧{i}")}), 1).await.unwrap());
        }
        for i in 0..10 {
            s.append(sid, EntryKind::Message, json!({"role": "user", "content": format!("新{i}")}), 1).await.unwrap();
        }
        // 冻结的批次上界在 LIMIT 之前生效：新消息再多，也取回原来那一批
        let batch = s.messages_between(sid, 0, Some(ids[4]), 3).await.unwrap();
        assert_eq!(batch.iter().map(|e| e.id).collect::<Vec<_>>(), ids[2..5].to_vec());
        assert_eq!(s.messages_since(sid, 0, 3).await.unwrap().len(), 3, "无上界时取最新的");
    }
}
