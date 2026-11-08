//! 多代理编排（docs/01-architecture.md 第 7 节）。
//!
//! - Worker：一个任务一个会话，做完归档但会话保留，可续接；
//! - Specialist：配置定义的常驻专员，长期保有自己的会话，新任务排进其队列；
//! - 上下文继承：fork（零拷贝继承唐语歌当前分支）/ brief（任务 + 背景说明）/ fresh（空白）；
//! - 结果以 [`WorkerReport`] 先写入信箱再经通道交回运行时，由唐语歌转述；重启后未确认的汇报与被中断的任务由 [`Supervisor::recover`] 与信箱补投。

mod supervisor;
mod tools;

pub use supervisor::{
    ContextMode, DelegateRequest, ExtrasFn, Notify, ProviderFn, Supervisor, Target, WorkerInfo, WorkerKind, WorkerReport, WorkerStatus,
    default_providers,
};
pub use tools::{AgentCtlTool, DelegateTool, render_list};
