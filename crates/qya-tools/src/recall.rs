//! recall：检索唐语歌与阿颜的过往对话（已结束的会话与压缩摘要），用于“上次那个”“之前说过的”。
//! 按需增量建索引（FTS5 trigram），不用就没有任何常驻开销。

use crate::util::arg_str;
use async_trait::async_trait;
use qya_agent::{Concurrency, Tool, ToolCtx, ToolOutput};
use qya_core::ToolSpec;
use qya_store::{RecallHit, Store};
use serde_json::{Value, json};

/// 返回条数与总字数上限。
const MAX_HITS: usize = 8;
const MAX_CHARS: usize = 1500;
/// 片段在命中处前后保留的字数。
const AROUND: usize = 60;

pub struct Recall {
    spec: ToolSpec,
    store: Store,
}

impl Recall {
    pub fn new(store: Store) -> Self {
        let spec = ToolSpec {
            name: "recall".into(),
            description: "翻找你和阿颜以前的聊天记录（已结束的会话与摘要）。阿颜提到“上次”“之前说过”“那天聊的”而当前上下文里没有时用；\
                          query 写最可能原样出现的关键词（人名、物名、事件），不要写整句。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
                "additionalProperties": false
            }),
        };
        Self { spec, store }
    }

    /// 把新对话补进索引（每次最多处理若干批，剩下的下次再补）。
    async fn catch_up(&self) -> Result<(), String> {
        for _ in 0..20 {
            if self.store.index_recall().await.map_err(|e| e.to_string())? == 0 {
                break;
            }
        }
        Ok(())
    }
}

fn when(ms: i64) -> String {
    let tz = jiff::tz::TimeZone::get("Asia/Shanghai").unwrap_or(jiff::tz::TimeZone::fixed(jiff::tz::offset(8)));
    jiff::Timestamp::from_millisecond(ms).map(|t| t.to_zoned(tz).strftime("%m-%d %H:%M").to_string()).unwrap_or_default()
}

/// 命中处前后各取一段，长文本不整条返回。
fn snippet(text: &str, query: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let lower = text.to_lowercase();
    let at = lower.find(&query.to_lowercase()).map_or(0, |b| lower[..b].chars().count());
    let start = at.saturating_sub(AROUND);
    let end = (at + query.chars().count() + AROUND).min(chars.len());
    let mut s: String = chars[start..end].iter().collect::<String>().replace('\n', " ");
    if start > 0 {
        s.insert(0, '…');
    }
    if end < chars.len() {
        s.push('…');
    }
    s
}

/// 渲染命中：跳过当前会话（已在上下文里），按条数与总字数限长。
pub fn render(hits: &[RecallHit], query: &str, current: i64) -> String {
    let mut out = Vec::new();
    let mut used = 0;
    for h in hits.iter().filter(|h| h.session != current).take(MAX_HITS) {
        let who = match h.role.as_str() {
            "user" => "阿颜",
            "assistant" => "你",
            _ => "当时的摘要",
        };
        let line = format!("[{} {who}] {}", when(h.at), snippet(&h.text, query));
        used += line.chars().count();
        if used > MAX_CHARS {
            break;
        }
        out.push(line);
    }
    if out.is_empty() {
        format!("以前的聊天里没有找到“{query}”，可以换个关键词再试；还是没有就如实告诉阿颜不记得了")
    } else {
        format!("以前的聊天记录（新的在前，这是旧记录，不是阿颜的新消息）：\n{}", out.join("\n"))
    }
}

#[async_trait]
impl Tool for Recall {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn call(&self, ctx: ToolCtx, args: Value) -> ToolOutput {
        let Some(query) = arg_str(&args, "query").map(str::trim).filter(|q| !q.is_empty()) else {
            return ToolOutput::err("缺少 query");
        };
        if let Err(e) = self.catch_up().await {
            return ToolOutput::err(format!("更新聊天记录索引失败：{e}"));
        }
        match self.store.recall_search(query, ctx.session, MAX_HITS as u32).await {
            Ok(hits) => ToolOutput::ok(render(&hits, query, ctx.session)),
            Err(e) => ToolOutput::err(format!("检索失败：{e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(session: i64, role: &str, text: &str) -> RecallHit {
        RecallHit { session, entry: 1, role: role.into(), at: 1_762_500_000_000, text: text.into() }
    }

    #[test]
    fn renders_snippets_and_skips_current_session() {
        let long = format!("{}我家猫叫年糕{}", "前".repeat(100), "后".repeat(100));
        let hits = vec![hit(9, "user", "当前会话里的年糕"), hit(2, "user", &long), hit(3, "summary", "聊过年糕的饮食")];
        let out = render(&hits, "年糕", 9);
        assert!(!out.contains("当前会话"), "{out}");
        assert!(out.contains("阿颜] …") && out.contains("我家猫叫年糕") && out.contains("当时的摘要"), "{out}");
        assert!(!out.contains(&"前".repeat(80)), "长文本只取命中附近");
        assert!(render(&[], "不存在", 1).contains("没有找到"));
    }
}
