//! 平台发件箱：暂时没有投递通道的出站消息先落库，等通道恢复后按顺序补发，进程重启不丢。

use crate::{Result, Store, now_ms};
use rusqlite::params;

/// 一条排队中的出站消息。
#[derive(Debug, Clone, PartialEq)]
pub struct Queued {
    pub id: i64,
    pub payload: serde_json::Value,
}

impl Store {
    /// 入队；同一渠道超过 `cap` 条时删除最旧的，返回被删除的条数。
    pub async fn outbox_push(&self, channel: &str, payload: serde_json::Value, cap: usize) -> Result<usize> {
        let (channel, text) = (channel.to_owned(), serde_json::to_string(&payload)?);
        self.call(move |c| {
            let tx = c.transaction()?;
            tx.execute("INSERT INTO outbox(channel, payload, created_at) VALUES(?1,?2,?3)", params![channel, text, now_ms()])?;
            let dropped = tx.execute(
                "DELETE FROM outbox WHERE channel=?1 AND id NOT IN (SELECT id FROM outbox WHERE channel=?1 ORDER BY id DESC LIMIT ?2)",
                params![channel, cap as i64],
            )?;
            tx.commit()?;
            Ok(dropped)
        })
        .await
    }

    /// 某渠道排队中的消息（先入的在前）。
    pub async fn outbox_list(&self, channel: &str) -> Result<Vec<Queued>> {
        let channel = channel.to_owned();
        self.call(move |c| {
            let mut st = c.prepare_cached("SELECT id, payload FROM outbox WHERE channel=?1 ORDER BY id")?;
            let rows = st.query_map([channel], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            let mut out = Vec::new();
            for row in rows {
                let (id, text) = row?;
                out.push(Queued { id, payload: serde_json::from_str(&text)? });
            }
            Ok(out)
        })
        .await
    }

    /// 删除已送达（或已合并重新入队）的消息。
    pub async fn outbox_remove(&self, ids: Vec<i64>) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.call(move |c| {
            let tx = c.transaction()?;
            for id in ids {
                tx.execute("DELETE FROM outbox WHERE id=?1", [id])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::Store;
    use serde_json::json;

    #[tokio::test]
    async fn ordered_capped_and_per_channel() {
        let s = Store::in_memory().unwrap();
        for i in 0..4 {
            s.outbox_push("qq", json!({"text": i}), 3).await.unwrap();
        }
        s.outbox_push("cli", json!({"text": "x"}), 3).await.unwrap();
        let q = s.outbox_list("qq").await.unwrap();
        assert_eq!(q.iter().map(|m| m.payload["text"].as_i64().unwrap()).collect::<Vec<_>>(), vec![1, 2, 3], "超出上限丢最旧的");
        s.outbox_remove(vec![q[0].id]).await.unwrap();
        assert_eq!(s.outbox_list("qq").await.unwrap().len(), 2);
        assert_eq!(s.outbox_list("cli").await.unwrap().len(), 1);
    }
}
