use crate::{SessionId, Usage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// 模型自然结束且没有工具调用
    Done,
    Cancelled,
    /// 工具循环防护触发
    LoopGuard,
    Error,
}

/// 内核对外发布的事件；平台、编排、日志各自订阅，互不耦合。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    TurnStart { session: SessionId },
    TextDelta { session: SessionId, text: String },
    ReasoningDelta { session: SessionId, text: String },
    ToolStart { session: SessionId, call_id: String, name: String },
    ToolEnd { session: SessionId, call_id: String, name: String, is_error: bool, ms: u64 },
    Compacted { session: SessionId, before_tokens: u64 },
    /// 一次模型请求结束（一轮可能包含多次请求）
    Usage { session: SessionId, usage: Usage },
    TurnEnd { session: SessionId, reason: StopReason, text: String },
}

impl AgentEvent {
    pub fn session(&self) -> SessionId {
        match self {
            Self::TurnStart { session }
            | Self::TextDelta { session, .. }
            | Self::ReasoningDelta { session, .. }
            | Self::ToolStart { session, .. }
            | Self::ToolEnd { session, .. }
            | Self::Compacted { session, .. }
            | Self::Usage { session, .. }
            | Self::TurnEnd { session, .. } => *session,
        }
    }
}
