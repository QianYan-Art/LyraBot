use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Auth,
    RateLimited,
    /// 套餐额度用尽（如 kimi 5 小时窗口），重试无意义
    QuotaExhausted,
    ContextOverflow,
    BadRequest,
    /// 网络、超时、5xx 等可重试错误
    Transient,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct LlmError {
    pub kind: ErrorKind,
    pub status: Option<u16>,
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl LlmError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, status: None, message: message.into(), retry_after: None }
    }

    pub fn from_status(status: u16, body: &str, retry_after: Option<Duration>) -> Self {
        let lower = body.to_ascii_lowercase();
        let kind = match status {
            401 | 403 => ErrorKind::Auth,
            429 if quota_text(&lower) => ErrorKind::QuotaExhausted,
            429 => ErrorKind::RateLimited,
            400 | 413
                if lower.contains("context") && (lower.contains("length") || lower.contains("exceed"))
                    || lower.contains("too many tokens") =>
            {
                ErrorKind::ContextOverflow
            }
            400 | 404 | 413 | 422 => ErrorKind::BadRequest,
            500..=599 => ErrorKind::Transient,
            _ => ErrorKind::Unknown,
        };
        // 错误正文截断，避免把大段回显写进日志
        let snippet: String = body.chars().take(500).collect();
        Self { kind, status: Some(status), message: snippet, retry_after }
    }

    /// 流式响应中途返回的 `error` 事件：没有状态码，额度用尽仍按正文识别，其余记为未知。
    pub fn from_stream_error(body: &str) -> Self {
        let kind = if quota_text(&body.to_ascii_lowercase()) { ErrorKind::QuotaExhausted } else { ErrorKind::Unknown };
        Self::new(kind, body.chars().take(500).collect::<String>())
    }

    pub fn retryable(&self) -> bool {
        matches!(self.kind, ErrorKind::Transient | ErrorKind::RateLimited)
    }
}

/// 套餐额度用尽的正文特征（参数为已转小写的正文）。
fn quota_text(lower: &str) -> bool {
    lower.contains("exceeded_current_quota") || lower.contains("usage limit") || lower.contains("insufficient_quota")
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(s) => write!(f, "{:?}（HTTP {s}）：{}", self.kind, self.message),
            None => write!(f, "{:?}：{}", self.kind, self.message),
        }
    }
}

impl std::error::Error for LlmError {}

impl From<reqwest::Error> for LlmError {
    fn from(e: reqwest::Error) -> Self {
        // reqwest 错误信息可能包含 URL，但不含请求头，因而不会泄露 key
        Self::new(ErrorKind::Transient, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classify() {
        assert_eq!(LlmError::from_status(401, "", None).kind, ErrorKind::Auth);
        assert_eq!(LlmError::from_status(429, "", None).kind, ErrorKind::RateLimited);
        let quota = LlmError::from_status(429, r#"{"error":{"type":"exceeded_current_quota_error","message":"You've reached your 5-hour usage limit."}}"#, None);
        assert_eq!(quota.kind, ErrorKind::QuotaExhausted);
        assert!(!quota.retryable(), "额度用尽不重试");
        assert_eq!(
            LlmError::from_status(400, "Invalid request: context length exceeded", None).kind,
            ErrorKind::ContextOverflow
        );
        assert_eq!(LlmError::from_status(400, "bad", None).kind, ErrorKind::BadRequest);
        assert_eq!(LlmError::from_status(503, "", None).kind, ErrorKind::Transient);
        assert!(LlmError::from_status(502, "", None).retryable());
        assert!(!LlmError::from_status(400, "", None).retryable());
    }
}
