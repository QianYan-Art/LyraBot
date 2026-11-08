//! web_search：网页搜索。kind = "tencent_wsa"：腾讯元宝搜索 SearchPro（服务 API Key，Bearer 鉴权）。
//! 2026-11-07 实测：请求体 {"Query", "Cnt"(10/20…), "Site"?, "Mode"?}；返回 Response.Pages 为 JSON 字符串数组，
//! 每项含 title/url/site/date/passage/content/score；FromTime 参数会被拒绝。

use crate::Host;
use async_trait::async_trait;
use qya_agent::{Concurrency, Tool, ToolCtx, ToolOutput};
use qya_core::ToolSpec;
use serde_json::{Value, json};
use std::time::Duration;

pub struct WebSearch {
    spec: ToolSpec,
    host: Host,
}

impl WebSearch {
    pub fn new(host: Host) -> Self {
        let spec = ToolSpec {
            name: "web_search".into(),
            description: "联网搜索，返回标题、来源、日期、链接和摘要。查新闻、天气、价格、版本号等时效信息时用；回答时注明来源与日期。site 可限定站点（如 weather.com.cn）。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "site": {"type": "string"},
                    "count": {"type": "integer", "enum": [10, 20]}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        };
        Self { spec, host }
    }

    async fn run(&self, args: &Value) -> Result<String, String> {
        let (c, p) = self.host.active("search", "tencent_wsa")?;
        let query = args.get("query").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).ok_or("缺少 query")?;
        let url = p.value_or_file(&c, "url")?;
        let key = p.secret(&c, "key")?;
        let cnt = args.get("count").and_then(Value::as_i64).unwrap_or(p.int("count", 10));
        let mut body = json!({"Query": query, "Cnt": if cnt >= 20 { 20 } else { 10 }});
        if let Some(s) = args.get("site").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            body["Site"] = json!(s);
        }
        let mode = p.int("mode", 0);
        if mode != 0 {
            body["Mode"] = json!(mode);
        }
        let resp = self
            .host
            .http
            .post(&url)
            .bearer_auth(key.expose())
            .timeout(Duration::from_secs(p.int("timeout_s", 30) as u64))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("搜索请求失败：{e}"))?;
        let v: Value = resp.json().await.map_err(|e| format!("搜索返回无法解析：{e}"))?;
        let r = v.get("Response").ok_or("搜索返回缺少 Response")?;
        if let Some(e) = r.get("Error") {
            return Err(format!("搜索失败：{} {}", e["Code"].as_str().unwrap_or(""), e["Message"].as_str().unwrap_or("")));
        }
        Ok(render(query, r.get("Pages").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default()))
    }
}

/// 把 Pages（JSON 字符串数组）整理成紧凑文本。
pub fn render(query: &str, pages: &[Value]) -> String {
    let items: Vec<Value> = pages.iter().filter_map(|p| p.as_str().and_then(|s| serde_json::from_str(s).ok())).collect();
    if items.is_empty() {
        return format!("“{query}”没有搜到结果，可以换个说法或去掉站点限制再试");
    }
    let mut out = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let s = |k: &str| it.get(k).and_then(Value::as_str).unwrap_or("").trim().to_owned();
        let date = s("date").split(' ').next().unwrap_or("").replace('/', "-");
        let passage: String = s("passage").chars().take(300).collect();
        let meta = [s("site"), date].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · ");
        out.push(format!("{}. {}（{meta}）\n{}\n{passage}", i + 1, s("title"), s("url")));
    }
    out.join("\n\n")
}

#[async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        match self.run(&args).await {
            Ok(t) => ToolOutput::ok(t),
            Err(e) => ToolOutput::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn render_pages() {
        let p = serde_json::json!(["{\"title\":\"北京天气\",\"url\":\"https://w.cn/1\",\"site\":\"天气网\",\"date\":\"2026/08/09 11:44:36\",\"passage\":\"有雨\"}", "not json"]);
        let t = super::render("北京天气", p.as_array().unwrap());
        assert_eq!(t, "1. 北京天气（天气网 · 2026-08-09）\nhttps://w.cn/1\n有雨");
        assert!(super::render("x", &[]).contains("没有搜到"));
    }
}
