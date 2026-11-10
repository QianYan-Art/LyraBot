//! 编排层共享类型、回调签名与执行者内部句柄。

use lyrabot_agent::{Queues, Tool};
use lyrabot_config::Config;
use lyrabot_core::{Effort, SessionId};
use lyrabot_store::Mail;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// 按提供方 id 取得模型客户端。运行时传入真实实现，测试传入假实现。
pub type ProviderFn = Arc<dyn Fn(&Config, &str) -> Result<Arc<dyn lyrabot_llm::Provider>, String> + Send + Sync>;

/// 按执行者身份追加的专属工具（如专员只能写自己的记忆文件）。
pub type ExtrasFn = Arc<dyn Fn(&Config, &WorkerKind) -> Vec<Arc<dyn Tool>> + Send + Sync>;

/// 默认实现：按配置构造。
pub fn default_providers() -> ProviderFn {
    Arc::new(|c: &Config, id: &str| {
        let p = c.provider(id).ok_or_else(|| format!("提供方 {id} 未配置"))?;
        lyrabot_llm::build(p).map_err(|e| e.to_string())
    })
}

/// 需要用户决定的问题上限（字）。
const QUESTION_LIMIT: usize = 300;

pub(crate) fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMode {
    Fork,
    Brief,
    Fresh,
}

impl ContextMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fork" => Some(Self::Fork),
            "brief" => Some(Self::Brief),
            "fresh" => Some(Self::Fresh),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Worker,
    Specialist(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Notify {
    /// 完成后立即交给主对话角色转述
    Relay,
    /// 只入信箱，等用户下次说话时一并带上
    Silent,
}

#[derive(Debug, Clone)]
pub struct DelegateRequest {
    /// 发起方（主对话角色）会话
    pub parent: SessionId,
    /// 发起方那次 delegate 调用的 id（fork 模式补结果用）
    pub parent_call_id: String,
    pub task: String,
    pub context: ContextMode,
    pub context_note: Option<String>,
    pub target: Target,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub notify: Notify,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerKind {
    Worker,
    Specialist(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerStatus {
    Queued,
    Running,
    Done,
    /// 本轮结束，等用户对汇报里的问题拍板
    Waiting,
    Failed,
    Cancelled,
    /// 服务重启时仍在排队或运行
    Interrupted,
}

impl WorkerStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "排队中",
            Self::Running => "进行中",
            Self::Done => "已完成",
            Self::Waiting => "等待决定",
            Self::Failed => "失败",
            Self::Cancelled => "已停止",
            Self::Interrupted => "被中断",
        }
    }
    pub(crate) fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

/// 交回主对话角色的结果。先写入信箱（`mail` 为信件 id）再投递，确认送达前进程重启也不会丢。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerReport {
    #[serde(skip)]
    pub mail: i64,
    pub id: SessionId,
    pub title: String,
    pub kind: WorkerKind,
    pub status: WorkerStatus,
    pub summary: String,
    /// 需要用户拍板的问题（执行者经 need_decision 提出）
    #[serde(default)]
    pub question: Option<String>,
    pub media: Vec<PathBuf>,
    pub notify: Notify,
    pub elapsed_s: u64,
}

impl WorkerReport {
    /// 从信箱记录还原（启动时补投未确认的汇报）。
    pub fn from_mail(m: &Mail) -> Option<Self> {
        let mut r: Self = serde_json::from_value(m.payload.clone()).ok()?;
        r.mail = m.id;
        Some(r)
    }
}

/// /agent 与 agent_ctl 展示用的快照。
#[derive(Debug, Clone)]
pub struct WorkerInfo {
    pub id: SessionId,
    pub kind: WorkerKind,
    pub title: String,
    pub model: String,
    pub effort: String,
    pub status: WorkerStatus,
    pub progress: Option<String>,
    pub elapsed: Option<Duration>,
}

pub(crate) struct HState {
    pub status: WorkerStatus,
    pub progress: Option<String>,
    pub question: Option<String>,
    pub cancel: Option<CancellationToken>,
    pub started: Option<Instant>,
    pub finished: Option<Duration>,
    pub notify: Notify,
}

pub(crate) struct Handle {
    pub id: SessionId,
    pub kind: WorkerKind,
    pub title: String,
    pub queues: Arc<Queues>,
    state: Mutex<HState>,
}

impl Handle {
    pub(crate) fn new(id: SessionId, kind: WorkerKind, title: String, status: WorkerStatus) -> Arc<Self> {
        Arc::new(Self {
            id,
            kind,
            title,
            queues: Arc::new(Queues::default()),
            state: Mutex::new(HState {
                status,
                progress: None,
                question: None,
                cancel: None,
                started: None,
                finished: None,
                notify: Notify::Relay,
            }),
        })
    }
    pub(crate) fn st(&self) -> std::sync::MutexGuard<'_, HState> {
        self.state.lock().expect("worker 状态锁中毒")
    }
    pub(crate) fn set_progress(&self, p: String) {
        self.st().progress = Some(p);
    }
    pub(crate) fn set_question(&self, q: &str) {
        self.st().question = Some(clip(q, QUESTION_LIMIT));
    }
}
