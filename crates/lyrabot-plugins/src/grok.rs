//! grok_search / grok_fetch：经 OpenAI 兼容 Chat Completions 调用具备联网能力的 grok 模型（kind = "openai_chat"）。
//! 移植自 Hermes grok-search 技能的提示词；web_map 与 Tavily 兜底未移植（web_search 已覆盖常规搜索）。

use crate::Host;
use crate::shape::Hit;
use async_trait::async_trait;
use lyrabot_agent::{Concurrency, Tool, ToolCtx, ToolOutput};
use lyrabot_config::{ProviderConfig, Timeouts, Transport};
use lyrabot_core::{Message, ToolSpec};
use lyrabot_llm::{ChatRequest, FinishReason};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const SEARCH_PROMPT: &str = "# 角色: 搜索助手\n\n以 JSON 数组形式返回搜索结果。每个结果必须包含以下字段：\n- \"title\": 字符串，结果标题\n- \"url\": 字符串，有效的 URL\n- \"description\": 字符串，20-50 字摘要\n\n仅输出有效的 JSON 数组，不包含 markdown，不包含解释。";
const FETCH_PROMPT: &str = "# 角色: 网页内容获取器\n\n获取网页内容并转换为结构化 Markdown：\n- 保留所有标题、段落、列表、表格、代码块\n- 包含元数据头部：源 URL、标题、获取时间戳\n- 不要摘要 - 返回完整内容\n- 使用 UTF-8 编码";

/// 用内核的流式 Chat Completions 客户端请求 grok（连接复用、SSE 解析、可重试错误退避重试）。
/// 必须流式：grok 联网检索常需 12–20 秒，前置 CDN 只等 15 秒响应头，非流式请求会被截断为 524；
/// 流式的响应头立即返回，不受该限制。
async fn chat(host: &Host, system: &str, user: String, cancel: &CancellationToken) -> Result<Reply, String> {
    let (c, p) = host.active("grok", "openai_chat")?;
    // 只由总时长 timeout_s 约束：首字节与流空闲上限放宽到同一值，避免默认 90 秒空闲先于总超时触发
    let total_s = p.int("timeout_s", 120) as u64;
    let cfg = ProviderConfig {
        id: format!("plugin-grok:{}", p.name),
        base_url: p.value_or_file(&c, "url")?.trim_end_matches('/').to_owned(),
        api_key: p.secret(&c, "key")?,
        transport: Transport::ChatCompletions,
        user_agent: None,
        effort_map: Default::default(),
        models: Default::default(),
        timeouts: Timeouts { first_byte_s: total_s, idle_s: total_s, ..Timeouts::default() },
    };
    let provider = lyrabot_llm::build(&cfg).map_err(|e| format!("grok 客户端初始化失败：{e}"))?;
    let req = ChatRequest {
        model: p.value_or_file(&c, "model")?,
        system: vec![system.to_owned()],
        messages: vec![Message::user(user)],
        tools: Vec::new(),
        effort: None,
        max_tokens: None,
        cache_key: None,
        strip_video: false,
    };
    // 总超时时取消子令牌，让后台读流任务随之退出，不留连接
    let stream_cancel = cancel.child_token();
    let out = tokio::time::timeout(Duration::from_secs(total_s), provider.complete(req, stream_cancel.clone())).await;
    let out = match out {
        Ok(r) => r.map_err(|e| format!("grok 请求失败：{e}"))?,
        Err(_) => {
            stream_cancel.cancel();
            return Err(format!("grok 请求超时（{total_s} 秒）"));
        }
    };
    let text = out.message.text();
    if text.trim().is_empty() {
        return Err("grok 返回中没有内容".into());
    }
    Ok(Reply { text, truncated: out.finish == FinishReason::Length })
}

/// grok 的回答；`truncated` 表示模型输出达到长度上限被截断。
struct Reply {
    text: String,
    truncated: bool,
}

impl Reply {
    /// 截断时在整理好的结果末尾如实注明。
    fn note(&self, rendered: String) -> String {
        if self.truncated { format!("{rendered}\n（grok 输出达到长度上限，结果可能不完整）") } else { rendered }
    }
}

fn now_beijing() -> String {
    // 时效类查询附带当前时间，避免模型按训练时间理解“今天”
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("当前 UNIX 时间 {t}（请按北京时间理解“今天/最近”）")
}

pub struct GrokSearch {
    spec: ToolSpec,
    host: Host,
}

impl GrokSearch {
    pub fn new(host: Host) -> Self {
        let spec = ToolSpec {
            name: "grok_search".into(),
            description: "用 grok 联网检索，适合需要多来源交叉、社交平台（如 X）或 web_search 结果不够时；明确要求用 grok 搜时也用它。platform 可指定专注的平台。".into(),
            parameters: json!({
                "type": "object",
                "properties": {"query": {"type": "string"}, "platform": {"type": "string"}},
                "required": ["query"],
                "additionalProperties": false
            }),
        };
        Self { spec, host }
    }
}

#[async_trait]
impl Tool for GrokSearch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn call(&self, ctx: ToolCtx, args: Value) -> ToolOutput {
        let Some(q) = args.get("query").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) else { return ToolOutput::err("缺少 query") };
        let platform = args.get("platform").and_then(Value::as_str).map(|p| format!("\n\n专注于平台: {p}")).unwrap_or_default();
        let user = format!("{}\n{q}{platform}\n\n以 JSON 数组形式返回 3-10 个结果。", now_beijing());
        match chat(&self.host, SEARCH_PROMPT, user, &ctx.cancel).await {
            Ok(r) => ToolOutput::ok(r.note(render_search(q, &r.text))),
            Err(e) => ToolOutput::err(e),
        }
    }
}

pub struct GrokFetch {
    spec: ToolSpec,
    host: Host,
}

impl GrokFetch {
    pub fn new(host: Host) -> Self {
        let spec = ToolSpec {
            name: "grok_fetch".into(),
            description: "用 grok 读取一个网页（转为 Markdown），用于阅读搜索结果中的具体页面。只需要其中某部分时写 focus（如“安装步骤”“价格表”），只返回相关段落，读得更快也更省。".into(),
            parameters: json!({
                "type": "object",
                "properties": {"url": {"type": "string"}, "focus": {"type": "string"}},
                "required": ["url"],
                "additionalProperties": false
            }),
        };
        Self { spec, host }
    }
}

#[async_trait]
impl Tool for GrokFetch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn call(&self, ctx: ToolCtx, args: Value) -> ToolOutput {
        let Some(url) = args.get("url").and_then(Value::as_str).filter(|s| s.starts_with("http")) else { return ToolOutput::err("需要 http(s) 开头的 url") };
        let focus = args.get("focus").and_then(Value::as_str).map(str::trim).filter(|f| !f.is_empty());
        let user = match focus {
            Some(f) => format!("{url}\n\n只提取与「{f}」相关的段落、列表、表格或代码，保留原文措辞与关键数据，去掉导航、广告和页脚；不超过 3000 字。"),
            None => format!("{url}\n\n获取并返回结构化 Markdown，去掉导航、广告和页脚。"),
        };
        match chat(&self.host, FETCH_PROMPT, user, &ctx.cancel).await {
            Ok(r) => ToolOutput::ok(r.note(clip(&r.text, FETCH_LIMIT))),
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// 读网页结果的上限（字）。
const FETCH_LIMIT: usize = 12_000;

/// 把 grok 返回的 JSON 数组整理成与 web_search 一致的紧凑列表；不是合法 JSON 时截断原文兜底。
fn render_search(query: &str, raw: &str) -> String {
    let body = raw.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    let list = body.find('[').zip(body.rfind(']')).filter(|(a, b)| a < b).and_then(|(a, b)| serde_json::from_str::<Vec<Value>>(&body[a..=b]).ok());
    let Some(list) = list else { return clip(raw, 4_000) };
    let hits = list
        .iter()
        .map(|it| {
            let s = |k: &str| it.get(k).and_then(Value::as_str).unwrap_or("").trim().to_owned();
            Hit { title: s("title"), url: s("url"), meta: String::new(), summary: s("description") }
        })
        .collect();
    crate::shape::render(query, hits)
}

pub(crate) fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_owned() } else { format!("{}…（已截断）", s.chars().take(n).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::render_search;

    #[test]
    fn grok_results_become_compact_list() {
        let raw = "```json\n[{\"title\":\"A\",\"url\":\"https://a.cn\",\"description\":\"甲\"},{\"title\":\"A\",\"url\":\"https://a.cn/\",\"description\":\"重复\"}]\n```";
        assert_eq!(render_search("q", raw), "1. A\nhttps://a.cn\n甲");
        assert_eq!(render_search("q", "不是 JSON"), "不是 JSON", "解析失败时截断原文兜底");
        assert_eq!(render_search("q", "] 反序 ["), "] 反序 [", "括号反序不越界");
    }
}
