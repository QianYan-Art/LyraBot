//! 多代理编排（docs/05-orchestra.md）。
//!
//! - Worker：一个任务一个会话，做完归档但会话保留，可续接；
//! - Specialist：配置定义的专员（保有专属工具集与长期记忆文件），每次新任务同样创建独立会话、同专员任务串行排队，做完归档可续接、可清理；
//! - 上下文继承：fork（零拷贝继承主对话角色当前分支）/ brief（任务 + 背景说明）/ fresh（空白）；
//! - 结果以 [`WorkerReport`] 先写入信箱再经通道交回运行时，由主对话角色转述；重启后未确认的汇报与被中断的任务由 [`Supervisor::recover`] 与信箱补投。

mod spec;
mod supervisor;
mod tools;
mod types;

pub use supervisor::Supervisor;
pub use tools::{AgentCtlTool, DelegateTool, render_list};
pub use types::{
    ContextMode, DelegateRequest, ExtrasFn, Notify, ProviderFn, Target, WorkerInfo, WorkerKind, WorkerReport, WorkerStatus,
    default_providers,
};
