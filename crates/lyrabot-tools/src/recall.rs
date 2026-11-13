//! recall：检索主对话角色与用户的过往对话（之前的会话、压缩摘要，以及本会话已被压缩出上下文的原话），用于“上次那个”“之前说过的”。
//! 按需增量建索引（FTS5 trigram），不用就没有任何常驻开销。

use crate::util::arg_str;
use async_trait::async_trait;
use lyrabot_agent::{Concurrency, Tool, ToolCtx, ToolOutput};
use lyrabot_core::ToolSpec;
use lyrabot_store::{RecallHit, Store};
use serde_json::{Value, json};

/// 返回条数与总字数上限。
const MAX_HITS: usize = 8;
const MAX_CHARS: usize = 1500;
/// 片段在命中处前后保留的字数。
const AROUND: usize = 60;
/// “最近对话”模式下每条保留的字数与条数。
const RECENT_CHARS: usize = 120;
const RECENT_ITEMS: u32 = 12;

pub struct Recall {
    spec: ToolSpec,
    store: Store,
}

impl Recall {
    pub fn new(store: Store) -> Self {
        let spec = ToolSpec {
            name: "recall".into(),
            description: "翻找你们以前的聊天记录（之前的会话、摘要，以及这次聊天里较早、已经压缩掉的原话）。对方提到“上次”“之前说过”“那天聊的”而当前上下文里没有时用。\
                          query 写可能原样出现的关键词，可用空格写几个不同说法（如“苏州 园林 旅游”）；想不出关键词或没搜到时留空，\
                          会返回之前最近的一段对话。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
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

/// 渲染“最近对话”：按时间正序排列，便于读出上下文。
pub fn render_recent(hits: &[RecallHit]) -> String {
    if hits.is_empty() {
        return "之前没有可翻的聊天记录".into();
    }
    let mut lines = Vec::new();
    let mut used = 0;
    for h in hits {
        let who = match h.role.as_str() {
            "user" => "对方",
            "assistant" => "你",
            _ => "当时的摘要",
        };
        let text: String = h.text.chars().take(RECENT_CHARS).collect::<String>().replace('\n', " ");
        let line = format!("[{} {who}] {text}", when(h.at));
        used += line.chars().count();
        if used > MAX_CHARS {
            break;
        }
        lines.push(line);
    }
    lines.reverse();
    format!("之前最近的一段对话（旧记录，不是对方的新消息）：\n{}", lines.join("\n"))
}

/// 渲染命中，按条数与总字数限长。当前会话仍在上下文里的部分已由检索排除，这里不再过滤。
pub fn render(hits: &[RecallHit], query: &str) -> String {
    let mut out = Vec::new();
    let mut used = 0;
    for h in hits.iter().take(MAX_HITS) {
        let who = match h.role.as_str() {
            "user" => "对方",
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
        format!("以前的聊天里没有找到“{query}”。可以换几个说法再试，或者 query 留空看之前最近的一段对话；还是没有就如实告诉对方不记得了")
    } else {
        format!("以前的聊天记录（新的在前，这是旧记录，不是对方的新消息）：\n{}", out.join("\n"))
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
        if let Err(e) = self.catch_up().await {
            return ToolOutput::err(format!("更新聊天记录索引失败：{e}"));
        }
        let query = arg_str(&args, "query").map(str::trim).unwrap_or_default();
        if query.is_empty() {
            return match self.store.recall_recent(ctx.session, RECENT_ITEMS).await {
                Ok(hits) => ToolOutput::ok(render_recent(&hits)),
                Err(e) => ToolOutput::err(format!("检索失败：{e}")),
            };
        }
        match self.store.recall_search(query, ctx.session, MAX_HITS as u32).await {
            Ok(hits) => ToolOutput::ok(render(&hits, query)),
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
    fn renders_snippets() {
        let long = format!("{}我家猫叫年糕{}", "前".repeat(100), "后".repeat(100));
        let hits = vec![hit(2, "user", &long), hit(3, "summary", "聊过年糕的饮食")];
        let out = render(&hits, "年糕");
        assert!(out.contains("对方] …") && out.contains("我家猫叫年糕") && out.contains("当时的摘要"), "{out}");
        assert!(!out.contains(&"前".repeat(80)), "长文本只取命中附近");
        assert!(render(&[], "不存在").contains("没有找到"));
        let recent = render_recent(&[hit(2, "assistant", "后说的"), hit(2, "user", "先说的")]);
        assert!(recent.find("先说的").unwrap() < recent.find("后说的").unwrap(), "按时间正序");
        assert!(render_recent(&[]).contains("没有可翻"));
    }
}
