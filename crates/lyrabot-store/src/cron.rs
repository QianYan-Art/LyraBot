//! 定时任务存储。调度计算在 lyrabot-tools::cron，这里只负责持久化。

use crate::{Result, Store, StoreError, now_ms};
use rusqlite::params;

#[derive(Debug, Clone, PartialEq)]
pub struct CronJob {
    pub id: i64,
    pub name: String,
    /// 5 段 cron 表达式（北京时间）
    pub schedule: String,
    /// JSON：{"kind":"remind","text":..} 或 {"kind":"task","to":..,"task":..}
    pub action: serde_json::Value,
    pub enabled: bool,
    pub last_run: Option<i64>,
    pub created_at: i64,
}

/// 原始行：id, name, schedule, action(JSON), enabled, last_run, created_at
type RawRow = (i64, String, String, String, bool, Option<i64>, i64);

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
}

impl Store {
    pub async fn cron_add(&self, name: &str, schedule: &str, action: serde_json::Value) -> Result<i64> {
        let (name, schedule, action) = (name.to_owned(), schedule.to_owned(), serde_json::to_string(&action)?);
        self.call(move |c| {
            c.execute(
                "INSERT INTO cron_jobs(name,schedule,action,enabled,created_at) VALUES(?1,?2,?3,1,?4)",
                params![name, schedule, action, now_ms()],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
    }

    pub async fn cron_list(&self) -> Result<Vec<CronJob>> {
        self.call(|c| {
            let mut st = c.prepare_cached("SELECT id,name,schedule,action,enabled,last_run,created_at FROM cron_jobs ORDER BY id")?;
            let rows = st.query_map([], row)?;
            let mut out = Vec::new();
            for r in rows {
                let (id, name, schedule, action, enabled, last_run, created_at) = r?;
                out.push(CronJob { id, name, schedule, action: serde_json::from_str(&action)?, enabled, last_run, created_at });
            }
            Ok(out)
        })
        .await
    }

    pub async fn cron_set_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        self.call(move |c| {
            if c.execute("UPDATE cron_jobs SET enabled=?2 WHERE id=?1", params![id, enabled])? == 0 {
                return Err(StoreError::NotFound(format!("定时任务 {id}")));
            }
            Ok(())
        })
        .await
    }

    pub async fn cron_remove(&self, id: i64) -> Result<bool> {
        self.call(move |c| Ok(c.execute("DELETE FROM cron_jobs WHERE id=?1", [id])? > 0)).await
    }

    pub async fn cron_mark_run(&self, id: i64, at_ms: i64) -> Result<()> {
        self.call(move |c| {
            c.execute("UPDATE cron_jobs SET last_run=?2 WHERE id=?1", params![id, at_ms])?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn crud() {
        let s = Store::in_memory().unwrap();
        let id = s.cron_add("早安", "0 9 * * *", serde_json::json!({"kind":"remind","text":"早"})).await.unwrap();
        s.cron_set_enabled(id, false).await.unwrap();
        s.cron_mark_run(id, 123).await.unwrap();
        let j = &s.cron_list().await.unwrap()[0];
        assert_eq!((j.enabled, j.last_run, j.action["kind"].as_str()), (false, Some(123), Some("remind")));
        assert!(s.cron_remove(id).await.unwrap());
        assert!(s.cron_set_enabled(id, true).await.is_err());
    }
}
