//! 表达/黑话库：学习结果的去重写入、衰减淘汰与读取。

use crate::{Result, Store, now_ms};
use rusqlite::params;

#[derive(Debug, Clone, PartialEq)]
pub struct Expression {
    pub id: i64,
    /// habit / jargon
    pub kind: String,
    pub text: String,
    pub meaning: Option<String>,
    pub situation: String,
    pub triggers: Vec<String>,
    pub hits: i64,
    pub weight: f64,
}

/// 一条学习产出（尚未入库）。
#[derive(Debug, Clone, PartialEq)]
pub struct LearnedExpression {
    pub kind: String,
    pub text: String,
    pub meaning: Option<String>,
    pub situation: String,
    pub triggers: Vec<String>,
}

pub const WEIGHT_MAX: f64 = 5.0;
const WEIGHT_REINFORCE: f64 = 0.5;
const WEIGHT_DECAY: f64 = 0.05;
const WEIGHT_FLOOR: f64 = 0.2;

impl Store {
    /// 写入一批学习结果并执行衰减：本批出现的条目加权，未出现的条目衰减，
    /// 低于下限的删除，总数超过 `cap` 时淘汰权重最低的。返回（新增, 强化, 删除）。
    pub async fn learn_expressions(&self, items: Vec<LearnedExpression>, cap: usize) -> Result<(usize, usize, usize)> {
        self.call(move |c| {
            let tx = c.transaction()?;
            let now = now_ms();
            let (mut added, mut reinforced) = (0, 0);
            let mut touched = Vec::new();
            for it in &items {
                let triggers = serde_json::to_string(&it.triggers)?;
                let existing: Option<i64> = {
                    use rusqlite::OptionalExtension;
                    tx.query_row("SELECT id FROM expressions WHERE kind=?1 AND text=?2", params![it.kind, it.text], |r| r.get(0))
                        .optional()?
                };
                match existing {
                    Some(id) => {
                        tx.execute(
                            "UPDATE expressions SET hits=hits+1, weight=MIN(weight+?2, ?3), situation=?4,
                             meaning=COALESCE(?5, meaning), triggers=?6, last_used=?7 WHERE id=?1",
                            params![id, WEIGHT_REINFORCE, WEIGHT_MAX, it.situation, it.meaning, triggers, now],
                        )?;
                        touched.push(id);
                        reinforced += 1;
                    }
                    None => {
                        tx.execute(
                            "INSERT INTO expressions(situation,text,kind,meaning,hits,weight,last_used,created_at,triggers)
                             VALUES(?1,?2,?3,?4,1,1.0,?5,?5,?6)",
                            params![it.situation, it.text, it.kind, it.meaning, now, triggers],
                        )?;
                        touched.push(tx.last_insert_rowid());
                        added += 1;
                    }
                }
            }
            // 未被本批强化的条目衰减
            let placeholders = if touched.is_empty() { "0".to_owned() } else { touched.iter().map(i64::to_string).collect::<Vec<_>>().join(",") };
            tx.execute(&format!("UPDATE expressions SET weight=weight-?1 WHERE id NOT IN ({placeholders})"), [WEIGHT_DECAY])?;
            let mut removed = tx.execute("DELETE FROM expressions WHERE weight < ?1", [WEIGHT_FLOOR])?;
            removed += tx.execute(
                "DELETE FROM expressions WHERE id NOT IN (SELECT id FROM expressions ORDER BY weight DESC, last_used DESC LIMIT ?1)",
                [cap as i64],
            )?;
            tx.commit()?;
            Ok((added, reinforced, removed))
        })
        .await
    }

    pub async fn expressions(&self) -> Result<Vec<Expression>> {
        self.call(|c| {
            let mut st = c.prepare_cached(
                "SELECT id,kind,text,meaning,situation,triggers,hits,weight FROM expressions ORDER BY weight DESC, id",
            )?;
            let rows = st.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, f64>(7)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, kind, text, meaning, situation, triggers, hits, weight) = row?;
                out.push(Expression { id, kind, text, meaning, situation, triggers: serde_json::from_str(&triggers)?, hits, weight });
            }
            Ok(out)
        })
        .await
    }

    pub async fn forget_expression(&self, id: i64) -> Result<bool> {
        self.call(move |c| Ok(c.execute("DELETE FROM expressions WHERE id=?1", [id])? > 0)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str) -> LearnedExpression {
        LearnedExpression {
            kind: "jargon".into(),
            text: text.into(),
            meaning: Some("失败".into()),
            situation: "说事情搞砸".into(),
            triggers: vec![text.into()],
        }
    }

    #[tokio::test]
    async fn upsert_decay_and_cap() {
        let s = Store::in_memory().unwrap();
        assert_eq!(s.learn_expressions(vec![item("寄了"), item("蚌埠住了")], 300).await.unwrap(), (2, 0, 0));
        assert_eq!(s.learn_expressions(vec![item("寄了")], 300).await.unwrap(), (0, 1, 0));
        let all = s.expressions().await.unwrap();
        assert_eq!(all[0].text, "寄了");
        assert_eq!((all[0].hits, all[0].weight), (2, 1.5));
        assert!((all[1].weight - 0.95).abs() < 1e-9, "未强化的条目应衰减");
        // 超过上限时淘汰权重最低的
        s.learn_expressions(vec![item("a"), item("b")], 2).await.unwrap();
        let left: Vec<_> = s.expressions().await.unwrap().into_iter().map(|e| e.text).collect();
        assert_eq!(left.len(), 2);
        assert!(left.contains(&"寄了".to_owned()));
        let id = s.expressions().await.unwrap()[0].id;
        assert!(s.forget_expression(id).await.unwrap());
        assert!(!s.forget_expression(id).await.unwrap());
    }

    #[tokio::test]
    async fn decays_below_floor_are_removed() {
        let s = Store::in_memory().unwrap();
        s.learn_expressions(vec![item("x")], 300).await.unwrap();
        for _ in 0..20 {
            s.learn_expressions(vec![], 300).await.unwrap();
        }
        assert!(s.expressions().await.unwrap().is_empty());
    }
}
