use qya_agent::{Kernel, Queues, Tool, ToolRegistry, TurnSpec};
use qya_config::{Config, ConfigHandle};
use qya_core::{Effort, Message, SessionId, StopReason};
use qya_store::{NewSession, SessionKind, SessionStatus};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 按提供方 id 取得模型客户端。运行时传入真实实现，测试传入假实现。
pub type ProviderFn = Arc<dyn Fn(&Config, &str) -> Result<Arc<dyn qya_llm::Provider>, String> + Send + Sync>;

/// 按执行者身份追加的专属工具（如专员只能写自己的记忆文件）。
pub type ExtrasFn = Arc<dyn Fn(&Config, &WorkerKind) -> Vec<Arc<dyn Tool>> + Send + Sync>;

/// 默认实现：按配置构造。
pub fn default_providers() -> ProviderFn {
    Arc::new(|c: &Config, id: &str| {
        let p = c.provider(id).ok_or_else(|| format!("提供方 {id} 未配置"))?;
        qya_llm::build(p).map_err(|e| e.to_string())
    })
}
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

/// worker 汇报摘要的上限（字）；完整过程留在 worker 会话里。
const SUMMARY_LIMIT: usize = 1200;
/// fork 模式下为唐语歌那次 delegate 调用补的工具结果。
const FORK_PLACEHOLDER: &str = "（这件事已交给后台执行者处理）";
const FORK_INSTRUCTION: &str = "〔以上是唐语歌和阿颜的对话，供你了解来龙去脉。从现在起你是后台执行者，不扮演唐语歌，不对阿颜说话，只完成下面的任务并按系统提示写汇报。〕";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notify {
    /// 完成后立即交给唐语歌转述
    Relay,
    /// 只入信箱，等阿颜下次说话时一并带上
    Silent,
}

#[derive(Debug, Clone)]
pub struct DelegateRequest {
    /// 发起方（唐语歌）会话
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerKind {
    Worker,
    Specialist(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStatus {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl WorkerStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "排队中",
            Self::Running => "进行中",
            Self::Done => "已完成",
            Self::Failed => "失败",
            Self::Cancelled => "已停止",
        }
    }
    fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

/// 交回唐语歌的结果。
#[derive(Debug, Clone)]
pub struct WorkerReport {
    pub id: SessionId,
    pub title: String,
    pub kind: WorkerKind,
    pub status: WorkerStatus,
    pub summary: String,
    pub media: Vec<PathBuf>,
    pub notify: Notify,
    pub elapsed: Duration,
}

/// /agents 与 agent_ctl 展示用的快照。
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
            state: Mutex::new(HState { status, progress: None, cancel: None, started: None, finished: None, notify: Notify::Relay }),
        })
    }
    fn st(&self) -> std::sync::MutexGuard<'_, HState> {
        self.state.lock().expect("worker 状态锁中毒")
    }
    pub(crate) fn set_progress(&self, p: String) {
        self.st().progress = Some(p);
    }
}

struct Inner {
    cfg: Arc<ConfigHandle>,
    kernel: Kernel,
    tools: ToolRegistry,
    sem: Arc<Semaphore>,
    workers: Mutex<BTreeMap<SessionId, Arc<Handle>>>,
    reports: mpsc::UnboundedSender<WorkerReport>,
    providers: ProviderFn,
    extras: Option<ExtrasFn>,
}

#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Inner>,
}

fn kv_specialist(name: &str) -> String {
    format!("specialist:{name}")
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

    /// 派出任务，立即返回（不等待执行）。成功时返回给唐语歌的确认文本。
    pub async fn delegate(&self, req: DelegateRequest) -> Result<String, String> {
        let c = self.inner.cfg.get();
        let (model, effort) = Self::resolve_model(&c, &req.target, req.model.clone(), req.effort)?;
        let store = self.inner.kernel.store();
        let err = |e: qya_store::StoreError| format!("存储错误：{e}");
        let task = req.task.trim();
        if task.is_empty() {
            return Err("任务内容不能为空".into());
        }
        let note = req.context_note.as_deref().map(str::trim).filter(|n| !n.is_empty());
        let mut downgraded = false;

        let (handle, input) = match &req.target {
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
                        // 分叉点是唐语歌那条带 delegate 调用的消息，先补上结果保持消息结构合法
                        input.push(Message::tool_result(&req.parent_call_id, FORK_PLACEHOLDER, false));
                        format!("{FORK_INSTRUCTION}\n\n任务：{task}{}", note.map(|n| format!("\n补充：{n}")).unwrap_or_default())
                    }
                    (ContextMode::Brief, Some(n)) => format!("任务：{task}\n\n背景：{n}"),
                    _ => format!("任务：{task}"),
                };
                input.push(Message::user(body));
                let h = Handle::new(id, WorkerKind::Worker, clip(task, 30), WorkerStatus::Queued);
                self.workers().insert(id, h.clone());
                (h, input)
            }
            Target::Specialist(name) => {
                if req.context == ContextMode::Fork {
                    downgraded = true;
                }
                let id = match store.kv_get(&kv_specialist(name)).await.map_err(err)?.and_then(|v| v.parse().ok()) {
                    Some(id) if store.session(id).await.is_ok() => id,
                    _ => {
                        let id = store
                            .create_session(NewSession {
                                kind: Some(SessionKind::Specialist),
                                name: Some(name.clone()),
                                model: model.clone(),
                                effort: effort.to_string(),
                                ..Default::default()
                            })
                            .await
                            .map_err(err)?;
                        store.kv_set(&kv_specialist(name), &id.to_string()).await.map_err(err)?;
                        id
                    }
                };
                store.update_session(id, Some(model.clone()), Some(effort.to_string()), None).await.map_err(err)?;
                let body = match note {
                    Some(n) => format!("新任务：{task}\n\n背景：{n}"),
                    None => format!("新任务：{task}"),
                };
                let h = self
                    .workers()
                    .entry(id)
                    .or_insert_with(|| Handle::new(id, WorkerKind::Specialist(name.clone()), name.clone(), WorkerStatus::Done))
                    .clone();
                if h.st().status.is_active() {
                    h.queues.follow_up(Message::user(body));
                    return Ok(format!("专员 {name} 正在忙，任务已排在它手头的活之后（#{id}）"));
                }
                (h, vec![Message::user(body)])
            }
        };
        handle.st().notify = req.notify;
        let id = handle.id;
        self.spawn(handle, input);
        let who = match &req.target {
            Target::Worker => format!("后台 #{id}"),
            Target::Specialist(n) => format!("专员 {n}（#{id}）"),
        };
        let mut msg = format!("已交给{who}（模型 {model}，思考 {effort}）。结果出来后会以〔后台结果〕交回给你。");
        if downgraded {
            msg.push_str("专员有自己的长期会话，fork 不适用，已按 brief 处理。");
        }
        Ok(msg)
    }

    /// 续接或插话：运行中则作为插话注入，已结束则在原会话上再跑一轮。
    pub async fn continue_worker(&self, id: SessionId, text: &str) -> Result<String, String> {
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
        if handle.st().status.is_active() {
            handle.queues.steer(msg);
            return Ok(format!("已把这句话插给 #{id}"));
        }
        handle.st().notify = Notify::Relay;
        self.spawn(handle, vec![msg]);
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

    pub fn stop_all(&self) {
        for h in self.workers().values() {
            if let Some(c) = h.st().cancel.take() {
                c.cancel();
            }
        }
    }

    /// 会话被清理后移除对应句柄。
    pub fn forget(&self, ids: &[SessionId]) {
        let mut w = self.workers();
        for id in ids {
            w.remove(id);
        }
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

    fn spawn(&self, h: Arc<Handle>, input: Vec<Message>) {
        let cancel = CancellationToken::new();
        {
            let mut st = h.st();
            st.status = WorkerStatus::Queued;
            st.cancel = Some(cancel.clone());
            st.progress = None;
            st.finished = None;
        }
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let report = Self::run(&inner, &h, input, cancel).await;
            let _ = inner.reports.send(report);
        });
    }

    async fn run(inner: &Inner, h: &Arc<Handle>, input: Vec<Message>, cancel: CancellationToken) -> WorkerReport {
        let started = Instant::now();
        let outcome: Result<qya_agent::TurnOutcome, String> = async {
            let _permit = tokio::select! {
                p = inner.sem.clone().acquire_owned() => p.map_err(|e| e.to_string())?,
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
            let out = inner.kernel.run_turn(&spec, h.id, input, &h.queues, cancel.clone()).await.map_err(|e| e.to_string())?;
            // 执行者没有“合并重来”的需求：未得到回应的任务说明也要留在会话里，续接时才看得到
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
                StopReason::Error => (WorkerStatus::Failed, clip(&o.text, SUMMARY_LIMIT), o.media),
            },
            Err(e) if cancel.is_cancelled() => (WorkerStatus::Cancelled, format!("已停止：{e}"), Vec::new()),
            Err(e) => (WorkerStatus::Failed, format!("执行出错：{}", clip(&e, 300)), Vec::new()),
        };
        let elapsed = started.elapsed();
        let notify = {
            let mut st = h.st();
            st.status = status;
            st.cancel = None;
            st.finished = Some(elapsed);
            st.notify
        };
        if h.kind == WorkerKind::Worker {
            let _ = inner.kernel.store().update_session(h.id, None, None, Some(SessionStatus::Archived)).await;
        }
        tracing::info!(id = h.id, status = status.label(), secs = elapsed.as_secs(), "后台任务结束");
        WorkerReport { id: h.id, title: h.title.clone(), kind: h.kind.clone(), status, summary, media, notify, elapsed }
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
                let mut extra = format!("## 你的身份\n你是常驻专员「{name}」：{}。你有自己的长期会话，之前处理过的事可以直接参考。", s.description);
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
            .with(Arc::new(crate::tools::ReportProgress::new(h.clone())) as Arc<dyn Tool>);
        if let Some(extras) = &inner.extras {
            for t in extras(&c, &h.kind) {
                tools = tools.with(t);
            }
        }
        Ok(TurnSpec {
            provider: (inner.providers)(&c, &o.provider)?,
            model: row.model.clone(),
            effort: row.effort.parse().ok(),
            context_window: info.context_length,
            max_output_tokens: info.max_output_tokens,
            system,
            tools,
            tail: None,
        })
    }
}
