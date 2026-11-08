//! grok_search / grok_fetch：经 OpenAI 兼容 Chat Completions 调用具备联网能力的 grok 模型（kind = "openai_chat"）。
//! 移植自 Hermes grok-search 技能的提示词；web_map 与 Tavily 兜底未移植（web_search 已覆盖常规搜索）。

use crate::Host;
use crate::shape::Hit;
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
            Ok(t) => ToolOutput::ok(render_search(q, &t)),
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
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        let Some(url) = args.get("url").and_then(Value::as_str).filter(|s| s.starts_with("http")) else { return ToolOutput::err("需要 http(s) 开头的 url") };
        let focus = args.get("focus").and_then(Value::as_str).map(str::trim).filter(|f| !f.is_empty());
        let user = match focus {
            Some(f) => format!("{url}\n\n只提取与「{f}」相关的段落、列表、表格或代码，保留原文措辞与关键数据，去掉导航、广告和页脚；不超过 3000 字。"),
            None => format!("{url}\n\n获取并返回结构化 Markdown，去掉导航、广告和页脚。"),
        };
        match chat(&self.host, FETCH_PROMPT, user).await {
            Ok(t) => ToolOutput::ok(clip(&t, FETCH_LIMIT)),
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
