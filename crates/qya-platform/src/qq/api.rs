//! QQ 开放平台 HTTP 接口：access_token、网关地址、单聊消息与富媒体上传。

use crate::{SendError, SendErrorKind};
use base64::Engine;
use qya_config::QqConfig;
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub const MSG_TYPE_TEXT: u32 = 0;
pub const MSG_TYPE_MEDIA: u32 = 7;
/// 内联 base64 上传的上限（QQ 约 10 MB，留余量）
pub const INLINE_UPLOAD_MAX: usize = 9 * 1024 * 1024;

pub struct Api {
    cfg: QqConfig,
    http: reqwest::Client,
    token: Mutex<Option<(String, Instant)>>,
}

/// 富媒体类型：1 图片、2 视频、3 语音、4 文件。
pub fn file_type_for(path: &Path) -> u32 {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("png" | "jpg" | "jpeg" | "gif" | "webp") => 1,
        Some("mp4" | "mov") => 2,
        Some("silk" | "slk" | "amr" | "wav" | "mp3") => 3,
        _ => 4,
    }
}

pub fn classify(status: u16, body: &str) -> SendErrorKind {
    let lower = body.to_lowercase();
    match status {
        401 | 403 => SendErrorKind::Forbidden,
        404 => SendErrorKind::NotFound,
        429 => SendErrorKind::RateLimited,
        500..=599 => SendErrorKind::Transient,
        _ if lower.contains("too long") || body.contains("过长") || body.contains("长度") => SendErrorKind::TooLong,
        _ if body.contains("主动消息") || body.contains("无权限") || lower.contains("permission") => SendErrorKind::Forbidden,
        _ if lower.contains("format") || body.contains("格式") => SendErrorKind::BadFormat,
        _ => SendErrorKind::Unknown,
    }
}

fn err(kind: SendErrorKind, message: impl Into<String>) -> SendError {
    SendError { kind, message: message.into() }
}

impl Api {
    pub fn new(cfg: QqConfig) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("构建 HTTP 客户端");
        Self { cfg, http, token: Mutex::new(None) }
    }

    pub fn cfg(&self) -> &QqConfig {
        &self.cfg
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// 取 access_token；到期前 60 秒刷新。
    pub async fn token(&self) -> Result<String, SendError> {
        let mut g = self.token.lock().await;
        if let Some((t, exp)) = g.as_ref()
            && Instant::now() < *exp
        {
            return Ok(t.clone());
        }
        let body = json!({"appId": self.cfg.app_id, "clientSecret": self.cfg.secret.expose()});
        let resp = self.http.post(&self.cfg.token_url).json(&body).send().await.map_err(|e| err(SendErrorKind::Transient, format!("获取 token 失败：{e}")))?;
        let v: Value = resp.json().await.map_err(|e| err(SendErrorKind::Transient, format!("token 响应无法解析：{e}")))?;
        let t = v.get("access_token").and_then(Value::as_str).ok_or_else(|| err(SendErrorKind::Forbidden, "token 响应缺少 access_token（检查 AppID/Secret）"))?.to_owned();
        let expires: u64 = match &v["expires_in"] {
            Value::String(s) => s.parse().unwrap_or(7200),
            Value::Number(n) => n.as_u64().unwrap_or(7200),
            _ => 7200,
        };
        *g = Some((t.clone(), Instant::now() + Duration::from_secs(expires.saturating_sub(60))));
        Ok(t)
    }

    /// 使 token 失效（网关返回 4004 时调用）。
    pub async fn invalidate_token(&self) {
        *self.token.lock().await = None;
    }

    async fn request(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Value, SendError> {
        let token = self.token().await?;
        let mut rb = self.http.request(method, format!("{}{path}", self.cfg.api_base)).header("Authorization", format!("QQBot {token}"));
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let resp = rb.send().await.map_err(|e| err(SendErrorKind::Transient, format!("请求 {path} 失败：{e}")))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if status >= 400 {
            return Err(err(classify(status, &text), format!("HTTP {status} {path}：{}", text.chars().take(300).collect::<String>())));
        }
        let v: Value = if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::Null) };
        // 部分错误以 200 + {code, message} 返回
        if let Some(code) = v.get("code").and_then(Value::as_i64).filter(|c| *c != 0) {
            let msg = v.get("message").and_then(Value::as_str).unwrap_or_default();
            return Err(err(classify(400, msg), format!("QQ 接口错误 {code}：{msg}")));
        }
        Ok(v)
    }

    pub async fn gateway_url(&self) -> Result<String, SendError> {
        let v = self.request(reqwest::Method::GET, "/gateway", None).await?;
        v.get("url").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| err(SendErrorKind::Unknown, "网关地址为空"))
    }

    /// 发送单聊消息。`anchor`：("msg_id" | "event_id", id, msg_seq)；None 为主动消息。
    pub async fn send_c2c(&self, openid: &str, mut body: Value, anchor: Option<(&str, &str, u32)>) -> Result<Value, SendError> {
        match anchor {
            Some((key, id, seq)) => {
                body[key] = json!(id);
                body["msg_seq"] = json!(seq);
            }
            None => body["msg_seq"] = json!(1),
        }
        self.request(reqwest::Method::POST, &format!("/v2/users/{openid}/messages"), Some(&body)).await
    }

    /// 上传本地文件（内联 base64），返回 file_info。
    pub async fn upload(&self, openid: &str, path: &Path, file_type: u32) -> Result<String, SendError> {
        let bytes = tokio::fs::read(path).await.map_err(|e| err(SendErrorKind::NotFound, format!("读取 {} 失败：{e}", path.display())))?;
        if bytes.len() > INLINE_UPLOAD_MAX {
            return Err(err(SendErrorKind::TooLong, format!("文件 {:.1} MB 超过当前支持的 9 MB 上传上限", bytes.len() as f64 / 1048576.0)));
        }
        let mut body = json!({
            "file_type": file_type,
            "srv_send_msg": false,
            "file_data": base64::engine::general_purpose::STANDARD.encode(&bytes),
        });
        if file_type == 4 {
            body["file_name"] = json!(path.file_name().and_then(|n| n.to_str()).unwrap_or("file"));
        }
        let v = self.request(reqwest::Method::POST, &format!("/v2/users/{openid}/files"), Some(&body)).await?;
        v.get("file_info")
            .or_else(|| v.pointer("/data/file_info"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| err(SendErrorKind::Unknown, "上传成功但没有返回 file_info"))
    }

    /// 下载入站附件到目录，返回路径。
    pub async fn download(&self, url: &str, dir: &Path, name: &str) -> Option<std::path::PathBuf> {
        let resp = self.http.get(url).timeout(Duration::from_secs(60)).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let bytes = resp.bytes().await.ok()?;
        tokio::fs::create_dir_all(dir).await.ok()?;
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let safe: String = name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect();
        let path = dir.join(format!("{ts}-{safe}"));
        tokio::fs::write(&path, &bytes).await.ok()?;
        Some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_and_errors() {
        assert_eq!(file_type_for(Path::new("a.PNG")), 1);
        assert_eq!(file_type_for(Path::new("a.wav")), 3);
        assert_eq!(file_type_for(Path::new("a.mp4")), 2);
        assert_eq!(file_type_for(Path::new("a.pdf")), 4);
        assert_eq!(classify(429, ""), SendErrorKind::RateLimited);
        assert_eq!(classify(400, "主动消息发送次数已达上限"), SendErrorKind::Forbidden);
        assert_eq!(classify(400, "content too long"), SendErrorKind::TooLong);
        assert_eq!(classify(502, ""), SendErrorKind::Transient);
    }
}
