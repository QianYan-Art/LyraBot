//! qya 共享类型：消息、工具规格、用量、思考等级、内核事件。
//! 本 crate 不依赖任何运行时，所有上层 crate 只通过这里交换数据。

pub mod card;
pub mod effort;
pub mod event;
pub mod media;
pub mod message;
pub mod secret;

pub use effort::Effort;
pub use event::{AgentEvent, StopReason};
pub use message::{ContentPart, Message, Role, ToolCall, ToolSpec, Usage};
pub use secret::Secret;

/// 会话主键（SQLite rowid）。
pub type SessionId = i64;
/// 会话树条目主键（SQLite rowid）。
pub type EntryId = i64;
