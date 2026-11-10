use lyrabot_agent::{Kernel, Queues, Tool, ToolRegistry, TurnSpec};
use lyrabot_config::{Config, ConfigHandle};
use lyrabot_core::{Effort, Message, SessionId, StopReason};
use lyrabot_store::{Mail, NewSession, SessionKind, SessionStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

/// worker 汇报摘要的上限（字）；完整过程留在 worker 会话里。
const SUMMARY_LIMIT: usize = 1200;
/// 需要用户决定的问题上限（字）。
const QUESTION_LIMIT: usize = 300;
const INTERRUPTED_SUMMARY: &str = "服务重启时这个任务还没做完，已有进展保存在它的会话里。需要的话用 agent_ctl continue 续接；续接时它会先核对已经做过的部分，不重复外部操作。";
/// fork 模式下为主对话角色那次 delegate 调用补的工具结果。
const FORK_PLACEHOLDER: &str = "（这件事已交给后台执行者处理）";
const FORK_INSTRUCTION: &str = "〔以上是主对话角色与用户的对话，供你了解来龙去脉。从现在起你是后台执行者，不扮演角色，不对用户直接说话，只完成下面的任务并按系统提示写汇报。〕";

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
    fn is_active(self) -> bool {
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

struct HState {
    status: WorkerStatus,
    progress: Option<String>,
    question: Option<String>,
    cancel: Option<CancellationToken>,
    started: Option<Instant>,
    finished: Option<Duration>,
    notify: Notify,
}

pub(crate) struct Handle {
    pub id: SessionId,
    pub kind: WorkerKind,
    pub title: String,
    queues: Arc<Queues>,
    state: Mutex<HState>,
}

impl Handle {
    fn new(id: SessionId, kind: WorkerKind, title: String, status: WorkerStatus) -> Arc<Self> {
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
    fn st(&self) -> std::sync::MutexGuard<'_, HState> {
        self.state.lock().expect("worker 状态锁中毒")
    }
    pub(crate) fn set_progress(&self, p: String) {
        self.st().progress = Some(p);
    }
    pub(crate) fn set_question(&self, q: &str) {
        self.st().question = Some(clip(q, QUESTION_LIMIT));
    }
}

struct Inner {
    cfg: Arc<ConfigHandle>,
    kernel: Kernel,
    tools: ToolRegistry,
    sem: Arc<Semaphore>,
    /// 给用户续接已有任务预留的额外名额：并发占满时，人的追问不必排在新任务后面
    spare: Arc<Semaphore>,
    /// 同一专员的任务串行互斥锁：每次新任务用独立会话，但同一专员不并发执行
    specialist_locks: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
    workers: Mutex<BTreeMap<SessionId, Arc<Handle>>>,
    reports: mpsc::UnboundedSender<WorkerReport>,
    providers: ProviderFn,
    extras: Option<ExtrasFn>,
}

#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Inner>,
}

fn tokens_of(m: &Message) -> i64 {
    lyrabot_agent::context::estimate_tokens(m.char_len()) as i64
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

impl Supervisor {
    pub fn new(
        cfg: Arc<ConfigHandle>,
        kernel: Kernel,
        tools: ToolRegistry,
        reports: mpsc::UnboundedSender<WorkerReport>,
        providers: ProviderFn,
    ) -> Self {
        let n = cfg.get().orchestra.max_concurrent;
        Self {
            inner: Arc::new(Inner {
                cfg,
                kernel,
                tools,
                sem: Arc::new(Semaphore::new(n)),
                spare: Arc::new(Semaphore::new(1)),
                specialist_locks: Mutex::new(BTreeMap::new()),
                workers: Mutex::new(BTreeMap::new()),
                reports,
                providers,
                extras: None,
            }),
        }
    }

    /// 设置按身份追加工具的钩子（构造后、派出任务前调用）。
    pub fn with_extras(mut self, extras: ExtrasFn) -> Self {
        Arc::get_mut(&mut self.inner).expect("with_extras 必须在共享之前调用").extras = Some(extras);
        self
    }

    fn workers(&self) -> std::sync::MutexGuard<'_, BTreeMap<SessionId, Arc<Handle>>> {
        self.inner.workers.lock().expect("workers 锁中毒")
    }

    fn specialist_lock(inner: &Inner, name: &str) -> Arc<tokio::sync::Mutex<()>> {
        inner
            .specialist_locks
            .lock()
            .expect("specialist_locks 锁中毒")
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn is_specialist_busy(&self, name: &str) -> bool {
        self.workers()
            .values()
            .any(|h| matches!(&h.kind, WorkerKind::Specialist(n) if n == name) && h.st().status.is_active())
    }

    /// 校验并解析模型与思考等级。
    fn resolve_model(c: &Config, target: &Target, model: Option<String>, effort: Option<Effort>) -> Result<(String, Effort), String> {
        let o = &c.orchestra;
        let spec = match target {
            Target::Specialist(n) => Some(o.specialists.get(n).ok_or_else(|| format!("没有名为 {n} 的专员"))?),
            Target::Worker => None,
        };
        let model = model
            .or_else(|| spec.and_then(|s| s.model.clone()))
            .unwrap_or_else(|| o.default_model.clone());
        let effort = effort.or_else(|| spec.and_then(|s| s.effort)).unwrap_or(o.default_effort);
        let p = c.provider(&o.provider).ok_or("编排提供方未配置")?;
        if !p.models.contains_key(&model) {
            let list: Vec<_> = p.models.keys().cloned().collect();
            return Err(format!("模型 {model} 不可用，可选：{}", list.join(" ")));
        }
        Ok((model, effort))
    }

    /// 派出任务，立即返回（不等待执行）。成功时返回给主对话角色的确认文本。
    pub async fn delegate(&self, req: DelegateRequest) -> Result<String, String> {
        let c = self.inner.cfg.get();
        let (model, effort) = Self::resolve_model(&c, &req.target, req.model.clone(), req.effort)?;
        let store = self.inner.kernel.store();
        let err = |e: lyrabot_store::StoreError| format!("存储错误：{e}");
        let task = req.task.trim();
        if task.is_empty() {
            return Err("任务内容不能为空".into());
        }
        let note = req.context_note.as_deref().map(str::trim).filter(|n| !n.is_empty());
        let mut downgraded = false;

        let mut busy_specialist = false;

        let (handle, input, cancel) = match &req.target {
            Target::Worker => {
                let fork_entry = if req.context == ContextMode::Fork {
                    store.session(req.parent).await.map_err(err)?.leaf_id
                } else {
                    None
                };
                let id = store
                    .create_session(NewSession {
                        kind: Some(SessionKind::Worker),
                        name: Some(clip(task, 30)),
                        parent_session: Some(req.parent),
                        fork_entry,
                        model: model.clone(),
                        effort: effort.to_string(),
                    })
                    .await
                    .map_err(err)?;
                let mut input = Vec::new();
                let body = match (req.context, note) {
                    (ContextMode::Fork, _) => {
                        // 分叉点是主对话角色那条带 delegate 调用的消息，先补上结果保持消息结构合法
                        input.push(Message::tool_result(&req.parent_call_id, FORK_PLACEHOLDER, false));
                        format!("{FORK_INSTRUCTION}\n\n任务：{task}{}", note.map(|n| format!("\n补充：{n}")).unwrap_or_default())
                    }
                    (ContextMode::Brief, Some(n)) => format!("任务：{task}\n\n背景：{n}"),
                    _ => format!("任务：{task}"),
                };
                input.push(Message::user(body));
                let h = Handle::new(id, WorkerKind::Worker, clip(task, 30), WorkerStatus::Done);
                let cancel = Self::claim(&mut h.st(), req.notify);
                self.workers().insert(id, h.clone());
                (h, input, cancel)
            }
            Target::Specialist(name) => {
                if req.context == ContextMode::Fork {
                    downgraded = true;
                }
                busy_specialist = self.is_specialist_busy(name);
                let id = store
                    .create_session(NewSession {
                        kind: Some(SessionKind::Specialist),
                        name: Some(name.clone()),
                        parent_session: Some(req.parent),
                        fork_entry: None,
                        model: model.clone(),
                        effort: effort.to_string(),
                    })
                    .await
                    .map_err(err)?;
                let body = match note {
                    Some(n) => format!("任务：{task}\n\n背景：{n}"),
                    None => format!("任务：{task}"),
                };
                let h = Handle::new(id, WorkerKind::Specialist(name.clone()), clip(task, 30), WorkerStatus::Done);
                let cancel = Self::claim(&mut h.st(), req.notify);
                self.workers().insert(id, h.clone());
                (h, vec![Message::user(body)], cancel)
            }
        };
        let id = handle.id;
        self.launch(handle, input, req.parent, cancel, false).await?;
        let who = match &req.target {
            Target::Worker => format!("后台 #{id}"),
            Target::Specialist(n) => format!("专员 {n}（#{id}）"),
        };
        let mut msg = format!("已交给{who}（模型 {model}，思考 {effort}）。结果出来后会以〔后台结果〕交回给你。");
        if busy_specialist
            && let Target::Specialist(n) = &req.target
        {
            msg.push_str(&format!("专员 {n} 正在忙，新任务已排在它手头的活之后（#{id}）。"));
        }
        if downgraded {
            msg.push_str("专员使用独立会话，fork 不适用，已按 brief 处理。");
        }
        Ok(msg)
    }

    /// 续接或插话：运行中则作为插话注入，已结束则在原会话上再跑一轮（可使用预留名额）。
    /// `owner` 为这次续接的汇报收件会话。
    pub async fn continue_worker(&self, id: SessionId, text: &str, owner: SessionId) -> Result<String, String> {
        let existing = self.workers().get(&id).cloned();
        let handle = match existing {
            Some(h) => h,
            None => {
                let row = self.inner.kernel.store().session(id).await.map_err(|_| format!("没有 #{id} 这个后台任务"))?;
                let kind = match row.kind {
                    SessionKind::Worker => WorkerKind::Worker,
                    SessionKind::Specialist => WorkerKind::Specialist(row.name.clone().unwrap_or_default()),
                    SessionKind::Persona => return Err(format!("#{id} 不是后台任务")),
                };
                let h = Handle::new(id, kind, row.name.unwrap_or_default(), WorkerStatus::Done);
                self.workers().entry(id).or_insert(h).clone()
            }
        };
        let msg = Message::user(text.trim().to_owned());
        let Some(cancel) = self.enqueue_or_claim(&handle, &msg, true, Notify::Relay, owner).await else {
            return Ok(format!("已把这句话插给 #{id}"));
        };
        self.launch(handle, vec![msg], owner, cancel, true).await?;
        Ok(format!("已续接 #{id}"))
    }

    pub fn stop(&self, id: SessionId) -> Result<String, String> {
        let h = self.workers().get(&id).cloned().ok_or_else(|| format!("#{id} 不在运行"))?;
        let mut st = h.st();
        match st.cancel.take() {
            Some(c) if st.status.is_active() => {
                c.cancel();
                Ok(format!("已让 #{id} 停下"))
            }
            _ => Err(format!("#{id} 当前没有在运行")),
        }
    }

    /// 会话被清理后移除对应句柄。
    pub fn forget(&self, ids: &[SessionId]) {
        let mut w = self.workers();
        for id in ids {
            w.remove(id);
        }
    }

    /// 排队或运行中的执行者会话。
    pub fn active_ids(&self) -> Vec<SessionId> {
        self.workers().iter().filter(|(_, h)| h.st().status.is_active()).map(|(id, _)| *id).collect()
    }

    pub fn running(&self) -> usize {
        self.workers().values().filter(|h| h.st().status.is_active()).count()
    }

    /// 当前进程内的执行者快照（最近的在前）。
    pub async fn list(&self) -> Vec<WorkerInfo> {
        let handles: Vec<Arc<Handle>> = self.workers().values().cloned().collect();
        let mut out = Vec::new();
        for h in handles.into_iter().rev() {
            let (model, effort) = match self.inner.kernel.store().session(h.id).await {
                Ok(r) => (r.model, r.effort),
                Err(_) => ("?".into(), "?".into()),
            };
            let st = h.st();
            out.push(WorkerInfo {
                id: h.id,
                kind: h.kind.clone(),
                title: h.title.clone(),
                model,
                effort,
                status: st.status,
                progress: st.progress.clone(),
                elapsed: st.finished.or_else(|| st.started.map(|t| t.elapsed())),
            });
        }
        out
    }

    /// 启动时、任何任务派出之前调用一次：把上次退出时仍带运行标记的执行者转成“被中断”汇报写入信箱。
    /// 逐项写入（写汇报的同一事务清标记），中途退出时剩下的标记保留到下次启动。
    pub async fn recover(&self) -> Result<usize, String> {
        let store = self.inner.kernel.store();
        let lost = store.running_markers().await.map_err(|e| e.to_string())?;
        for it in &lost {
            let row = store.session(it.worker).await.map_err(|e| e.to_string())?;
            let kind = match row.kind {
                SessionKind::Specialist => WorkerKind::Specialist(row.name.clone().unwrap_or_default()),
                _ => WorkerKind::Worker,
            };
            store.update_session(it.worker, None, None, Some(SessionStatus::Archived)).await.map_err(|e| e.to_string())?;
            let restored = store.restore_queue(it.worker).await.map_err(|e| e.to_string())?;
            if restored == 0 && !it.marked {
                continue;
            }
            let mut summary = INTERRUPTED_SUMMARY.to_owned();
            if restored > 0 {
                summary.push_str(&format!("运行中收到的 {restored} 条补充已写进它的会话。"));
            }
            let report = WorkerReport {
                mail: 0,
                id: it.worker,
                title: row.name.unwrap_or_default(),
                kind,
                status: WorkerStatus::Interrupted,
                summary,
                question: None,
                media: Vec::new(),
                notify: Notify::Relay,
                elapsed_s: 0,
            };
            let payload = serde_json::to_value(&report).map_err(|e| e.to_string())?;
            store.post_report(it.worker, it.owner, payload).await.map_err(|e| e.to_string())?;
        }
        if !lost.is_empty() {
            tracing::warn!(count = lost.len(), "发现上次退出时被中断的后台任务");
        }
        let archived_legacy = store.archive_idle_workers().await.map_err(|e| e.to_string())?;
        if archived_legacy > 0 {
            tracing::info!(count = archived_legacy, "已归档旧版遗留的闲置后台/专员会话");
        }
        Ok(lost.len())
    }

    /// 运行中则把消息放进它的队列（插话或追问）并记入数据库，返回 `None`；否则占下这个执行者并返回取消令牌。
    /// 检查与入队在同一把锁内，与收尾时的“检查剩余消息”互斥，不会漏消息。
    async fn enqueue_or_claim(&self, h: &Handle, msg: &Message, steer: bool, notify: Notify, owner: SessionId) -> Option<CancellationToken> {
        {
            let mut st = h.st();
            if !st.status.is_active() {
                return Some(Self::claim(&mut st, notify));
            }
            if steer {
                h.queues.steer(msg.clone());
            } else {
                h.queues.follow_up(msg.clone());
            }
        }
        let saved = match serde_json::to_value(msg) {
            Ok(v) => self.inner.kernel.store().queue_input(h.id, owner, v, tokens_of(msg)).await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        if let Err(e) = saved {
            tracing::warn!(id = h.id, error = %e, "插话落库失败，进程重启前退出会丢失这条");
        }
        None
    }

    fn claim(st: &mut HState, notify: Notify) -> CancellationToken {
        let cancel = CancellationToken::new();
        st.status = WorkerStatus::Queued;
        st.cancel = Some(cancel.clone());
        st.progress = None;
        st.question = None;
        st.finished = None;
        st.notify = notify;
        cancel
    }

    fn fail(h: &Handle) {
        let mut st = h.st();
        st.status = WorkerStatus::Failed;
        st.cancel = None;
    }

    /// 先把任务正文与运行标记写入数据库（同一事务），成功后在同一个后台任务里开跑：
    /// 返回 Ok 即保证进程随后退出也能在重启时发现并续接；调用方中途被取消也不会留下没人驱动的任务。
    async fn launch(&self, h: Arc<Handle>, input: Vec<Message>, owner: SessionId, cancel: CancellationToken, priority: bool) -> Result<(), String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let inner = self.inner.clone();
        tokio::spawn(async move {
            match Self::persist_input(&inner, h.id, owner, &input).await {
                Ok(()) => {
                    let _ = tx.send(Ok(()));
                    Self::drive(inner, h, owner, cancel, priority).await;
                }
                Err(e) => {
                    Self::fail(&h);
                    let _ = tx.send(Err(format!("保存任务失败：{e}")));
                }
            }
        });
        rx.await.map_err(|_| "派出任务时内部错误".to_owned())?
    }

    async fn persist_input(inner: &Inner, id: SessionId, owner: SessionId, input: &[Message]) -> Result<(), String> {
        let rows = input
            .iter()
            .map(|m| Ok((serde_json::to_value(m)?, tokens_of(m))))
            .collect::<Result<Vec<_>, serde_json::Error>>()
            .map_err(|e| e.to_string())?;
        inner.kernel.store().begin_run(id, owner, rows).await.map_err(|e| e.to_string())
    }

    /// 后台驱动：执行 → 汇报写入信箱（同时清运行标记）→ 公开结束状态 → 交给运行时。
    /// 公开结束状态前若队列里还有后到的插话或追问：被要求停止的只把它们写进会话；否则在同一会话上接着跑一轮。
    async fn drive(inner: Arc<Inner>, h: Arc<Handle>, owner: SessionId, mut cancel: CancellationToken, mut priority: bool) {
        loop {
            let mut report = Self::run(&inner, &h, cancel, priority).await;
            match serde_json::to_value(&report) {
                Ok(payload) => report.mail = Self::post_with_retry(&inner, h.id, owner, payload).await,
                Err(e) => tracing::error!(id = h.id, error = %e, "后台汇报序列化失败"),
            }
            let (leftover, next) = {
                let mut st = h.st();
                // stop 会取走取消令牌：汇报落库期间才到的 stop 同样不再续跑
                let stopped = report.status == WorkerStatus::Cancelled || st.cancel.is_none();
                let leftover = h.queues.take_all();
                if leftover.is_empty() || stopped {
                    st.status = report.status;
                    st.cancel = None;
                    st.finished = Some(Duration::from_secs(report.elapsed_s));
                    (leftover, None)
                } else {
                    let next = Self::claim(&mut st, Notify::Relay);
                    (leftover, Some(next))
                }
            };
            let _ = inner.reports.send(report);
            let Some(next) = next else {
                // 被要求停止：后到的消息不再执行，但写进会话，续接时看得到
                if !leftover.is_empty()
                    && let Err(e) = inner.kernel.append_messages(h.id, &leftover).await
                {
                    tracing::error!(id = h.id, error = %e, "保存停止后到达的消息失败");
                }
                return;
            };
            if let Err(e) = Self::persist_input(&inner, h.id, owner, &leftover).await {
                tracing::error!(id = h.id, error = %e, "保存后到的消息失败");
                for m in leftover {
                    h.queues.follow_up(m);
                }
                Self::fail(&h);
                return;
            }
            (cancel, priority) = (next, true);
        }
    }

    /// 写入信箱，失败时短暂重试；仍失败则返回 0（照常投递，运行标记保留，重启后会再报一次“被中断”）。
    async fn post_with_retry(inner: &Inner, id: SessionId, owner: SessionId, payload: serde_json::Value) -> i64 {
        let store = inner.kernel.store();
        for attempt in 0..3u32 {
            match store.post_report(id, owner, payload.clone()).await {
                Ok(mail) => return mail,
                Err(e) => {
                    tracing::warn!(id, attempt, error = %e, "后台汇报落库失败");
                    tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
                }
            }
        }
        0
    }

    async fn run(inner: &Inner, h: &Arc<Handle>, cancel: CancellationToken, priority: bool) -> WorkerReport {
        let started = Instant::now();
        let outcome: Result<lyrabot_agent::TurnOutcome, String> = async {
            let _spec_guard = match &h.kind {
                WorkerKind::Specialist(name) => {
                    let lock = Self::specialist_lock(inner, name);
                    Some(tokio::select! {
                        g = lock.lock_owned() => g,
                        _ = cancel.cancelled() => return Err("排队时被停止".to_owned()),
                    })
                }
                WorkerKind::Worker => None,
            };
            let _permit = tokio::select! {
                p = inner.sem.clone().acquire_owned() => p.map_err(|e| e.to_string())?,
                p = inner.spare.clone().acquire_owned(), if priority => p.map_err(|e| e.to_string())?,
                _ = cancel.cancelled() => return Err("排队时被停止".to_owned()),
            };
            {
                let mut st = h.st();
                st.status = WorkerStatus::Running;
                st.started = Some(Instant::now());
            }
            let spec = Self::spec(inner, h).await?;
            let store = inner.kernel.store();
            let _ = store.update_session(h.id, None, None, Some(SessionStatus::Active)).await;
            // 任务正文已在派出时写入会话，这里只驱动一轮
            let out = inner.kernel.run_turn(&spec, h.id, Vec::new(), &h.queues, cancel.clone()).await.map_err(|e| e.to_string())?;
            // 未得到回应的插话也要留在会话里，续接时才看得到
            if !out.unconsumed.is_empty() {
                inner.kernel.append_messages(h.id, &out.unconsumed).await.map_err(|e| e.to_string())?;
            }
            Ok(out)
        }
        .await;

        let (status, summary, media) = match outcome {
            Ok(o) => match o.reason {
                StopReason::Done => (WorkerStatus::Done, clip(&o.text, SUMMARY_LIMIT), o.media),
                StopReason::Cancelled => (WorkerStatus::Cancelled, "被要求停止，任务未完成。".to_owned(), o.media),
                StopReason::LoopGuard => (
                    WorkerStatus::Failed,
                    format!("工具调用陷入重复被停下。最后的进展：{}", clip(&o.text, 300)),
                    o.media,
                ),
                StopReason::Error => {
                    let why = o.error.as_ref().map(ToString::to_string).unwrap_or_default();
                    (WorkerStatus::Failed, format!("模型请求失败：{}", clip(&why, 300)), o.media)
                }
            },
            Err(e) if cancel.is_cancelled() => (WorkerStatus::Cancelled, format!("已停止：{e}"), Vec::new()),
            Err(e) => (WorkerStatus::Failed, format!("执行出错：{}", clip(&e, 300)), Vec::new()),
        };
        let (question, notify) = {
            let mut st = h.st();
            (st.question.take(), st.notify)
        };
        let status = if status == WorkerStatus::Done && question.is_some() { WorkerStatus::Waiting } else { status };
        let _ = inner.kernel.store().update_session(h.id, None, None, Some(SessionStatus::Archived)).await;
        let elapsed = started.elapsed();
        tracing::info!(id = h.id, status = status.label(), secs = elapsed.as_secs(), "后台任务结束");
        WorkerReport {
            mail: 0,
            id: h.id,
            title: h.title.clone(),
            kind: h.kind.clone(),
            status,
            summary,
            question,
            media,
            notify,
            elapsed_s: elapsed.as_secs(),
        }
    }

    /// 执行者一轮的规格：执行者系统提示（+专员专长与记忆）、工具白名单。
    async fn spec(inner: &Inner, h: &Arc<Handle>) -> Result<TurnSpec, String> {
        let c = inner.cfg.get();
        let o = &c.orchestra;
        let row = inner.kernel.store().session(h.id).await.map_err(|e| e.to_string())?;
        let p = c.provider(&o.provider).ok_or("编排提供方未配置")?;
        let info = p.models.get(&row.model).ok_or_else(|| format!("模型 {} 不在目录中", row.model))?;
        let base = std::fs::read_to_string(&o.worker_prompt)
            .map_err(|e| format!("读取执行者提示 {} 失败：{e}", o.worker_prompt.display()))?;
        let mut system = vec![base.trim().to_owned()];
        let tool_names: Vec<String> = match &h.kind {
            WorkerKind::Worker => o.worker_tools.clone(),
            WorkerKind::Specialist(name) => {
                let s = o.specialists.get(name).ok_or_else(|| format!("专员 {name} 已从配置中移除"))?;
                let mut extra = format!(
                    "## 你的身份\n你是专员「{name}」：{}。每次任务使用独立会话，请以当前任务与实际查询结果为准；跨任务需要长期记住的固定流程或环境事实，请参考下方长期记忆或用 memory 工具维护。",
                    s.description
                );
                if let Some(m) = &s.memory {
                    let path = c.persona.memory_dir.join(m);
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        let items: Vec<String> =
                            text.split("\n§\n").map(str::trim).filter(|e| !e.is_empty()).map(|e| format!("- {}", e.replace('\n', " "))).collect();
                        if !items.is_empty() {
                            extra.push_str(&format!("\n\n## 你的长期记忆\n{}", items.join("\n")));
                        }
                    }
                }
                system.push(extra);
                if s.tools.is_empty() { o.worker_tools.clone() } else { s.tools.clone() }
            }
        };
        let names: Vec<&str> = tool_names.iter().map(String::as_str).filter(|n| inner.tools.get(n).is_some()).collect();
        let missing: Vec<&String> = tool_names.iter().filter(|n| inner.tools.get(n).is_none()).collect();
        if !missing.is_empty() {
            tracing::warn!(?missing, "配置的工具在本机未注册，已忽略");
        }
        let mut tools = inner
            .tools
            .select(&names)
            .map_err(|e| e.to_string())?
            .with(Arc::new(crate::tools::ReportProgress::new(h.clone())) as Arc<dyn Tool>)
            .with(Arc::new(crate::tools::NeedDecision::new(h.clone())) as Arc<dyn Tool>);
        if let Some(extras) = &inner.extras {
            for t in extras(&c, &h.kind) {
                tools = tools.with(t);
            }
        }
        Ok(TurnSpec {
            provider: (inner.providers)(&c, &o.provider)?,
            model: row.model.clone(),
            effort: row.effort.parse().ok(),
            context_window: info.context_length.min(o.context_window),
            max_output_tokens: info.max_output_tokens,
            system,
            tools,
            tail: None,
        })
    }
}
