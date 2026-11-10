//! lyrabot 内核：pi 式 agent 循环、工具抽象、会话上下文与压缩。

pub mod context;
mod kernel;
mod tool;

pub use kernel::{AgentError, Kernel, KernelConfig, Queues, Result, TurnOutcome, TurnSpec};
pub use tool::{Concurrency, Tool, ToolCtx, ToolOutput, ToolRegistry, Toolset};
