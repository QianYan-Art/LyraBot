//! 平台层：把各聊天平台统一成 Inbound/Outbound。第一版有 CLI，QQ 官方适配器在任务 13 实现。

pub mod cli;
pub mod qq;

use async_trait::async_trait;
use qya_core::ContentPart;
use std::path::PathBuf;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlatformId {
    Cli,
    Qq,
}

#[derive(Debug, Clone)]
pub struct Inbound {
    pub platform: PlatformId,
    /// 平台侧用户标识（QQ openid 等）
    pub user: String,
    pub parts: Vec<ContentPart>,
    /// 平台侧消息 id，用于被动回复锚定
    pub msg_id: Option<String>,
}

impl Inbound {
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| if let ContentPart::Text { text } = p { Some(text.as_str()) } else { None })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Clone, Default)]
pub struct Outbound {
    pub text: String,
    pub media: Vec<PathBuf>,
    pub reply_to: Option<String>,
}

impl Outbound {
    pub fn text(t: impl Into<String>) -> Self {
        Self { text: t.into(), ..Default::default() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendErrorKind {
    TooLong,
    BadFormat,
    Forbidden,
    NotFound,
    RateLimited,
    Transient,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct SendError {
    pub kind: SendErrorKind,
    pub message: String,
}

pub type SendResult = Result<(), SendError>;

#[derive(Debug, Clone, Copy, Default)]
pub struct PlatformCaps {
    /// 能否逐字流式展示（CLI 可以；QQ 只能整段发送）
    pub streaming: bool,
}

#[async_trait]
pub trait Platform: Send + Sync {
    fn id(&self) -> PlatformId;
    fn caps(&self) -> PlatformCaps;
    /// 运行接收循环，直到连接终止或通道关闭。
    async fn run(&self, inbox: mpsc::Sender<Inbound>) -> anyhow_like::Result<()>;
    async fn send(&self, out: Outbound) -> SendResult;
    /// 流式增量（仅 `caps().streaming` 为真时调用）
    async fn stream_delta(&self, _text: &str) {}
    /// 一轮流式输出结束
    async fn stream_end(&self) {}
}

/// 平台层的轻量错误类型，避免引入 anyhow 依赖。
pub mod anyhow_like {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}
