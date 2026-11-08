//! grok_search / grok_fetch：经 OpenAI 兼容 Chat Completions 调用具备联网能力的 grok 模型（kind = "openai_chat"）。
//! 移植自 Hermes grok-search 技能的提示词；web_map 与 Tavily 兜底未移植（web_search 已覆盖常规搜索）。

use crate::Host;
use async_trait::async_trait;
use qya_agent::{Concurrency, Tool, ToolCtx, ToolOutput};
use qya_core::ToolSpec;
use serde_json::{Value, json};
use std::time::Duration;

const SEARCH_PROMPT: &str = "# 角色: 搜索助手\n\n以 JSON 数组形式返回搜索结果。每个结果必须包含以下字段：\n- \"title\": 字符串，结果标题\n- \"url\": 字符串，有效的 URL\n- \"description\": 字符串，20-50 字摘要\n\n仅输出有效的 JSON 数组，不包含 markdown，不包含解释。";
const FETCH_PROMPT: &str = "# 角色: 网页内容获取器\n\n获取网页内容并转换为结构化 Markdown：\n- 保留所有标题、段落、列表、表格、代码块\n- 包含元数据头部：源 URL、标题、获取时间戳\n- 不要摘要 - 返回完整内容\n- 使用 UTF-8 编码";

async fn chat(host: &Host, system: &str, user: String) -> Result<String, String> {
    let (c, p) = host.active("grok", "openai_chat")?;
    let base = p.value_or_file(&c, "url")?;
    let base = base.trim_end_matches('/');
    let key = p.secret(&c, "key")?;
    let model = p.value_or_file(&c, "model")?;
    let body = json!({"model": model, "stream": false, "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}]});
    let resp = host
        .http
        .post(format!("{base}/chat/completions"))
        .bearer_auth(key.expose())
        .timeout(Duration::from_secs(p.int("timeout_s", 120) as u64))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("grok 请求失败：{e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("grok 返回 HTTP {status}：{}", text.chars().take(300).collect::<String>()));
    }
    let v: Value = serde_json::from_str(&text).map_err(|_| "grok 返回无法解析".to_owned())?;
    v.pointer("/choices/0/message/content").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| "grok 返回中没有内容".into())
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
            description: "用 grok 联网检索，适合需要多来源交叉、社交平台（如 X）或 web_search 结果不够时；阿颜明确说用 grok 搜时也用它。platform 可指定专注的平台。".into(),
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
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        let Some(q) = args.get("query").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) else { return ToolOutput::err("缺少 query") };
        let platform = args.get("platform").and_then(Value::as_str).map(|p| format!("\n\n专注于平台: {p}")).unwrap_or_default();
        let user = format!("{}\n{q}{platform}\n\n以 JSON 数组形式返回 3-10 个结果。", now_beijing());
        match chat(&self.host, SEARCH_PROMPT, user).await {
            Ok(t) => ToolOutput::ok(crate::grok::clip(&t, 12_000)),
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
            description: "用 grok 获取一个网页的完整内容（转为 Markdown），用于阅读搜索结果中的具体页面。".into(),
            parameters: json!({
                "type": "object",
                "properties": {"url": {"type": "string"}},
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
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        let Some(url) = args.get("url").and_then(Value::as_str).filter(|s| s.starts_with("http")) else { return ToolOutput::err("需要 http(s) 开头的 url") };
        match chat(&self.host, FETCH_PROMPT, format!("{url}\n\n获取并返回结构化 Markdown。")).await {
            Ok(t) => ToolOutput::ok(clip(&t, 30_000)),
            Err(e) => ToolOutput::err(e),
        }
    }
}

pub(crate) fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_owned() } else { format!("{}…（已截断）", s.chars().take(n).collect::<String>()) }
}
