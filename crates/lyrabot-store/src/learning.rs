//! 长期记忆的后台整理：学习器提取的记忆候选先入库，空闲时批量整理进记忆文件；
//! 每次整理前后的文件内容存为修订，供 `/learn` 查看最近改动与撤销（只保留最近若干次）。

use crate::{Result, Store, now_ms};
use rusqlite::{OptionalExtension, params};

/// 一条待整理的记忆候选。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCandidate {
    pub id: i64,
    /// 目标文件：user（关于用户）或 memory（角色的备忘）
    pub target: String,
    pub text: String,
    /// 依据：stated（用户陈述）/ agreed（用户认可的约定）/ corrected（用户纠正）
    pub basis: String,
    pub created_at: i64,
}

/// 一次整理的修订：`before` / `after` 为 JSON（目标名 → 文件全文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRevision {
    pub id: i64,
    pub summary: String,
    pub before: String,
    pub after: String,
    pub created_at: i64,
}

/// 保留的修订条数。
const KEEP_REVISIONS: i64 = 20;

impl Store {
    pub async fn add_memory_candidates(&self, items: Vec<(String, String, String)>) -> Result<usize> {
        if items.is_empty() {
            return Ok(0);
        }
        self.call(move |c| {
            let tx = c.transaction()?;
            let now = now_ms();
            for (target, text, basis) in &items {
                tx.execute(
                    "INSERT INTO memory_candidates(target, text, basis, created_at) VALUES(?1,?2,?3,?4)",
                    params![target, text, basis, now],
                )?;
            }
            tx.commit()?;
            Ok(items.len())
        })
        .await
    }

    pub async fn memory_candidates(&self) -> Result<Vec<MemoryCandidate>> {
        self.call(|c| {
            let mut st = c.prepare_cached("SELECT id, target, text, basis, created_at FROM memory_candidates ORDER BY id")?;
            let rows = st.query_map([], |r| {
                Ok(MemoryCandidate { id: r.get(0)?, target: r.get(1)?, text: r.get(2)?, basis: r.get(3)?, created_at: r.get(4)? })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// 记录一次整理并删除已处理的候选（同一事务），只保留最近若干条修订。
    pub async fn commit_consolidation(&self, processed: Vec<i64>, summary: String, before: String, after: String) -> Result<i64> {
        self.call(move |c| {
            let tx = c.transaction()?;
            for id in &processed {
                tx.execute("DELETE FROM memory_candidates WHERE id=?1", [id])?;
            }
            tx.execute(
                "INSERT INTO memory_revisions(summary, before, after, created_at) VALUES(?1,?2,?3,?4)",
                params![summary, before, after, now_ms()],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute("DELETE FROM memory_revisions WHERE id <= ?1", [id - KEEP_REVISIONS])?;
            tx.commit()?;
            Ok(id)
        })
        .await
    }

    /// 只删除候选（整理后没有任何改动时）。
    pub async fn drop_memory_candidates(&self, ids: Vec<i64>) -> Result<()> {
        self.call(move |c| {
            let tx = c.transaction()?;
            for id in ids {
                tx.execute("DELETE FROM memory_candidates WHERE id=?1", [id])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn last_memory_revision(&self) -> Result<Option<MemoryRevision>> {
        self.call(|c| {
            Ok(c.query_row(
                "SELECT id, summary, before, after, created_at FROM memory_revisions ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok(MemoryRevision { id: r.get(0)?, summary: r.get(1)?, before: r.get(2)?, after: r.get(3)?, created_at: r.get(4)? }),
            )
            .optional()?)
        })
        .await
    }

    /// 撤销后删除该修订，使下一次撤销指向更早的一次。
    pub async fn delete_memory_revision(&self, id: i64) -> Result<()> {
        self.call(move |c| {
            c.execute("DELETE FROM memory_revisions WHERE id=?1", [id])?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::Store;

    #[tokio::test]
    async fn candidates_and_revisions() {
        let s = Store::in_memory().unwrap();
        let n = s
            .add_memory_candidates(vec![("user".into(), "喜欢少糖".into(), "stated".into()), ("memory".into(), "周末一起复盘".into(), "agreed".into())])
            .await
            .unwrap();
        assert_eq!(n, 2);
        let all = s.memory_candidates().await.unwrap();
        assert_eq!(all.iter().map(|c| c.basis.as_str()).collect::<Vec<_>>(), vec!["stated", "agreed"]);
        for i in 0..22 {
            s.commit_consolidation(if i == 0 { vec![all[0].id] } else { vec![] }, format!("第{i}次"), "{}".into(), "{}".into()).await.unwrap();
        }
        assert_eq!(s.memory_candidates().await.unwrap().len(), 1, "已处理的候选删除");
        let last = s.last_memory_revision().await.unwrap().unwrap();
        assert_eq!(last.summary, "第21次");
        s.delete_memory_revision(last.id).await.unwrap();
        assert_eq!(s.last_memory_revision().await.unwrap().unwrap().summary, "第20次");
        let kept: i64 = s.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM memory_revisions", [], |r| r.get(0))?)).await.unwrap();
        assert!(kept <= 20);
    }
}
