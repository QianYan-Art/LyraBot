//! 与 lyrabot-douyin 服务的 Unix socket 通信：一行 JSON 请求，一行 JSON 响应。
//! 登录类：{"action": "status"|"login", "recover"?}；内容类：{"action":"content","request":{...}}。
//! 只读请求（login 状态、list、job）在连接错误时重试一次；写请求从不重发。

use crate::sanitize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_RESPONSE: usize = 3 * 1024 * 1024;
pub const MAX_CONTENT_REQUEST: usize = 12 * 1024 * 1024;
pub const MAX_CONTENT_RESPONSE: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(50);

pub const LOGIN_STATES: &[&str] = &["logged_in", "login_required", "verification_required", "account_mismatch", "unknown", "page_error", "unavailable"];
pub const CONTENT_STATES: &[&str] = &[
    "ready", "in_progress", "draft_saved", "published", "needs_attention", "editor_ready", "unavailable",
    "account_mismatch", "login_required", "verification_required", "unknown", "page_error",
];
const CONTENT_ROOT_FIELDS: &[&str] = &[
    "ok", "state", "message", "releases", "plan", "job", "metrics", "ui", "covers", "cover_sources", "saved_sources",
    "cover", "recovery", "total", "next_offset", "verification_submitted",
];
const RECOVERY_ACTIONS: &[&str] = &["login", "manual_verification", "check_account", "check_status", "maintenance", "inspect_result", "resume_stage", "prepare"];

#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    #[cfg(unix)]
    async fn exchange(socket: &Path, body: &[u8], limit: usize) -> std::io::Result<Vec<u8>> {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let stream = tokio::net::UnixStream::connect(socket).await?;
        let (r, mut w) = stream.into_split();
        w.write_all(body).await?;
        w.flush().await?;
        let mut reader = BufReader::with_capacity(64 * 1024, r).take(limit as u64 + 1);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        Ok(line)
    }

    #[cfg(not(unix))]
    async fn exchange(_socket: &Path, _body: &[u8], _limit: usize) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "抖音服务只在 Linux 服务器上可用"))
    }

    async fn roundtrip(&self, body: Vec<u8>, limit: usize, retry: bool) -> Result<Value, String> {
        let attempts = if retry { 2 } else { 1 };
        let mut last = String::new();
        for i in 0..attempts {
            match tokio::time::timeout(TIMEOUT, Self::exchange(&self.socket, &body, limit)).await {
                Err(_) => return Err("抖音服务响应超时".into()),
                Ok(Err(e)) => {
                    last = format!("连接抖音服务失败：{e}");
                    if i + 1 < attempts {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
                Ok(Ok(raw)) => {
                    if !raw.ends_with(b"\n") || raw.len() > limit {
                        return Err("抖音服务响应长度无效".into());
                    }
                    return serde_json::from_slice(&raw).map_err(|_| "抖音服务响应不是合法 JSON".into());
                }
            }
        }
        Err(last)
    }

    /// 登录类请求（status / login），校验基本结构。
    pub async fn login(&self, action: &str, recover: bool) -> Result<Value, String> {
        let mut req = json!({"action": action});
        if recover {
            req["recover"] = json!(true);
        }
        let mut body = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
        body.push(b'\n');
        let v = self.roundtrip(body, MAX_RESPONSE, true).await?;
        let ok = v["state"].as_str().is_some_and(|s| LOGIN_STATES.contains(&s)) && v["ok"].is_boolean() && v["message"].is_string();
        if !ok {
            return Err("抖音登录服务响应结构无效".into());
        }
        Ok(v)
    }

    /// 内容类请求：结构校验 + 净化扫描（封面预览的 base64 由调用方先取出再扫描）。
    pub async fn content(&self, request: Value) -> Result<Value, String> {
        let action = request["action"].as_str().unwrap_or_default().to_owned();
        let mut body = serde_json::to_vec(&json!({"action": "content", "request": request})).map_err(|e| e.to_string())?;
        body.push(b'\n');
        if body.len() > MAX_CONTENT_REQUEST {
            return Err("请求超过大小限制".into());
        }
        let retry = matches!(action.as_str(), "list" | "job");
        let v = self.roundtrip(body, MAX_CONTENT_RESPONSE, retry).await?;
        validate_content(&v, &action)?;
        Ok(v)
    }
}

/// 内容响应结构校验（与插件 _validate_content_response 的结构部分一致；封面与净化另行处理）。
pub fn validate_content(v: &Value, action: &str) -> Result<(), String> {
    let m = v.as_object().ok_or("内容响应不是对象")?;
    if m.keys().any(|k| !CONTENT_ROOT_FIELDS.contains(&k.as_str()))
        || !m.get("ok").is_some_and(Value::is_boolean)
        || !m.get("state").and_then(Value::as_str).is_some_and(|s| CONTENT_STATES.contains(&s))
        || !m.get("message").is_some_and(Value::is_string)
    {
        return Err("内容响应结构无效".into());
    }
    if let Some(vs) = m.get("verification_submitted")
        && (action != "verification_submit" || vs != &Value::Bool(true) || m.get("state").and_then(Value::as_str) != Some("in_progress"))
    {
        return Err("验证码提交回执无效".into());
    }
    if let Some(r) = m.get("recovery") {
        let ok = r.as_object().is_some_and(|o| o.len() == 1 && o.get("action").and_then(Value::as_str).is_some_and(|a| RECOVERY_ACTIONS.contains(&a)));
        if !ok {
            return Err("恢复指引无效".into());
        }
    }
    if let Some(r) = m.get("releases")
        && !r.as_array().is_some_and(|a| a.iter().all(Value::is_object))
    {
        return Err("releases 响应无效".into());
    }
    for f in ["plan", "job", "metrics", "ui"] {
        if m.get(f).is_some_and(|x| !x.is_object()) {
            return Err(format!("{f} 响应无效"));
        }
    }
    if m.contains_key("cover") && !matches!(action, "cover_import" | "cover_import_saved" | "cover_reuse" | "cover_pair") {
        return Err("意外的 cover 响应".into());
    }
    if m.contains_key("covers") && !matches!(action, "covers" | "cover_library") {
        return Err("意外的 covers 响应".into());
    }
    if m.contains_key("total") || m.contains_key("next_offset") {
        let total = m.get("total").and_then(Value::as_i64).filter(|t| *t >= 0);
        let next_ok = match m.get("next_offset") {
            None | Some(Value::Null) => true,
            Some(n) => n.as_i64().zip(total).is_some_and(|(n, t)| (0..=t).contains(&n)),
        };
        if action != "cover_library" || total.is_none() || !next_ok {
            return Err("图库分页响应无效".into());
        }
    }
    if m.get("cover_sources").is_some_and(|x| !x.as_array().is_some_and(|a| a.iter().all(Value::is_object))) {
        return Err("cover_sources 响应无效".into());
    }
    let _ = sanitize::is_token; // 净化扫描在 tools 中对去掉预览后的结果执行
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_structure() {
        assert!(validate_content(&json!({"ok": true, "state": "ready", "message": "x", "releases": []}), "list").is_ok());
        assert!(validate_content(&json!({"ok": true, "state": "weird", "message": "x"}), "list").is_err());
        assert!(validate_content(&json!({"ok": true, "state": "ready", "message": "x", "extra": 1}), "list").is_err());
        assert!(validate_content(&json!({"ok": true, "state": "ready", "message": "x", "recovery": {"action": "rm"}}), "job").is_err());
        assert!(validate_content(&json!({"ok": true, "state": "in_progress", "message": "x", "verification_submitted": true}), "job").is_err());
        assert!(validate_content(&json!({"ok": true, "state": "ready", "message": "x", "total": 5, "next_offset": 3}), "cover_library").is_ok());
        assert!(validate_content(&json!({"ok": true, "state": "ready", "message": "x", "total": 5, "next_offset": 9}), "cover_library").is_err());
        // 缺少必需字段时返回错误而不是 panic
        assert!(validate_content(&json!({}), "list").is_err());
        assert!(validate_content(&json!({"state": "ready", "message": "x"}), "list").is_err());
    }
}
