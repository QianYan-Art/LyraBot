//! 计数额度（如 QQ 主动消息月度额度 `qq_proactive:2026-11`）：在存储线程内原子占用与归还，
//! 并发发送不会超额。

use crate::{Result, Store, now_ms};
use rusqlite::{OptionalExtension, params};

impl Store {
    /// 用量未达 `cap` 时加一并返回 `true`，已满返回 `false`。
    pub async fn quota_take(&self, key: &str, cap: u32) -> Result<bool> {
        let key = key.to_owned();
        self.call(move |c| {
            let now = now_ms();
            c.execute("INSERT INTO quota(key, used, updated_at) VALUES(?1, 0, ?2) ON CONFLICT(key) DO NOTHING", params![key, now])?;
            let taken = c.execute("UPDATE quota SET used = used + 1, updated_at = ?3 WHERE key = ?1 AND used < ?2", params![key, cap, now])?;
            Ok(taken == 1)
        })
        .await
    }

    /// 归还一次占用（发送失败时调用）。
    pub async fn quota_release(&self, key: &str) -> Result<()> {
        let key = key.to_owned();
        self.call(move |c| {
            c.execute("UPDATE quota SET used = MAX(used - 1, 0), updated_at = ?2 WHERE key = ?1", params![key, now_ms()])?;
            Ok(())
        })
        .await
    }

    /// 当前用量（没有记录为 0）。
    pub async fn quota_used(&self, key: &str) -> Result<u32> {
        let key = key.to_owned();
        self.call(move |c| Ok(c.query_row("SELECT used FROM quota WHERE key = ?1", [key], |r| r.get(0)).optional()?.unwrap_or(0)))
            .await
    }
}

#[cfg(test)]
mod tests {
    use crate::Store;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn take_is_atomic_under_concurrency_and_release_returns() {
        let s = Store::in_memory().unwrap();
        let tasks: Vec<_> = (0..20).map(|_| {
            let s = s.clone();
            tokio::spawn(async move { s.quota_take("q", 3).await.unwrap() })
        }).collect();
        let mut granted = 0;
        for t in tasks {
            granted += t.await.unwrap() as u32;
        }
        assert_eq!(granted, 3, "并发占用不得超过上限");
        assert_eq!(s.quota_used("q").await.unwrap(), 3);
        s.quota_release("q").await.unwrap();
        assert!(s.quota_take("q", 3).await.unwrap(), "归还后可再次占用");
        s.quota_release("none").await.unwrap();
        assert_eq!(s.quota_used("none").await.unwrap(), 0);
    }
}
