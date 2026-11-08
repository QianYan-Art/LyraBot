//! 模型提供方抽象与实现。
//!
//! 内核只依赖 [`Provider`]：给出规范化的 [`ChatRequest`]，拿回增量事件流，
//! 流结束时得到组装好的 assistant [`Message`]。各家接口差异封装在传输实现里。

mod chat_completions;
mod error;
pub mod media;

pub use chat_completions::ChatCompletionsProvider;
pub use error::{ErrorKind, LlmError};

use async_trait::async_trait;
use qya_core::{Effort, Message, ToolSpec, Usage};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub type Result<T> = std::result::Result<T, LlmError>;

#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub model: String,
    /// 系统段按顺序发送；越靠前越稳定，便于命中前缀缓存。
    pub system: Vec<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub effort: Option<Effort>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Other(String),
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub message: Message,
    pub usage: Usage,
    pub finish: FinishReason,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    /// 流的最后一个事件
    Completed(Completion),
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RemoteModel {
    pub id: String,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub display_name: Option<String>,
}

pub type EventRx = mpsc::Receiver<Result<StreamEvent>>;

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    /// 发起流式请求。返回前已完成连接与重试；之后的错误经流传递。
    async fn stream(&self, req: ChatRequest, cancel: CancellationToken) -> Result<EventRx>;
    async fn list_models(&self) -> Result<Vec<RemoteModel>>;

    /// 便捷方法：等待完整结果（压缩、后台学习等非交互场景）。
    async fn complete(&self, req: ChatRequest, cancel: CancellationToken) -> Result<Completion> {
        let mut rx = self.stream(req, cancel).await?;
        while let Some(ev) = rx.recv().await {
            if let StreamEvent::Completed(c) = ev? {
                return Ok(c);
            }
        }
        Err(LlmError::new(ErrorKind::Transient, "流提前结束"))
    }
}

/// 按配置构造提供方。
pub fn build(cfg: &qya_config::ProviderConfig) -> Result<Arc<dyn Provider>> {
    match cfg.transport {
        qya_config::Transport::ChatCompletions => Ok(Arc::new(ChatCompletionsProvider::new(cfg.clone())?)),
    }
}

/// 安装进程级 TLS 加密后端（ring）。重复调用无害。
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
