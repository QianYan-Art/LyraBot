//! OpenAI Chat Completions 兼容传输（kimi-code 现网使用）。
//!
//! kimi-code 实测要点（2026-11-07，见 docs/03-model-kernel.md 1.2）：
//! - 思考模式恒开，`reasoning_effort` 仅接受 low/high/max；
//! - 带工具调用的 assistant 消息回放时需非空 `reasoning_content`；
//! - 缓存命中在 `usage.prompt_tokens_details.cached_tokens`（也可能出现在 `usage.cached_tokens`）。

use crate::{ChatRequest, Completion, ErrorKind, EventRx, FinishReason, LlmError, Provider, RemoteModel, Result, StreamEvent};
use async_trait::async_trait;
use futures_util::StreamExt;
use qya_config::ProviderConfig;
use qya_core::{ContentPart, Message, Role, ToolCall, Usage};
use serde_json::{Value, json};
use crate::media::{IMAGE_INLINE_MAX, Inline, VIDEO_INLINE_MAX, local_data_uri};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_ATTEMPTS: u32 = 3;

/// SSE 未换行残余的上限（正常单行远小于此）。
const MAX_SSE_LINE: usize = 8 * 1024 * 1024;
/// 单条回复内工具调用索引上限。
const MAX_TOOL_CALLS_PER_MESSAGE: usize = 128;

pub struct ChatCompletionsProvider {
    cfg: ProviderConfig,
    http: reqwest::Client,
}

impl ChatCompletionsProvider {
    pub fn new(cfg: ProviderConfig) -> Result<Self> {
        crate::install_crypto();
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(cfg.timeouts.connect_s))
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60));
        b = b.user_agent(cfg.user_agent.clone().unwrap_or_else(|| concat!("qy-assistant/", env!("CARGO_PKG_VERSION")).to_owned()));
        let http = b.build().map_err(|e| LlmError::new(ErrorKind::Unknown, e.to_string()))?;
        Ok(Self { cfg, http })
    }

    fn encode(&self, req: &ChatRequest) -> Value {
        let mut messages: Vec<Value> = req.system.iter().map(|s| json!({"role": "system", "content": s})).collect();
        // 按目标模型的模态能力编码媒体；未登记的模型按全支持处理，由接口自行报错
        let caps = self.cfg.models.get(&req.model).map_or(Caps::ALL, |m| Caps { image: m.vision, video: m.video });
        messages.extend(req.messages.iter().map(|m| encode_message(m, caps)));
        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !req.tools.is_empty() {
            body["tools"] = req
                .tools
                .iter()
                .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
                .collect();
        }
        if let Some(e) = req.effort {
            body["reasoning_effort"] = Value::String(self.cfg.effort_param(e));
        }
        if let Some(m) = req.max_tokens {
            body["max_tokens"] = json!(m);
        }
        if let Some(k) = req.cache_key.as_deref().filter(|k| !k.is_empty()) {
            body["prompt_cache_key"] = Value::String(k.to_owned());
        }
        body
    }

    async fn send_with_retry(&self, body: &Value, cancel: &CancellationToken) -> Result<reqwest::Response> {
        let url = format!("{}/chat/completions", self.cfg.base_url);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let fut = self.http.post(&url).bearer_auth(self.cfg.api_key.expose()).json(body).send();
            let res = tokio::select! {
                _ = cancel.cancelled() => return Err(LlmError::new(ErrorKind::Cancelled, "已取消")),
                r = tokio::time::timeout(Duration::from_secs(self.cfg.timeouts.first_byte_s), fut) => r,
            };
            let err = match res {
                Err(_) => LlmError::new(ErrorKind::Transient, "等待响应超时"),
                Ok(Err(e)) => LlmError::from(e),
                Ok(Ok(resp)) if resp.status().is_success() => return Ok(resp),
                Ok(Ok(resp)) => {
                    let status = resp.status().as_u16();
                    let retry_after = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    // 错误正文也要有期限并响应取消：上游可能只发响应头、正文迟迟不结束
                    let text = tokio::select! {
                        _ = cancel.cancelled() => return Err(LlmError::new(ErrorKind::Cancelled, "已取消")),
                        r = tokio::time::timeout(Duration::from_secs(self.cfg.timeouts.first_byte_s), resp.text()) => r.ok().and_then(|t| t.ok()).unwrap_or_default(),
                    };
                    LlmError::from_status(status, &text, retry_after)
                }
            };
            if !err.retryable() || attempt >= MAX_ATTEMPTS {
                return Err(err);
            }
            let wait = err.retry_after.unwrap_or(Duration::from_secs(1 << (attempt - 1))).min(Duration::from_secs(30));
            tracing::warn!(provider = %self.cfg.id, attempt, ?wait, error = %err, "模型请求失败，退避重试");
            tokio::select! {
                _ = cancel.cancelled() => return Err(LlmError::new(ErrorKind::Cancelled, "已取消")),
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }
}

#[async_trait]
impl Provider for ChatCompletionsProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    async fn stream(&self, req: ChatRequest, cancel: CancellationToken) -> Result<EventRx> {
        let body = self.encode(&req);
        let resp = self.send_with_retry(&body, &cancel).await?;
        let (tx, rx) = mpsc::channel(64);
        let idle = Duration::from_secs(self.cfg.timeouts.idle_s);
        tokio::spawn(async move {
            let mut bytes = resp.bytes_stream();
            let mut acc = Accumulator::default();
            let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
            loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => { let _ = tx.send(Err(LlmError::new(ErrorKind::Cancelled, "已取消"))).await; return; }
                    n = tokio::time::timeout(idle, bytes.next()) => n,
                };
                let chunk = match next {
                    Err(_) => {
                        let _ = tx.send(Err(LlmError::new(ErrorKind::Transient, "流空闲超时"))).await;
                        return;
                    }
                    Ok(None) => break,
                    Ok(Some(Err(e))) => {
                        let _ = tx.send(Err(e.into())).await;
                        return;
                    }
                    Ok(Some(Ok(c))) => c,
                };
                buf.extend_from_slice(&chunk);
                if buf.len() > MAX_SSE_LINE {
                    let _ = tx.send(Err(LlmError::new(ErrorKind::Transient, "流式响应单行过长，已中止"))).await;
                    return;
                }
                // 按行切分 SSE；只处理完整行，残余留到下一个分块
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line);
                    for ev in acc.feed_line(line.trim_end()) {
                        if tx.send(Ok(ev)).await.is_err() {
                            return; // 接收端已丢弃
                        }
                    }
                    if acc.done {
                        break;
                    }
                }
                if acc.done {
                    break;
                }
            }
            let _ = tx.send(acc.finish()).await;
        });
        Ok(rx)
    }

    async fn list_models(&self) -> Result<Vec<RemoteModel>> {
        #[derive(serde::Deserialize)]
        struct List {
            data: Vec<RemoteModel>,
        }
        let url = format!("{}/models", self.cfg.base_url);
        let resp = self.http.get(url).bearer_auth(self.cfg.api_key.expose()).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        if !(200..300).contains(&status) {
            return Err(LlmError::from_status(status, &text, None));
        }
        serde_json::from_str::<List>(&text)
            .map(|l| l.data)
            .map_err(|e| LlmError::new(ErrorKind::Unknown, format!("解析模型列表失败：{e}")))
    }
}

/// 目标模型能直接接收的媒体类型。
#[derive(Debug, Clone, Copy)]
struct Caps {
    image: bool,
    video: bool,
}

impl Caps {
    const ALL: Self = Self { image: true, video: true };
}

fn encode_message(m: &Message, caps: Caps) -> Value {
    match m.role {
        Role::User => json!({"role": "user", "content": encode_content(&m.content, caps)}),
        Role::Tool => json!({
            "role": "tool",
            "tool_call_id": m.tool_call_id.as_deref().unwrap_or_default(),
            "content": m.text(),
        }),
        Role::Assistant => {
            let text = m.text();
            let mut v = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { Value::String(text) }});
            match &m.reasoning {
                Some(r) if !r.is_empty() => v["reasoning_content"] = Value::String(r.clone()),
                // 思考模式下带工具调用的 assistant 消息必须携带非空推理字段；用空格补齐，不伪造内容
                _ if !m.tool_calls.is_empty() => v["reasoning_content"] = Value::String(" ".into()),
                _ => {}
            }
            if !m.tool_calls.is_empty() {
                v["tool_calls"] = m
                    .tool_calls
                    .iter()
                    .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}}))
                    .collect();
            }
            v
        }
    }
}

fn encode_content(parts: &[ContentPart], caps: Caps) -> Value {
    if let [ContentPart::Text { text }] = parts {
        return Value::String(text.clone());
    }
    let note = |t: String| json!({"type": "text", "text": t});
    parts
        .iter()
        .map(|p| match p {
            ContentPart::Text { text } => json!({"type": "text", "text": text}),
            ContentPart::Image { .. } if !caps.image => note("〔这里有一张图片，当前模型看不了图片〕".into()),
            ContentPart::Image { url } => match url.strip_prefix("file://") {
                Some(path) => match local_data_uri(path, IMAGE_INLINE_MAX) {
                    Ok(data) => json!({"type": "image_url", "image_url": {"url": data}}),
                    Err(_) => note("〔这张图片的本地缓存已清理，看不到内容了〕".into()),
                },
                None => json!({"type": "image_url", "image_url": {"url": url}}),
            },
            // 当前模型不支持视频：不发送画面，提示用 watch_video 工具（由支持视频的模型代看）
            ContentPart::Video { url } if !caps.video => {
                note(format!("〔这段视频没有直接发给你（当前模型不支持视频）；需要了解内容时用 watch_video 查看：{}〕", url.trim_start_matches("file://")))
            }
            ContentPart::Video { url } => match url.strip_prefix("file://") {
                Some(path) => match local_data_uri(path, VIDEO_INLINE_MAX) {
                    Ok(data) => json!({"type": "video_url", "video_url": {"url": data}}),
                    Err(Inline::TooLarge) => note(format!("〔这段视频较大，没有直接发送；需要了解内容时用 watch_video 查看：{path}〕")),
                    Err(Inline::Missing) => note("〔这段视频的本地缓存已清理，看不到内容了〕".into()),
                },
                None => json!({"type": "video_url", "video_url": {"url": url}}),
            },
        })
        .collect()
}

/// SSE 增量累加器：负责把 delta 拼成完整消息。独立成结构便于离线测试。
#[derive(Default)]
struct Accumulator {
    text: String,
    reasoning: String,
    calls: Vec<ToolCall>,
    usage: Usage,
    finish: Option<FinishReason>,
    done: bool,
    error: Option<LlmError>,
}

impl Accumulator {
    fn feed_line(&mut self, line: &str) -> Vec<StreamEvent> {
        let Some(data) = line.strip_prefix("data:") else { return Vec::new() };
        let data = data.trim_start();
        if data == "[DONE]" {
            self.done = true;
            return Vec::new();
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            tracing::debug!(line = %data.chars().take(200).collect::<String>(), "忽略无法解析的 SSE 行");
            return Vec::new();
        };
        if let Some(err) = v.get("error") {
            self.error = Some(LlmError::new(ErrorKind::Unknown, err.to_string()));
            self.done = true;
            return Vec::new();
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage = parse_usage(u);
        }
        let mut out = Vec::new();
        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else { return out };
        if let Some(d) = choice.get("delta") {
            if let Some(t) = d.get("reasoning_content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                self.reasoning.push_str(t);
                out.push(StreamEvent::ReasoningDelta(t.to_owned()));
            }
            if let Some(t) = d.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                self.text.push_str(t);
                out.push(StreamEvent::TextDelta(t.to_owned()));
            }
            if let Some(calls) = d.get("tool_calls").and_then(Value::as_array) {
                for c in calls {
                    let idx = c.get("index").and_then(Value::as_u64).unwrap_or(self.calls.len() as u64) as usize;
                    if idx >= MAX_TOOL_CALLS_PER_MESSAGE {
                        continue; // 异常索引直接忽略，避免按索引无界扩容
                    }
                    while self.calls.len() <= idx {
                        self.calls.push(ToolCall { id: String::new(), name: String::new(), arguments: String::new() });
                    }
                    let slot = &mut self.calls[idx];
                    if let Some(id) = c.get("id").and_then(Value::as_str) {
                        slot.id.push_str(id);
                    }
                    if let Some(f) = c.get("function") {
                        if let Some(n) = f.get("name").and_then(Value::as_str) {
                            slot.name.push_str(n);
                        }
                        if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                            slot.arguments.push_str(a);
                        }
                    }
                }
            }
        }
        if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(match f {
                "stop" => FinishReason::Stop,
                "tool_calls" => FinishReason::ToolCalls,
                "length" => FinishReason::Length,
                o => FinishReason::Other(o.to_owned()),
            });
        }
        out
    }

    fn finish(mut self) -> Result<StreamEvent> {
        if let Some(e) = self.error.take() {
            return Err(e);
        }
        if self.finish.is_none() && !self.done {
            return Err(LlmError::new(ErrorKind::Transient, "流在结束标记前中断"));
        }
        // 缺失 id 的调用补本地 id，保证 tool 结果能对应上
        for (i, c) in self.calls.iter_mut().enumerate() {
            if c.id.is_empty() {
                c.id = format!("call_local_{i}");
            }
        }
        self.calls.retain(|c| !c.name.is_empty());
        let mut message = Message::with_parts(
            Role::Assistant,
            if self.text.is_empty() { Vec::new() } else { vec![ContentPart::text(self.text)] },
        );
        message.reasoning = (!self.reasoning.is_empty()).then_some(self.reasoning);
        let finish = match (&self.finish, self.calls.is_empty()) {
            (_, false) => FinishReason::ToolCalls,
            (Some(f), true) => f.clone(),
            (None, true) => FinishReason::Stop,
        };
        message.tool_calls = self.calls;
        Ok(StreamEvent::Completed(Completion { message, usage: self.usage, finish }))
    }
}

fn parse_usage(u: &Value) -> Usage {
    let n = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
    let cached = n(u.pointer("/prompt_tokens_details/cached_tokens")).max(n(u.get("cached_tokens")));
    Usage {
        prompt_tokens: n(u.get("prompt_tokens")),
        completion_tokens: n(u.get("completion_tokens")),
        cached_tokens: cached,
        reasoning_tokens: n(u.pointer("/completion_tokens_details/reasoning_tokens")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_text_reasoning_tools_usage() {
        let mut a = Accumulator::default();
        let lines = [
            r#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"想"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"你"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"好"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"sh","arguments":"{\"cmd\":"}}]}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"data: {"choices":[],"usage":{"prompt_tokens":96,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":64},"completion_tokens_details":{"reasoning_tokens":3}}}"#,
            ": keep-alive",
            "data: [DONE]",
        ];
        let mut deltas = 0;
        for l in lines {
            deltas += a.feed_line(l).len();
        }
        assert_eq!(deltas, 3);
        let StreamEvent::Completed(c) = a.finish().unwrap() else { panic!() };
        assert_eq!(c.message.text(), "你好");
        assert_eq!(c.message.reasoning.as_deref(), Some("想"));
        assert_eq!(c.message.tool_calls, vec![ToolCall { id: "c1".into(), name: "sh".into(), arguments: r#"{"cmd":"ls"}"#.into() }]);
        assert_eq!(c.finish, FinishReason::ToolCalls);
        assert_eq!(c.usage, Usage { prompt_tokens: 96, completion_tokens: 10, cached_tokens: 64, reasoning_tokens: 3 });
    }

    #[test]
    fn truncated_stream_is_error() {
        let mut a = Accumulator::default();
        a.feed_line(r#"data: {"choices":[{"index":0,"delta":{"content":"半"}}]}"#);
        assert!(a.finish().is_err());
    }

    #[test]
    fn inline_error_surfaces() {
        let mut a = Accumulator::default();
        a.feed_line(r#"data: {"error":{"message":"quota"}}"#);
        assert!(a.finish().is_err());
    }

    #[test]
    fn assistant_tool_call_gets_reasoning_pad() {
        let mut m = Message::with_parts(Role::Assistant, vec![]);
        m.tool_calls.push(ToolCall { id: "c".into(), name: "x".into(), arguments: "{}".into() });
        let v = encode_message(&m, Caps::ALL);
        assert_eq!(v["reasoning_content"], " ");
        assert_eq!(v["content"], Value::Null);
        let plain = encode_message(&Message::assistant("hi"), Caps::ALL);
        assert!(plain.get("reasoning_content").is_none());
    }

    #[test]
    fn local_images_become_data_uri() {
        let p = std::env::temp_dir().join(format!("qya-llm-img-{}.png", std::process::id()));
        std::fs::write(&p, b"\x89PNG\r\n\x1a\nrest").unwrap();
        let m = Message::with_parts(Role::User, vec![ContentPart::Image { url: format!("file://{}", p.display()) }, ContentPart::Image { url: "file:///nope/x.jpg".into() }]);
        let v = encode_message(&m, Caps::ALL);
        assert!(v["content"][0]["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,"));
        assert_eq!(v["content"][1]["type"], "text");
    }

    #[test]
    fn user_multimodal_content() {
        let m = Message::with_parts(Role::User, vec![ContentPart::text("看"), ContentPart::Image { url: "data:image/png;base64,AA".into() }]);
        let v = encode_message(&m, Caps::ALL);
        assert_eq!(v["content"][1]["type"], "image_url");
        assert_eq!(encode_message(&Message::user("hi"), Caps::ALL)["content"], "hi");
    }

    #[test]
    fn media_follows_model_capabilities() {
        let m = Message::with_parts(
            Role::User,
            vec![ContentPart::text("看看"), ContentPart::Video { url: "file:///m/inbox/a.mp4".into() }],
        );
        // 不支持视频：不发画面，提示用 watch_video 并给出路径
        let v = encode_message(&m, Caps { image: true, video: false });
        let note = v["content"][1]["text"].as_str().unwrap();
        assert!(note.contains("watch_video") && note.contains("/m/inbox/a.mp4"), "{note}");
        // 支持视频但本地文件已清理：如实说明
        let v = encode_message(&m, Caps::ALL);
        assert!(v["content"][1]["text"].as_str().unwrap().contains("已清理"));
        // 远程地址原样发送
        let remote = Message::with_parts(Role::User, vec![ContentPart::text("x"), ContentPart::Video { url: "https://v/1.mp4".into() }]);
        assert_eq!(encode_message(&remote, Caps::ALL)["content"][1]["video_url"]["url"], "https://v/1.mp4");
    }
}
