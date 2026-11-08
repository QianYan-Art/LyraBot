//! 运行时：持有配置、存储、内核与唐语歌会话，按 Pacer 节奏处理入站消息和命令。
//!
//! 唐语歌会话的阶段机（决策见 qya_persona::pacer）：
//! Idle/Debouncing/Delivering 收到消息 → 进缓冲，去抖结束后合并为一轮；
//! Generating（尚未调用工具、尚未发出内容）收到消息 → 取消本轮，合并重来；
//! Tools 收到消息 → 作为插话注入当前轮。

use anyhow::{Context, Result};
use qya_agent::{Kernel, KernelConfig, Queues, Tool, ToolRegistry, TurnOutcome, TurnSpec};
use qya_command::{AgentAction, Command, LearnCmd, ParseError};
use qya_config::{Config, ConfigHandle, ModelInfo};
use qya_core::card::{self, Card};
use qya_core::{AgentEvent, Effort, Message, Role, SessionId, StopReason, Usage};
use qya_persona::Persona;
use qya_persona::learner::{self, Index};
use qya_persona::pacer::{self, Disposition, PacerConfig, Phase};
use qya_orchestra::{AgentCtlTool, ContextMode, DelegateRequest, DelegateTool, Notify, Supervisor, Target, WorkerKind, WorkerReport};
use qya_tools::{Memory, MemoryTarget};
use qya_platform::{Inbound, Outbound, Platform};
use qya_store::{NewSession, SessionKind, SessionRow, Store};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod learning;

const KV_PERSONA_SESSION: &str = "persona_session";
const KV_LEARN_ENABLED: &str = "learn:enabled";

/// 表达学习在某会话中已处理到的条目 id。
fn kv_learn_ckpt(id: SessionId) -> String {
    format!("learn_ckpt:{id}")
}

/// 会话冻结的 S2 记忆快照键。
fn kv_snapshot(id: SessionId) -> String {
    format!("persona_s2:{id}")
}

/// 一次装载得到的人格及其专属工具；/reload 时整体替换。
struct PersonaKit {
    persona: Persona,
    tools: Vec<Arc<dyn Tool>>,
}

impl PersonaKit {
    fn load(c: &Config, sup: &Supervisor, memory_lock: &Arc<tokio::sync::Mutex<()>>) -> Result<Self> {
        let pc = &c.persona;
        let persona = Persona::load(&pc.assets_dir).context("装载人格资产")?;
        let memory = Memory::new(
            vec![
                MemoryTarget { name: "user".into(), file: pc.memory_dir.join("user.md"), about: "关于阿颜的事实与偏好".into() },
                MemoryTarget { name: "memory".into(), file: pc.memory_dir.join("memory.md"), about: "你自己的备忘（你们之间的约定、共同的事）".into() },
            ],
            "只在阿颜明确让你记住、或纠正你记错的事时写；日常聊到的事实与约定会在后台自动整理进记忆，不必每次都写。",
        )
        .with_lock(memory_lock.clone());
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(persona.notes_tool()),
            Arc::new(DelegateTool::new(sup.clone(), c)),
            Arc::new(AgentCtlTool::new(sup.clone())),
            Arc::new(memory),
        ];
        match qya_persona::Sticker::scan(&pc.stickers_dir) {
            Some(s) => tools.push(Arc::new(s)),
            None => tracing::info!(dir = %pc.stickers_dir.display(), "表情包目录不存在或为空，不启用 sticker"),
        }
        Ok(Self { persona, tools })
    }
}

pub struct Runtime {
    cfg: Arc<ConfigHandle>,
    store: Store,
    kernel: Kernel,
    tools: ToolRegistry,
    sup: Supervisor,
    reports: Mutex<Option<mpsc::UnboundedReceiver<WorkerReport>>>,
    /// 抖音外部服务（配置了 [douyin] 时）
    douyin: Option<Arc<qya_douyin::Douyin>>,
    kit: Mutex<Arc<PersonaKit>>,
    /// 表达/黑话检索索引；学习或删除后整体替换
    index: Mutex<Arc<Index>>,
    learning: AtomicBool,
    /// 唐语歌记忆文件（user.md / memory.md）的写锁：memory 工具、后台整理、/learn forget 与 undo 共用
    memory_lock: Arc<tokio::sync::Mutex<()>>,
    state: Mutex<State>,
    queues: Arc<Queues>,
    platform: OnceLock<Arc<dyn Platform>>,
    started: Instant,
}

/// 一条待交给唐语歌的后台汇报。
#[derive(Debug, Clone, PartialEq)]
struct Relay {
    /// 信箱信件 id；0 表示落库失败、无需确认
    id: i64,
    msg: Message,
    notify: Notify,
}

impl State {
    /// 有需要开一轮的内容：阿颜的消息，或需要立即转述的后台汇报。
    fn has_work(&self) -> bool {
        !self.pending.is_empty() || self.mail.iter().any(|r| r.notify == Notify::Relay)
    }
}

/// 一轮结束时处理带上的汇报：输入已落库则全部确认；否则放回队首，
/// `quiet` 时（出错或 /stop）改为静默，等阿颜下次说话再带上，避免对同一错误反复重试。
fn settle(inflight: Vec<Relay>, saved: bool, quiet: bool, queue: &mut Vec<Relay>) -> Vec<i64> {
    if saved {
        return inflight.into_iter().map(|r| r.id).filter(|&id| id != 0).collect();
    }
    let back = inflight.into_iter().map(|r| if quiet { Relay { notify: Notify::Silent, ..r } } else { r });
    let rest = std::mem::take(queue);
    queue.extend(back);
    queue.extend(rest);
    Vec::new()
}

/// 被打断的一轮：未得到回应的输入、内核队列里没来得及注入的消息、期间新到的消息，按到达顺序排回缓冲。
/// `skip` 为输入开头那几条后台汇报的条数（它们经 [`settle`] 放回汇报队列，这里去掉以免重复）。
fn requeue(mut unconsumed: Vec<Message>, skip: usize, mut leftover: Vec<Message>, mut pending: Vec<Message>) -> Vec<Message> {
    unconsumed.drain(..skip.min(unconsumed.len()));
    unconsumed.append(&mut leftover);
    unconsumed.append(&mut pending);
    unconsumed
}

#[derive(Default)]
struct State {
    session: SessionId,
    phase: Phase,
    /// 当前进行中的一轮
    running: Option<CancellationToken>,
    /// 等待合并进下一轮的消息
    pending: Vec<Message>,
    first_pending: Option<Instant>,
    /// 去抖计时器代号；新消息或 /stop 使旧计时器失效
    timer_gen: u64,
    /// 最近一次收到阿颜消息的时间（空闲学习判断）
    last_activity: Option<Instant>,
    /// 后台结果附带的媒体，随下一轮唐语歌的回复一起发出
    relay_media: Vec<PathBuf>,
    /// 当前这一轮正在发送的后台媒体（发完前不被媒体清理删除）
    media_inflight: Vec<PathBuf>,
    /// 等待交给唐语歌的后台汇报（信箱中尚未确认）；只在一轮开始时整体取用，不走插话通道
    mail: Vec<Relay>,
    /// 当前这一轮带上的汇报；该轮输入落库后确认，否则放回 `mail`
    inflight: Vec<Relay>,
    /// /stop 发出后为真：被取消那一轮未处理的输入直接丢弃，而不是重新排队
    discard_unconsumed: bool,
    last_usage: Usage,
    total_usage: Usage,
}

impl Runtime {
    pub async fn open(cfg: ConfigHandle) -> Result<Self> {
        let cfg = Arc::new(cfg);
        let c = cfg.get();
        let store = Store::open(&c.data_dir.join("qya.db")).context("打开数据库")?;
        let kernel = Kernel::new(store.clone(), KernelConfig::from(&c.agent));
        let mut tools = ToolRegistry::default();
        let mut names = qya_tools::register_builtin(&mut tools, &c, &store);
        names.extend(qya_plugins::register(&mut tools, &qya_plugins::Host::new(cfg.clone())));
        let douyin = c.douyin.as_ref().map(qya_douyin::Douyin::new);
        if let Some(dy) = &douyin {
            tools.register(Arc::new(qya_douyin::DouyinLogin::new(dy.clone())));
            tools.register(Arc::new(qya_douyin::DouyinContent::new(dy.clone())));
            tools.register(Arc::new(qya_douyin::DouyinVerification::new(dy.clone())));
            names.extend(["douyin_login", "douyin_content", "douyin_verification"].map(str::to_owned));
        }
        tracing::info!(tools = %names.join(","), "已注册工具");
        let (tx, reports) = mpsc::unbounded_channel();
        let sup = Supervisor::new(cfg.clone(), kernel.clone(), tools.clone(), tx, qya_orchestra::default_providers())
            .with_extras(Arc::new(specialist_memory));
        let memory_lock = Arc::new(tokio::sync::Mutex::new(()));
        let kit = Arc::new(PersonaKit::load(&c, &sup, &memory_lock)?);
        let index = Arc::new(Index::build(store.expressions().await?));
        let rt = Self {
            cfg,
            store,
            kernel,
            tools,
            sup,
            reports: Mutex::new(Some(reports)),
            douyin,
            kit: Mutex::new(kit),
            index: Mutex::new(index),
            learning: AtomicBool::new(false),
            memory_lock,
            state: Mutex::new(State::default()),
            queues: Arc::new(Queues::default()),
            platform: OnceLock::new(),
            started: Instant::now(),
        };
        let session = match rt.store.kv_get(KV_PERSONA_SESSION).await?.and_then(|s| s.parse().ok()) {
            Some(id) if rt.store.session(id).await.is_ok() => id,
            _ => rt.new_persona_session().await?,
        };
        rt.state().session = session;
        // 在平台接入、定时任务启动之前：把上次被中断的任务转成汇报，并装载所有未确认的汇报
        let recovered = rt.sup.recover().await.map_err(anyhow::Error::msg).context("恢复被中断的后台任务")?;
        // 上次记忆整理若在写文件途中退出，补写完整（修订已记下）
        if let Some(rev) = rt.store.last_memory_revision().await?
            && rt.finish_interrupted_consolidation(&rev)?
        {
            tracing::warn!(revision = rev.id, "补写了上次中断的记忆整理");
        }
        let mail = rt.store.pending_mail().await?;
        {
            let mut st = rt.state();
            for m in &mail {
                match WorkerReport::from_mail(m) {
                    Some(r) => {
                        Self::queue_report(&mut st, r);
                    }
                    None => tracing::warn!(mail = m.id, "无法解析的后台汇报，已跳过"),
                }
            }
        }
        if recovered > 0 || !mail.is_empty() {
            tracing::info!(recovered, pending = mail.len(), "已装载未确认的后台汇报");
        }
        Ok(rt)
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn config(&self) -> Arc<Config> {
        self.cfg.get()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("state 锁中毒")
    }

    fn kit(&self) -> Arc<PersonaKit> {
        self.kit.lock().expect("kit 锁中毒").clone()
    }

    fn platform(&self) -> &Arc<dyn Platform> {
        self.platform.get().expect("平台尚未接入")
    }

    fn pacer_cfg(&self) -> PacerConfig {
        let p = self.cfg.get().persona.pacer;
        PacerConfig {
            debounce: Duration::from_millis(p.debounce_ms),
            max_wait: Duration::from_millis(p.max_wait_ms),
            max_segments: p.max_segments,
        }
    }

    async fn new_persona_session(&self) -> Result<SessionId> {
        let c = self.cfg.get();
        let id = self
            .store
            .create_session(NewSession {
                kind: Some(SessionKind::Persona),
                name: Some("唐语歌".into()),
                model: c.persona.model.clone(),
                effort: c.persona.effort.to_string(),
                ..Default::default()
            })
            .await?;
        self.refresh_snapshot(id).await?;
        self.store.kv_set(KV_PERSONA_SESSION, &id.to_string()).await?;
        Ok(id)
    }

    /// 重新生成并冻结某会话的 S2 记忆快照（新会话、压缩后调用）。
    async fn refresh_snapshot(&self, id: SessionId) -> Result<String> {
        let s2 = self.kit().persona.snapshot(&self.cfg.get().persona.memory_dir);
        self.store.kv_set(&kv_snapshot(id), &s2).await?;
        Ok(s2)
    }

    async fn snapshot(&self, id: SessionId) -> Result<String> {
        match self.store.kv_get(&kv_snapshot(id)).await? {
            Some(s) => Ok(s),
            None => self.refresh_snapshot(id).await,
        }
    }

    fn session(&self) -> SessionId {
        self.state().session
    }

    /// 接入平台并订阅内核事件：流式转发文本、跟踪工具阶段、压缩后刷新记忆快照。
    pub fn attach(self: Arc<Self>, platform: Arc<dyn Platform>) {
        let streaming = platform.caps().streaming;
        if self.platform.set(platform.clone()).is_err() {
            tracing::warn!("平台已接入，忽略重复 attach");
            return;
        }
        self.clone().spawn_learner();
        self.clone().spawn_report_consumer();
        self.clone().spawn_cron();
        self.clone().spawn_media_gc();
        self.clone().spawn_weekly_gc();
        let mut rx = self.kernel.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.session() != self.session() => {}
                    Ok(AgentEvent::Compacted { session, .. }) => {
                        if let Err(e) = self.refresh_snapshot(session).await {
                            tracing::warn!(error = %e, "压缩后刷新记忆快照失败");
                        }
                    }
                    Ok(AgentEvent::ToolStart { name, .. }) => {
                        {
                            let mut st = self.state();
                            if st.phase == Phase::Generating {
                                st.phase = Phase::Tools;
                            }
                        }
                        if streaming {
                            platform.stream_delta(&format!("\n[工具 {name}]\n")).await;
                        }
                    }
                    Ok(AgentEvent::TextDelta { text, .. }) if streaming => platform.stream_delta(&text).await,
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => tracing::warn!(n, "事件滞后，丢弃"),
                    Err(_) => break,
                }
            }
        });
    }

    pub async fn handle(self: Arc<Self>, msg: Inbound) {
        let text = msg.text();
        // 抖音前置拦截：发布确认与短信验证码由程序接管，原文不进入模型、日志或会话记录
        if let Some(dy) = &self.douyin {
            let images: Vec<PathBuf> = msg
                .parts
                .iter()
                .filter_map(|p| match p {
                    qya_core::ContentPart::Image { url } => url.strip_prefix("file://").map(PathBuf::from),
                    _ => None,
                })
                .collect();
            if let qya_douyin::Intercept::Handled { notice, work } = dy.intercept(&text, &images) {
                let _ = self.platform().send(Outbound::text(notice)).await;
                if let Some(work) = work {
                    let me = self.clone();
                    tokio::spawn(async move {
                        let out = work.await;
                        if let Some(n) = out.user_notice {
                            let _ = me.platform().send(Outbound::text(n)).await;
                        }
                        if let Some(note) = out.persona_note {
                            me.on_chat(Message::user(note), false);
                        }
                    });
                }
                return;
            }
        }
        match qya_command::parse(&text) {
            Some(Ok(cmd)) => {
                let reply = self.command(cmd).await.unwrap_or_else(|e| card::warn(format!("命令失败：{e:#}")));
                let _ = self.platform().send(Outbound::text(reply)).await;
            }
            Some(Err(ParseError::BadArgs(n))) => {
                let _ = self.platform().send(Outbound::text(format!("{}\n\n{}", card::warn("参数不对"), qya_command::help(Some(&n))))).await;
            }
            None => self.on_chat(Message::with_parts(Role::User, msg.parts), true),
        }
    }

    /// 消费后台结果：转为〔后台结果〕排队交给唐语歌，按 notify 决定立即开一轮转述还是等阿颜下次说话。
    fn spawn_report_consumer(self: Arc<Self>) {
        let Some(mut rx) = self.reports.lock().expect("reports 锁中毒").take() else { return };
        // 启动时装载的汇报若需要立即转述，先开一轮
        if self.state().has_work() {
            self.clone().arm_timer();
        }
        tokio::spawn(async move {
            while let Some(r) = rx.recv().await {
                let start = {
                    let mut st = self.state();
                    let relay = r.notify == Notify::Relay;
                    Self::queue_report(&mut st, r) && relay && matches!(st.phase, Phase::Idle | Phase::Debouncing)
                };
                if start {
                    self.clone().arm_timer();
                }
            }
        });
    }

    /// 汇报入队（按信件 id 去重），返回是否新加入。
    fn queue_report(st: &mut State, r: WorkerReport) -> bool {
        if r.mail != 0 && st.mail.iter().chain(&st.inflight).any(|m| m.id == r.mail) {
            return false;
        }
        st.relay_media.extend(r.media.iter().cloned());
        st.mail.push(Relay { id: r.mail, msg: Message::user(relay_text(&r)), notify: r.notify });
        true
    }

    /// 每周自动清理一次历史会话（上次时间记在 kv，进程重启不影响周期）。
    fn spawn_weekly_gc(self: Arc<Self>) {
        const KEY: &str = "gc:last";
        const WEEK_MS: i64 = 7 * 86_400_000;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tick.tick().await;
                let now = jiff::Timestamp::now().as_millisecond();
                let last = match self.store.kv_get(KEY).await {
                    Ok(v) => v.and_then(|v| v.parse::<i64>().ok()),
                    Err(e) => {
                        tracing::warn!(error = %e, "读取清理时间失败");
                        continue;
                    }
                };
                let Some(last) = last else {
                    // 首次运行只记下起点，一周后再清理
                    let _ = self.store.kv_set(KEY, &now.to_string()).await;
                    continue;
                };
                if now - last < WEEK_MS || self.state().phase != Phase::Idle || self.sup.running() > 0 {
                    continue;
                }
                let days = self.cfg.get().agent.gc_days;
                match self.gc(days).await {
                    Ok(msg) => tracing::info!(%msg, "每周自动清理完成"),
                    Err(e) => tracing::warn!(error = %e, "每周自动清理失败"),
                }
                let _ = self.store.kv_set(KEY, &now.to_string()).await;
            }
        });
    }

    async fn gc(&self, days: u32) -> Result<String> {
        // 排队或运行中的执行者（包括刚被续接、数据库里仍是 archived 的）不能被清理
        let mut keep = vec![self.session()];
        keep.extend(self.sup.active_ids());
        let r = self.store.gc_sessions(days, keep).await?;
        self.sup.forget(&r.deleted_ids);
        let mb = |b: i64| b as f64 / 1048576.0;
        let mut c = Card::new("🧹", "清理完成");
        c.line(format!("结束超过 {days} 天的会话"))
            .item(format!("删除会话 {} 个、记录 {} 条", r.sessions, r.entries))
            .item(format!("数据库 {:.2} → {:.2} MB", mb(r.bytes_before), mb(r.bytes_after)));
        if r.kept_referenced > 0 {
            c.item(format!("{} 个仍被较新的会话引用，暂时保留", r.kept_referenced));
        }
        Ok(c.build())
    }

    async fn sessions_report(&self) -> Result<String> {
        let cur = self.session();
        let list = self.store.persona_sessions(10).await?;
        let stats = self.store.stats().await?;
        let tz = jiff::tz::TimeZone::get("Asia/Shanghai").unwrap_or(jiff::tz::TimeZone::fixed(jiff::tz::offset(8)));
        let fmt = |ms: i64| {
            jiff::Timestamp::from_millisecond(ms).map(|t| t.to_zoned(tz.clone()).strftime("%m-%d %H:%M").to_string()).unwrap_or_default()
        };
        let mut c = Card::new("🗂", "会话与数据库");
        c.section("唐语歌最近的会话");
        for x in &list {
            let mark = if x.id == cur { "▶" } else { "·" };
            let state = if x.id == cur { "当前" } else if x.status == "closed" { "已结束" } else { "历史" };
            c.line(format!("{mark} #{} {state}｜{} 条｜{}", x.id, x.messages, fmt(x.updated_at)));
        }
        let label = |k: &str, st: &str| match (k, st) {
            ("persona", "active") => "唐语歌进行中",
            ("persona", "closed") => "唐语歌已结束",
            ("worker", "active") => "后台任务进行中",
            ("worker", "archived") => "后台任务已归档",
            ("specialist", _) => "常驻专员",
            _ => "其他",
        };
        c.section("合计");
        for (k, st, n) in &stats.sessions {
            c.item(format!("{} {n} 个", label(k, st)));
        }
        c.item(format!("记录 {} 条，数据库 {:.2} MB", stats.entries, stats.bytes as f64 / 1048576.0))
            .hint(format!("每周自动清理结束超过 {} 天的会话", self.cfg.get().agent.gc_days))
            .hint("手动清理：/gc 天数");
        Ok(c.build())
    }

    /// 媒体缓存清理：每小时删除媒体目录下超过 24 小时的文件（沿用 Hermes 的保留期）。
    fn spawn_media_gc(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tick.tick().await;
                let dir = self.cfg.get().tools.media_dir.clone();
                let keep = match self.referenced_media().await {
                    Ok(k) => k,
                    Err(e) => {
                        tracing::warn!(error = %e, "读取待投递媒体失败，本次跳过清理");
                        continue;
                    }
                };
                let removed = tokio::task::spawn_blocking(move || gc_dir(&dir, Duration::from_secs(24 * 3600), &keep)).await.unwrap_or(0);
                if removed > 0 {
                    tracing::info!(removed, "已清理过期媒体文件");
                }
            }
        });
    }

    /// 未确认的汇报、发件箱与待随回复发出的媒体文件（清理时保留）。
    async fn referenced_media(&self) -> Result<HashSet<PathBuf>> {
        let mut keep: HashSet<PathBuf> = {
            let st = self.state();
            st.relay_media.iter().chain(&st.media_inflight).cloned().collect()
        };
        for m in self.store.pending_mail().await? {
            keep.extend(m.payload["media"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).map(PathBuf::from));
        }
        for q in self.store.outbox_list("qq").await? {
            keep.extend(q.payload["media"].as_str().map(PathBuf::from));
        }
        Ok(keep)
    }

    /// 定时任务调度：每 30 秒检查一次到期任务。
    fn spawn_cron(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let jobs = match self.store.cron_list().await {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::warn!(error = %e, "读取定时任务失败");
                        continue;
                    }
                };
                let now = jiff::Timestamp::now();
                for job in qya_tools::cron::due(&jobs, now) {
                    // 先记下（一次性任务直接删除）再触发：重启不会重复触发
                    let recorded = if qya_tools::cron::is_once(&job) {
                        self.store.cron_remove(job.id).await.map(|_| ())
                    } else {
                        self.store.cron_mark_run(job.id, now.as_millisecond()).await
                    };
                    if let Err(e) = recorded {
                        tracing::warn!(error = %e, id = job.id, "记录定时任务运行失败");
                        continue;
                    }
                    tracing::info!(id = job.id, name = %job.name, "定时任务触发");
                    self.clone().fire_cron(job).await;
                }
            }
        });
    }

    async fn fire_cron(self: Arc<Self>, job: qya_store::CronJob) {
        let action = &job.action;
        match action["kind"].as_str() {
            Some("task") => {
                let to = action["to"].as_str().unwrap_or("worker");
                let req = DelegateRequest {
                    parent: self.session(),
                    parent_call_id: format!("cron-{}", job.id),
                    task: action["task"].as_str().unwrap_or_default().to_owned(),
                    context: ContextMode::Brief,
                    context_note: Some(format!("这是定时任务 #{}「{}」自动触发的，阿颜此刻不一定在线。", job.id, job.name)),
                    target: if to == "worker" { Target::Worker } else { Target::Specialist(to.to_owned()) },
                    model: None,
                    effort: None,
                    notify: Notify::Relay,
                };
                if let Err(e) = self.sup.delegate(req).await {
                    let msg = format!("〔定时任务 #{}「{}」派发失败：{e}〕\n〔请告诉阿颜这次没能执行〕", job.id, job.name);
                    self.on_chat(Message::user(msg), false);
                }
            }
            _ => {
                let msg = format!(
                    "〔定时提醒 #{}「{}」到点了〕\n{}\n〔这是阿颜之前让你设的提醒，不是他刚说的话。用你自己的话提醒他。〕",
                    job.id,
                    job.name,
                    action["text"].as_str().unwrap_or_default()
                );
                self.on_chat(Message::user(msg), false);
            }
        }
    }

    /// 按当前阶段处置一条消息。`from_user` 为假时（后台结果）不计入阿颜的活跃时间。
    fn on_chat(self: Arc<Self>, msg: Message, from_user: bool) {
        let mut st = self.state();
        if from_user {
            st.last_activity = Some(Instant::now());
        }
        match pacer::on_message(st.phase) {
            Disposition::Steer => self.queues.steer(msg),
            Disposition::CancelAndMerge => {
                tracing::debug!("生成中收到新消息，取消本轮合并重来");
                st.pending.push(msg);
                if let Some(c) = st.running.take() {
                    c.cancel();
                }
                // 本轮结束后由 finish_turn 重新开始去抖
            }
            Disposition::Buffer => {
                st.pending.push(msg);
                if matches!(st.phase, Phase::Idle | Phase::Debouncing) {
                    drop(st);
                    self.arm_timer();
                }
            }
        }
    }

    /// （重新）启动去抖计时器。
    fn arm_timer(self: Arc<Self>) {
        let (gen_, wait) = {
            let mut st = self.state();
            st.phase = Phase::Debouncing;
            let first = *st.first_pending.get_or_insert_with(Instant::now);
            st.timer_gen += 1;
            (st.timer_gen, self.pacer_cfg().wait(first.elapsed()))
        };
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let (inputs, media, cancel) = {
                let mut st = self.state();
                if st.timer_gen != gen_ || st.phase != Phase::Debouncing || !st.has_work() {
                    return;
                }
                st.first_pending = None;
                st.phase = Phase::Generating;
                let c = CancellationToken::new();
                st.running = Some(c.clone());
                st.inflight = std::mem::take(&mut st.mail);
                let mut inputs: Vec<Message> = st.inflight.iter().map(|r| r.msg.clone()).collect();
                inputs.append(&mut st.pending);
                let media = std::mem::take(&mut st.relay_media);
                st.media_inflight = media.clone();
                (inputs, media, c)
            };
            // 单独的任务里跑：即使本轮 panic，也能回到 finish_turn 复位状态，而不是永远卡在“回复中”
            let me = self.clone();
            let result = match tokio::spawn(async move { me.run_persona_turn(inputs, cancel).await }).await {
                Ok(r) => r,
                Err(e) => Err(anyhow::anyhow!("本轮异常中止：{e}")),
            };
            self.finish_turn(result, media).await;
        });
    }

    /// 一轮结束：投递结果，然后回到空闲；期间积累的消息开始新一轮去抖。
    async fn finish_turn(self: Arc<Self>, mut result: Result<TurnOutcome>, relay_media: Vec<PathBuf>) {
        let acked = {
            let mut st = self.state();
            st.phase = Phase::Delivering;
            st.running = None;
            let discard = std::mem::take(&mut st.discard_unconsumed);
            // 内核队列里还没来得及注入的插话/追问：被打断时并回缓冲，/stop 时一并丢弃
            let mut leftover = self.queues.take_all();
            match &mut result {
                // 被打断的一轮不投递：未得到回应的输入排回缓冲最前面，后台媒体留给下一轮
                Ok(o) if o.reason == StopReason::Cancelled => {
                    if !discard {
                        let unconsumed = std::mem::take(&mut o.unconsumed);
                        let skip = if o.inputs_saved { 0 } else { st.inflight.len() };
                        let pending = std::mem::take(&mut st.pending);
                        st.pending = requeue(unconsumed, skip, std::mem::take(&mut leftover), pending);
                    }
                    st.relay_media.extend(relay_media);
                }
                // 出错的一轮不投递回复，后台媒体留给下一轮
                Ok(o) if o.reason == StopReason::Error => st.relay_media.extend(relay_media),
                Ok(o) => o.media.extend(relay_media),
                Err(_) => st.relay_media.extend(relay_media),
            }
            // 正常结束后才到达队列的消息（错过了最后一次检查）排在缓冲最前，下一轮处理
            if !discard && !leftover.is_empty() {
                leftover.append(&mut st.pending);
                st.pending = leftover;
            }
            if let Ok(o) = &result {
                st.last_usage = o.usage;
                st.total_usage.add(&o.usage);
            }
            let saved = matches!(&result, Ok(o) if o.inputs_saved);
            let failed = !matches!(&result, Ok(o) if o.reason != StopReason::Error);
            let inflight = std::mem::take(&mut st.inflight);
            settle(inflight, saved, discard || failed, &mut st.mail)
        };
        if let Err(e) = self.store.ack_mail(acked).await {
            tracing::warn!(error = %e, "确认后台汇报失败，重启后可能重复转述");
        }
        self.deliver(result).await;
        let has_pending = {
            let mut st = self.state();
            st.phase = Phase::Idle;
            st.media_inflight.clear();
            st.has_work()
        };
        if has_pending {
            self.arm_timer();
        }
    }

    async fn deliver(&self, result: Result<TurnOutcome>) {
        let platform = self.platform();
        let streaming = platform.caps().streaming;
        let o = match result {
            Ok(o) => o,
            Err(e) => {
                tracing::error!(error = %e, "唐语歌本轮失败");
                if streaming {
                    platform.stream_end().await;
                }
                let _ = platform.send(Outbound::text(friendly_error(&e))).await;
                return;
            }
        };
        if streaming {
            platform.stream_end().await;
        }
        match o.reason {
            StopReason::Cancelled => return, // 被打断或 /stop：不投递半成品
            StopReason::LoopGuard => {
                let _ = platform.send(Outbound::text("（工具调用陷入重复，已停下）")).await;
            }
            StopReason::Error => {
                tracing::error!(error = ?o.error, "唐语歌本轮模型请求失败");
                let _ = platform.send(Outbound::text(friendly(o.error.as_ref().map(|e| e.kind)))).await;
                return;
            }
            StopReason::Done => {}
        }
        if !streaming {
            let segs = pacer::segments(&o.text, self.pacer_cfg().max_segments);
            for (i, seg) in segs.iter().enumerate() {
                if i > 0 {
                    tokio::time::sleep(pacer::segment_delay(seg)).await;
                }
                if let Err(e) = platform.send(Outbound::text(seg.clone())).await {
                    tracing::warn!(?e, "分段发送失败");
                    break;
                }
            }
        }
        if !o.media.is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = platform.send(Outbound { text: String::new(), media: o.media, reply_to: None }).await;
        }
    }

    /// 唐语歌一轮请求的完整规格：S1+S2 系统段、轻工具、时间尾注入。
    async fn persona_spec(&self, c: &Config, s: &SessionRow, query: &str) -> Result<TurnSpec> {
        let pcfg = c.provider(&c.persona.provider).context("persona provider 未配置")?;
        let info = model_info(c, &s.model)?;
        let kit = self.kit();
        let s2 = self.snapshot(s.id).await?;
        let mut wanted: Vec<&str> = c.persona.tools.iter().map(String::as_str).filter(|n| self.tools.get(n).is_some()).collect();
        // watch_video：当前模型看不了视频时代看；模型支持视频但视频超过内联上限时也靠它回退
        if self.tools.get("watch_video").is_some() && !wanted.contains(&"watch_video") {
            wanted.push("watch_video");
        }
        let mut tools = self.tools.select(&wanted).map_err(anyhow::Error::msg)?;
        for t in &kit.tools {
            tools = tools.with(t.clone());
        }
        Ok(TurnSpec {
            provider: qya_llm::build(pcfg)?,
            model: s.model.clone(),
            effort: s.effort.parse::<Effort>().ok(),
            context_window: info.context_length,
            max_output_tokens: info.max_output_tokens,
            system: kit.persona.system(&s2),
            tools,
            tail: Some(self.tail(&kit.persona, c, query)),
        })
    }

    /// 尾注入：当前时间 + 与本轮输入相关的表达提示（本地检索，不调用模型）。
    fn tail(&self, persona: &Persona, c: &Config, query: &str) -> String {
        let mut t = persona.tail(jiff::Timestamp::now());
        let index = self.index.lock().expect("index 锁中毒").clone();
        if let Some(h) = index.hints(query, 3, c.persona.learner.inject_max_chars) {
            t.push('\n');
            t.push_str(&h);
        }
        t
    }

    async fn run_persona_turn(&self, inputs: Vec<Message>, cancel: CancellationToken) -> Result<TurnOutcome> {
        let c = self.cfg.get();
        let s = self.store.session(self.session()).await?;
        let query: Vec<String> = inputs.iter().map(Message::text).collect();
        let spec = self.persona_spec(&c, &s, &query.join("\n")).await?;
        tracing::debug!(tail = ?spec.tail, "尾注入");
        Ok(self.kernel.run_turn(&spec, s.id, inputs, &self.queues, cancel).await?)
    }

    async fn command(&self, cmd: Command) -> Result<String> {
        let c = self.cfg.get();
        Ok(match cmd {
            Command::Help(t) => qya_command::help(t.as_deref()),
            Command::Status => self.status().await?,
            Command::New => {
                self.stop_current();
                let old = self.session();
                let id = self.new_persona_session().await?;
                self.state().session = id;
                self.store.update_session(old, None, None, Some(qya_store::SessionStatus::Closed)).await?;
                format!("{}\n记忆和关系不变\n旧会话 #{old} 已结束，{} 天后可被清理", card::ok(format!("已开启新会话 #{id}")), c.agent.gc_days)
            }
            Command::Sessions => self.sessions_report().await?,
            Command::Gc(days) => self.gc(days.unwrap_or(c.agent.gc_days)).await?,
            Command::Stop => {
                if self.stop_current() {
                    card::ok("已停止")
                } else {
                    "💤 当前没有进行中的回复".into()
                }
            }
            Command::Compact => {
                if self.state().phase != Phase::Idle {
                    return Ok(card::warn("正在回复中，稍后再压缩"));
                }
                let s = self.store.session(self.session()).await?;
                let mut spec = self.persona_spec(&c, &s, "").await?;
                spec.tail = None;
                let ctx = qya_agent::context::build(&self.store.branch(s.id).await?)?;
                let est = qya_agent::context::estimate_messages(&ctx);
                if self.kernel.compact(&spec, s.id, &ctx, est, &CancellationToken::new()).await? {
                    card::ok(format!("已压缩，压缩前约 {} token", tokens(est)))
                } else {
                    "💤 内容太少，无需压缩".into()
                }
            }
            Command::Model(None) => {
                let s = self.store.session(self.session()).await?;
                let p = c.provider(&c.persona.provider).context("persona provider 未配置")?;
                let mut out = Card::new("🧠", "模型");
                out.line(format!("当前 {}", s.model)).section("可选");
                for m in p.models.keys() {
                    out.line(if *m == s.model { format!("▶ {m}") } else { format!("· {m}") });
                }
                out.hint("切换：/model 名称").build()
            }
            Command::Model(Some(m)) => {
                model_info(&c, &m)?;
                self.store.update_session(self.session(), Some(m.clone()), None, None).await?;
                card::ok(format!("唐语歌的模型已切换为 {m}"))
            }
            Command::Effort(None) => {
                let s = self.store.session(self.session()).await?;
                Card::new("💭", "思考等级")
                    .line(format!("当前 {}", s.effort))
                    .line("可选 low / medium / high / xhigh / max")
                    .hint("切换：/effort 等级")
                    .build()
            }
            Command::Effort(Some(e)) => {
                let e: Effort = e.parse().map_err(anyhow::Error::msg)?;
                self.store.update_session(self.session(), None, Some(e.to_string()), None).await?;
                let p = c.provider(&c.persona.provider).context("persona provider 未配置")?;
                format!("{}\n实际请求参数：{}", card::ok(format!("思考等级已切换为 {e}")), p.effort_param(e))
            }
            Command::Learn(cmd) => self.learn_command(cmd).await?,
            Command::Agents => qya_orchestra::render_list(&self.sup.list().await),
            Command::Provider(None) => qya_plugins::provider_cmd::list(&self.cfg),
            Command::Provider(Some((cat, field, value))) => {
                qya_plugins::provider_cmd::apply(&self.cfg, &cat, &field, &value).map_or_else(card::warn, card::ok)
            }
            Command::Agent { id, action } => match action {
                AgentAction::Show => self.agent_detail(id).await?,
                AgentAction::Stop => self.sup.stop(id).map_or_else(card::warn, card::ok),
                AgentAction::Say(t) => self.sup.continue_worker(id, &t, self.session()).await.map_or_else(card::warn, card::ok),
            },
            Command::Reload => {
                let c = self.cfg.reload()?;
                let kit = PersonaKit::load(&c, &self.sup, &self.memory_lock)?;
                *self.kit.lock().expect("kit 锁中毒") = Arc::new(kit);
                format!("{}\n记忆快照在新会话或压缩后生效", card::ok("配置与人格资产已重新加载"))
            }
        })
    }

    /// 停止当前一轮并丢弃待处理缓冲；返回是否确有东西被停止。
    fn stop_current(&self) -> bool {
        let mut st = self.state();
        st.timer_gen += 1;
        st.first_pending = None;
        let had_pending = !std::mem::take(&mut st.pending).is_empty();
        if st.phase == Phase::Debouncing {
            st.phase = Phase::Idle;
        }
        match st.running.take() {
            Some(c) => {
                st.discard_unconsumed = true;
                c.cancel();
                true
            }
            None => had_pending,
        }
    }

    async fn status(&self) -> Result<String> {
        let c = self.cfg.get();
        let s = self.store.session(self.session()).await?;
        let info = model_info(&c, &s.model)?;
        let ctx = qya_agent::context::build(&self.store.branch(s.id).await?)?;
        let est = qya_agent::context::estimate_messages(&ctx);
        let workers = self.sup.running();
        let (phase, pending, last, total) = {
            let st = self.state();
            (st.phase, st.pending.len(), st.last_usage, st.total_usage)
        };
        let rss = memory_stats::memory_stats().map(|m| m.physical_mem / 1024 / 1024).unwrap_or(0);
        let up = self.started.elapsed().as_secs();
        let hit = |u: &Usage| if u.prompt_tokens == 0 { 0.0 } else { u.cached_tokens as f64 * 100.0 / u.prompt_tokens as f64 };
        let phase = match phase {
            Phase::Idle => "空闲",
            Phase::Debouncing => "等你说完",
            Phase::Generating => "回复中",
            Phase::Tools => "调用工具中",
            Phase::Delivering => "发送中",
        };
        Ok(Card::new("📊", "运行状态")
            .line(format!("qya {}", crate::VERSION))
            .section("会话")
            .item(format!("#{}｜{phase}", s.id))
            .item(format!("{}｜思考 {}", s.model, s.effort))
            .section("上下文")
            .line(card::bar(est as f64 / info.context_length.max(1) as f64))
            .line(format!("约 {} / {} token", tokens(est), tokens(info.context_length)))
            .section("用量")
            .item(format!("上一轮 输入 {}，输出 {}", tokens(last.prompt_tokens), tokens(last.completion_tokens)))
            .item(format!("缓存命中 {:.0}%（累计 {:.0}%）", hit(&last), hit(&total)))
            .item(format!("累计输入 {}", tokens(total.prompt_tokens)))
            .section("运行")
            .item(format!("待合并 {pending} 条，后台 {workers} 个"))
            .item(format!("内存 {rss} MB，已运行 {}", duration(up)))
            .build())
    }

    /// `--check`：配置、数据库、人格资产、模型连通性自检（不连接聊天平台）。
    pub async fn check(&self) -> Result<()> {
        let c = self.cfg.get();
        println!("版本：qya {}", crate::VERSION);
        println!("配置：{}", self.cfg.path().display());
        println!("数据：{}", c.data_dir.display());
        println!("唐语歌会话：#{}", self.session());
        let kit = self.kit();
        let names: Vec<_> = kit.tools.iter().map(|t| t.spec().name.clone()).collect();
        println!(
            "人格 S1：{} 字符；记忆目录 {}；人格工具 {}",
            kit.persona.identity().chars().count(),
            c.persona.memory_dir.display(),
            names.join(", ")
        );
        for (id, pc) in &c.llm.providers {
            match qya_llm::build(pc)?.list_models().await {
                Ok(list) => {
                    let ids: Vec<_> = list.iter().map(|m| m.id.as_str()).collect();
                    println!("提供方 {id}：可用模型 {}", ids.join(", "));
                    for m in pc.models.keys() {
                        if !ids.contains(&m.as_str()) {
                            println!("  ⚠ 配置中的 {m} 不在当前 key 的可用列表中");
                        }
                    }
                }
                Err(e) => println!("提供方 {id}：连通失败 {e}"),
            }
        }
        let rss = memory_stats::memory_stats().map(|m| m.physical_mem / 1024 / 1024).unwrap_or(0);
        println!("内存 {rss} MB");
        Ok(())
    }

    /// 退出前停下唐语歌当前一轮。后台任务不主动取消：保留运行标记，重启后作为“被中断”汇报交回，
    /// 而不是误报成“被要求停止”；其子进程随服务的 cgroup 一起结束。
    pub async fn shutdown(&self) {
        self.stop_current();
    }

    /// /agent <id>：基本信息 + 该执行者最近一条回复。
    async fn agent_detail(&self, id: SessionId) -> Result<String> {
        let row = self.store.session(id).await.with_context(|| format!("没有 #{id}"))?;
        if row.kind == SessionKind::Persona {
            anyhow::bail!("#{id} 是唐语歌的会话，不是后台任务");
        }
        let info = self.sup.list().await.into_iter().find(|w| w.id == id);
        let status = info.as_ref().map(|w| w.status.label()).unwrap_or("已归档");
        let mut c = Card::new("🤖", &format!("后台 #{id}｜{status}"));
        c.line(row.name.unwrap_or_default()).item(format!("{}｜思考 {}", row.model, row.effort));
        if let Some(p) = info.and_then(|w| w.progress) {
            c.item(format!("进度：{p}"));
        }
        let msgs = self.store.messages_since(id, 0, 50).await?;
        let last = msgs
            .iter()
            .rev()
            .filter_map(|e| serde_json::from_value::<Message>(e.payload.clone()).ok())
            .find(|m| m.role == Role::Assistant && !m.text().trim().is_empty());
        if let Some(m) = last {
            let t: String = m.text().chars().take(800).collect();
            c.section("最近回复").line(t);
        }
        c.hint(format!("续接：/agent {id} 话"));
        Ok(c.build())
    }
}

/// token 数的简写：1234 → 1.2k，1234567 → 1.2M。
fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// 时长的简写：3725 秒 → 1 小时 2 分。
fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} 秒"),
        60..3600 => format!("{} 分", secs / 60),
        3600..86400 => format!("{} 小时 {} 分", secs / 3600, secs % 3600 / 60),
        _ => format!("{} 天 {} 小时", secs / 86400, secs % 86400 / 3600),
    }
}

fn model_info<'a>(c: &'a Config, model: &str) -> Result<&'a ModelInfo> {
    c.provider(&c.persona.provider)
        .and_then(|p| p.models.get(model))
        .with_context(|| format!("模型 {model} 不在模型目录中"))
}

/// 后台结果交给唐语歌时的消息文本（以〔〕标明是系统信息，见 assets/persona/runtime/agency.md）。
fn relay_text(r: &WorkerReport) -> String {
    let who = match &r.kind {
        WorkerKind::Worker => format!("后台 #{}「{}」", r.id, r.title),
        WorkerKind::Specialist(n) => format!("专员 {n}（#{}）", r.id),
    };
    let mut s = format!("〔后台结果：{who}{}，用时 {} 秒〕\n{}", r.status.label(), r.elapsed_s, r.summary);
    if let Some(q) = &r.question {
        s.push_str(&format!("\n〔需要阿颜决定〕{q}"));
    }
    s.push_str("\n〔这是后台交回的结果，不是阿颜的新消息。用你自己的话把要点告诉阿颜；没做成就如实说。");
    if r.question.is_some() {
        s.push_str("需要阿颜决定的问题要单独、简短地问出来，等阿颜回答后再用 agent_ctl continue 交给它。");
    }
    s.push('〕');
    s
}

/// 专员只能编辑自己的记忆文件。
fn specialist_memory(c: &Config, kind: &WorkerKind) -> Vec<Arc<dyn Tool>> {
    let WorkerKind::Specialist(name) = kind else { return Vec::new() };
    let Some(file) = c.orchestra.specialists.get(name).and_then(|s| s.memory.clone()) else { return Vec::new() };
    let target = MemoryTarget {
        name: name.clone(),
        file: c.persona.memory_dir.join(file),
        about: format!("专员 {name} 的长期工作记忆（固定流程、注意事项、环境事实）"),
    };
    vec![Arc::new(Memory::new(vec![target], "确认是以后还会用到的流程或事实再写。"))]
}

/// 递归删除目录下修改时间早于 `max_age` 的普通文件（不跟随符号链接、跳过 `keep` 中的文件），返回删除数量。
fn gc_dir(dir: &std::path::Path, max_age: Duration, keep: &HashSet<PathBuf>) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for e in rd.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else { continue };
        if meta.is_dir() {
            n += gc_dir(&e.path(), max_age, keep);
        } else if meta.is_file() && !keep.contains(&e.path()) && meta.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|age| age > max_age) && std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

/// 给阿颜看的错误说明：只说原因类别，原始响应只写日志。
fn friendly_error(e: &anyhow::Error) -> String {
    friendly(e.chain().find_map(|c| match c.downcast_ref::<qya_agent::AgentError>() {
        Some(qya_agent::AgentError::Llm(l)) => Some(l.kind),
        _ => c.downcast_ref::<qya_llm::LlmError>().map(|l| l.kind),
    }))
}

fn friendly(kind: Option<qya_llm::ErrorKind>) -> String {
    use qya_llm::ErrorKind as K;
    match kind {
        Some(K::QuotaExhausted) => "（我这边的模型额度用完了，要等额度重置后才能好好回你。）".into(),
        Some(K::RateLimited) => "（模型那边有点忙，过一会儿再找我好吗。）".into(),
        Some(K::Auth) => "（模型认证失败了，需要检查一下 key。）".into(),
        Some(K::Transient) => "（暂时连不上模型，等会儿再试试。）".into(),
        Some(K::ContextOverflow) => "（这段对话太长了，发 /compact 压缩一下或 /new 开个新会话再继续。）".into(),
        _ => "（刚才出了点问题，没能回复，详细原因记在日志里了。）".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Notify, Relay, State, duration, requeue, settle, tokens};

    #[test]
    fn compact_numbers_for_chat() {
        assert_eq!((tokens(999), tokens(12_345), tokens(262_144), tokens(1_048_576)), ("999".into(), "12.3k".into(), "262.1k".into(), "1.0M".into()));
        assert_eq!((duration(42), duration(125), duration(3_725), duration(90_000)), ("42 秒".into(), "2 分".into(), "1 小时 2 分".into(), "1 天 1 小时".into()));
    }
    use qya_core::Message;

    fn relay(id: i64, notify: Notify) -> Relay {
        Relay { id, msg: Message::user(format!("〔后台结果 {id}〕")), notify }
    }

    #[test]
    fn requeue_drops_relay_copies_and_keeps_order() {
        let m = |t: &str| Message::user(t);
        // 本轮输入 = [汇报, 阿颜1]，未落库被打断；插话与新消息依次排在后面
        let out = requeue(vec![m("〔汇报〕"), m("阿颜1")], 1, vec![m("插话")], vec![m("阿颜2")]);
        assert_eq!(out.iter().map(Message::text).collect::<Vec<_>>(), vec!["阿颜1", "插话", "阿颜2"]);
        // 输入已落库时 unconsumed 只剩之后到的消息，不做截取
        let out = requeue(vec![m("插话")], 0, vec![], vec![]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn settle_acks_only_saved_turns() {
        let mut queue = vec![relay(3, Notify::Relay)];
        // 输入已落库：确认（id 0 表示未落库的汇报，不需要确认）
        assert_eq!(settle(vec![relay(1, Notify::Relay), relay(0, Notify::Relay)], true, false, &mut queue), vec![1]);
        assert_eq!(queue.len(), 1);
        // 被新消息打断、尚未落库：原样放回队首
        assert!(settle(vec![relay(2, Notify::Relay)], false, false, &mut queue).is_empty());
        assert_eq!(queue.iter().map(|r| (r.id, r.notify)).collect::<Vec<_>>(), vec![(2, Notify::Relay), (3, Notify::Relay)]);
        // 出错或 /stop：放回并改为静默，不反复重试
        let mut queue = Vec::new();
        settle(vec![relay(4, Notify::Relay)], false, true, &mut queue);
        assert_eq!(queue, vec![relay(4, Notify::Silent)]);
        let st = State { mail: queue, ..Default::default() };
        assert!(!st.has_work(), "静默汇报等阿颜下次说话");
    }

    #[test]
    fn gc_removes_only_old_files() {
        let d = std::env::temp_dir().join(format!("qya-gc-{}", std::process::id()));
        std::fs::create_dir_all(d.join("inbox")).unwrap();
        std::fs::write(d.join("inbox/new.jpg"), b"x").unwrap();
        let none = std::collections::HashSet::new();
        assert_eq!(super::gc_dir(&d, std::time::Duration::from_secs(3600), &none), 0);
        let keep = std::collections::HashSet::from([d.join("inbox/new.jpg")]);
        assert_eq!(super::gc_dir(&d, std::time::Duration::ZERO, &keep), 0, "仍被引用的文件保留");
        assert_eq!(super::gc_dir(&d, std::time::Duration::ZERO, &none), 1);
        assert!(!d.join("inbox/new.jpg").exists());
    }
}
