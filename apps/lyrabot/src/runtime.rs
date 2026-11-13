//! 运行时：持有配置、存储、内核与主对话角色会话，按 Pacer 节奏处理入站消息和命令。
//!
//! 主对话角色会话的阶段机（决策见 lyrabot_persona::pacer）：
//! Idle/Debouncing/Delivering 收到消息 → 进缓冲，去抖结束后合并为一轮；
//! Generating（尚未调用工具、尚未发出内容）收到消息 → 取消本轮，合并重来；
//! Tools 收到消息 → 作为插话注入当前轮。

use anyhow::{Context, Result};
use lyrabot_agent::{Kernel, KernelConfig, Queues, Tool, ToolRegistry, TurnOutcome, TurnSpec};
use lyrabot_command::{LearnCmd, ParseError};
use lyrabot_config::{Config, ConfigHandle, ModelInfo};
use lyrabot_core::card::{self, Card};
use lyrabot_core::{AgentEvent, Effort, Message, Role, SessionId, StopReason, Usage};
use lyrabot_orchestra::{AgentCtlTool, DelegateTool, Notify, Supervisor, WorkerKind, WorkerReport};
use lyrabot_persona::Persona;
use lyrabot_persona::learner::{self, Index};
use lyrabot_persona::pacer::{self, Disposition, PacerConfig, Phase};
use lyrabot_platform::{Inbound, Outbound, Platform};
use lyrabot_store::{NewSession, SessionKind, SessionRow, Store};
use lyrabot_tools::{Memory, MemoryTarget};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod commands;
mod learning;
mod tasks;

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
        let uname = persona.user_name();
        let memory = Memory::new(
            vec![
                MemoryTarget { name: "user".into(), file: pc.memory_dir.join("user.md"), about: format!("关于{uname}的事实与偏好") },
                MemoryTarget { name: "memory".into(), file: pc.memory_dir.join("memory.md"), about: "你自己的备忘（你们之间的约定、共同的事）".into() },
            ],
            &format!("只在{uname}明确让你记住、或纠正你记错的事时写；日常聊到的事实与约定会在后台自动整理进记忆，不必每次都写。"),
        )
        .with_lock(memory_lock.clone());
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        if let Some(notes) = persona.notes_tool() {
            tools.push(Arc::new(notes));
        }
        tools.push(Arc::new(DelegateTool::new(sup.clone(), c)));
        tools.push(Arc::new(AgentCtlTool::new(sup.clone())));
        tools.push(Arc::new(memory));
        match lyrabot_persona::Sticker::scan(&pc.stickers_dir) {
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
    douyin: Option<Arc<lyrabot_douyin::Douyin>>,
    kit: Mutex<Arc<PersonaKit>>,
    /// 表达/黑话检索索引；学习或删除后整体替换
    index: Mutex<Arc<Index>>,
    learning: AtomicBool,
    /// 主对话角色记忆文件（user.md / memory.md）的写锁：memory 工具、后台整理、/learn forget 与 undo 共用
    memory_lock: Arc<tokio::sync::Mutex<()>>,
    state: Mutex<State>,
    queues: Arc<Queues>,
    platform: OnceLock<Arc<dyn Platform>>,
    started: Instant,
}

/// 一条待交给主对话角色的后台汇报。
#[derive(Debug, Clone, PartialEq)]
struct Relay {
    /// 信箱信件 id；0 表示落库失败、无需确认
    id: i64,
    msg: Message,
    notify: Notify,
}

impl State {
    /// 有需要开一轮的内容：用户的消息，或需要立即转述的后台汇报。
    fn has_work(&self) -> bool {
        !self.pending.is_empty() || self.mail.iter().any(|r| r.notify == Notify::Relay)
    }

    /// 取消正在进行的保活请求、自增保活代次并重置计数；`clear_timer` 为真时一并清空上次模型请求时间（会话/模型切换或 /stop 后需等新对话再开始保活）。
    fn reset_keepalive(&mut self, clear_timer: bool) {
        if let Some(c) = self.keepalive_cancel.take() {
            c.cancel();
        }
        self.keepalive_count = 0;
        self.keepalive_epoch += 1;
        if clear_timer {
            self.last_llm_at = None;
        }
    }
}

/// 一轮结束时处理带上的汇报：输入已落库则全部确认；否则放回队首，
/// `quiet` 时（出错或 /stop）改为静默，等用户下次说话再带上，避免对同一错误反复重试。
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
    /// 当前正在分段投递的回复；/stop 时取消，停掉尚未发出的分段与媒体（已发出的保留）
    delivering: Option<CancellationToken>,
    /// 等待合并进下一轮的消息
    pending: Vec<Message>,
    first_pending: Option<Instant>,
    /// 去抖计时器代号；新消息或 /stop 使旧计时器失效
    timer_gen: u64,
    /// 最近一次收到用户消息的时间（空闲学习判断）
    last_activity: Option<Instant>,
    /// 后台结果附带的媒体，随下一轮主对话角色回复一起发出
    relay_media: Vec<PathBuf>,
    /// 当前这一轮正在发送的后台媒体（发完前不被媒体清理删除）
    media_inflight: Vec<PathBuf>,
    /// 等待交给主对话角色的后台汇报（信箱中尚未确认）；只在一轮开始时整体取用，不走插话通道
    mail: Vec<Relay>,
    /// 当前这一轮带上的汇报；该轮输入落库后确认，否则放回 `mail`
    inflight: Vec<Relay>,
    /// /stop 发出后为真：被取消那一轮未处理的输入直接丢弃，而不是重新排队
    discard_unconsumed: bool,
    /// 主会话最近一次完成模型请求（正式对话或保活）的时刻
    last_llm_at: Option<Instant>,
    /// 自用户上次说话以来已连续保活的次数
    keepalive_count: u32,
    /// 保活状态代次；切换会话/模型、重载或 /stop 时自增，使进行中的旧轮次或旧保活无法写回新状态
    keepalive_epoch: u64,
    /// 当前进行中的保活请求取消令牌
    keepalive_cancel: Option<CancellationToken>,
    last_usage: Usage,
    total_usage: Usage,
}

impl Runtime {
    pub async fn open(cfg: ConfigHandle) -> Result<Self> {
        let cfg = Arc::new(cfg);
        let c = cfg.get();
        let store = Store::open(&c.data_dir.join("lyrabot.db")).context("打开数据库")?;
        let kernel = Kernel::new(store.clone(), KernelConfig::from(&c.agent));
        let mut tools = ToolRegistry::default();
        let mut names = lyrabot_tools::register_builtin(&mut tools, &c, &store);
        names.extend(lyrabot_plugins::register(&mut tools, &lyrabot_plugins::Host::new(cfg.clone())));
        let douyin = c.douyin.as_ref().map(lyrabot_douyin::Douyin::new);
        if let Some(dy) = &douyin {
            tools.register(Arc::new(lyrabot_douyin::DouyinLogin::new(dy.clone())));
            tools.register(Arc::new(lyrabot_douyin::DouyinContent::new(dy.clone())));
            tools.register(Arc::new(lyrabot_douyin::DouyinVerification::new(dy.clone())));
            names.extend(["douyin_login", "douyin_content", "douyin_verification"].map(str::to_owned));
        }
        tracing::info!(tools = %names.join(","), "已注册工具");
        let (tx, reports) = mpsc::unbounded_channel();
        let sup = Supervisor::new(cfg.clone(), kernel.clone(), tools.clone(), tx, lyrabot_orchestra::default_providers())
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
            let uname = rt.kit().persona.user_name().to_owned();
            let mut st = rt.state();
            for m in &mail {
                match WorkerReport::from_mail(m) {
                    Some(r) => {
                        Self::queue_report(&mut st, r, &uname);
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
        let name = self.kit().persona.name().to_owned();
        let id = self
            .store
            .create_session(NewSession {
                kind: Some(SessionKind::Persona),
                name: Some(name),
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
        self.clone().spawn_keepalive();
        self.clone().spawn_report_consumer();
        self.clone().spawn_cron();
        self.clone().spawn_media_gc();
        self.clone().spawn_weekly_gc();
        let mut rx = self.kernel.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.session() != self.session() => {}
                    Ok(AgentEvent::Compacted { session, summarized: true, .. }) => {
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
                    lyrabot_core::ContentPart::Image { url } => url.strip_prefix("file://").map(PathBuf::from),
                    _ => None,
                })
                .collect();
            if let lyrabot_douyin::Intercept::Handled { notice, work } = dy.intercept(&text, &images) {
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
        match lyrabot_command::parse(&text) {
            Some(Ok(cmd)) => {
                let reply = self.command(cmd).await.unwrap_or_else(|e| card::warn(format!("命令失败：{e:#}")));
                let _ = self.platform().send(Outbound::text(reply)).await;
                let resume = {
                    let st = self.state();
                    st.phase == Phase::Idle && st.has_work()
                };
                if resume {
                    self.arm_timer();
                }
            }
            Some(Err(ParseError::BadArgs(n))) => {
                let _ = self.platform().send(Outbound::text(format!("{}\n\n{}", card::warn("参数不对"), lyrabot_command::help(Some(&n))))).await;
            }
            None => self.on_chat(Message::with_parts(Role::User, msg.parts), true),
        }
    }

    /// 按当前阶段处置一条消息。`from_user` 为假时（后台结果）不计入用户的活跃时间。
    fn on_chat(self: Arc<Self>, msg: Message, from_user: bool) {
        let mut st = self.state();
        if let Some(c) = st.keepalive_cancel.take() {
            c.cancel();
        }
        if from_user {
            st.last_activity = Some(Instant::now());
            st.keepalive_count = 0;
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
            let (inputs, media, cancel, turn_session, turn_epoch) = {
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
                (inputs, media, c, st.session, st.keepalive_epoch)
            };
            // 单独的任务里跑：即使本轮 panic，也能回到 finish_turn 复位状态，而不是永远卡在“回复中”
            let me = self.clone();
            let result = match tokio::spawn(async move { me.run_persona_turn(inputs, cancel).await }).await {
                Ok(r) => r,
                Err(e) => Err(anyhow::anyhow!("本轮异常中止：{e}")),
            };
            self.finish_turn(result, media, turn_session, turn_epoch).await;
        });
    }

    /// 一轮结束：投递结果，然后回到空闲；期间积累的消息开始新一轮去抖。
    async fn finish_turn(
        self: Arc<Self>,
        mut result: Result<TurnOutcome>,
        relay_media: Vec<PathBuf>,
        turn_session: SessionId,
        turn_epoch: u64,
    ) {
        let stop_delivery = CancellationToken::new();
        let acked = {
            let mut st = self.state();
            st.phase = Phase::Delivering;
            st.running = None;
            st.delivering = Some(stop_delivery.clone());
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
                if !discard
                    && st.session == turn_session
                    && st.keepalive_epoch == turn_epoch
                    && o.usage.prompt_tokens > 0
                {
                    st.last_llm_at = self.kernel.session_last_req(turn_session).or_else(|| Some(Instant::now()));
                }
            }
            let saved = matches!(&result, Ok(o) if o.inputs_saved);
            let failed = !matches!(&result, Ok(o) if o.reason != StopReason::Error);
            let inflight = std::mem::take(&mut st.inflight);
            settle(inflight, saved, discard || failed, &mut st.mail)
        };
        if let Err(e) = self.store.ack_mail(acked).await {
            tracing::warn!(error = %e, "确认后台汇报失败，重启后可能重复转述");
        }
        self.deliver(result, &stop_delivery).await;
        let has_pending = {
            let mut st = self.state();
            st.phase = Phase::Idle;
            st.delivering = None;
            st.media_inflight.clear();
            st.has_work()
        };
        if has_pending {
            self.arm_timer();
        }
    }

    async fn deliver(&self, result: Result<TurnOutcome>, stop: &CancellationToken) {
        let Some(platform) = self.platform.get() else { return };
        let streaming = platform.caps().streaming;
        let o = match result {
            Ok(o) => o,
            Err(e) => {
                tracing::error!(error = %e, "主对话本轮失败");
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
                tracing::error!(error = ?o.error, "主对话本轮模型请求失败");
                let _ = platform.send(Outbound::text(friendly(o.error.as_ref().map(|e| e.kind)))).await;
                return;
            }
            StopReason::Done => {}
        }
        if !streaming {
            let segs = pacer::segments(&o.text, self.pacer_cfg().max_segments);
            for (i, seg) in segs.iter().enumerate() {
                if i > 0 {
                    tokio::select! {
                        _ = tokio::time::sleep(pacer::segment_delay(seg)) => {}
                        _ = stop.cancelled() => return,
                    }
                }
                if stop.is_cancelled() {
                    return;
                }
                if let Err(e) = platform.send(Outbound::text(seg.clone())).await {
                    tracing::warn!(?e, "分段发送失败");
                    break;
                }
            }
        }
        if !o.media.is_empty() {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                _ = stop.cancelled() => return,
            }
            // 媒体逐个上传可能较久：/stop 时放弃尚未发出的部分（已发出的保留）
            tokio::select! {
                _ = platform.send(Outbound { text: String::new(), media: o.media }) => {}
                _ = stop.cancelled() => {}
            }
        }
    }

    /// 主对话角色一轮请求的完整规格：S1+S2 系统段、轻工具、时间尾注入。
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
            provider: lyrabot_llm::build(pcfg)?,
            model: s.model.clone(),
            effort: s.effort.parse::<Effort>().ok(),
            context_window: info.context_length,
            max_output_tokens: info.max_output_tokens,
            system: kit.persona.system(&s2),
            tools,
            tail: Some(self.tail(&kit.persona, c, query)),
            compaction: lyrabot_agent::CompactPolicy {
                ratio: c.agent.compaction_ratio,
                keep_tokens: c.agent.compaction_keep_tokens,
                style: lyrabot_agent::CompactStyle::Chat,
                time_label: Some({
                    let kit = kit.clone();
                    std::sync::Arc::new(move |ms: i64| kit.persona.time_label(ms))
                }),
            },
        })
    }

    /// 尾注入：当前时间 + 与本轮输入相关的表达提示（本地检索，不调用模型）。
    fn tail(&self, persona: &Persona, c: &Config, query: &str) -> String {
        let mut t = persona.tail(jiff::Timestamp::now());
        let index = self.index.lock().expect("index 锁中毒").clone();
        if let Some(h) = index.hints(query, persona.user_name(), 3, c.persona.learner.inject_max_chars) {
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

    /// `--check`：配置、数据库、人格资产、模型连通性自检（不连接聊天平台）。
    pub async fn check(&self) -> Result<()> {
        let c = self.cfg.get();
        let kit = self.kit();
        println!("版本：lyrabot {}", crate::VERSION);
        println!("配置：{}", self.cfg.path().display());
        println!("数据：{}", c.data_dir.display());
        println!("{}会话：#{}", kit.persona.name(), self.session());
        let names: Vec<_> = kit.tools.iter().map(|t| t.spec().name.clone()).collect();
        println!(
            "人格 S1：{} 字符；记忆目录 {}；人格工具 {}",
            kit.persona.identity().chars().count(),
            c.persona.memory_dir.display(),
            names.join(", ")
        );
        for (id, pc) in &c.llm.providers {
            match lyrabot_llm::build(pc)?.list_models().await {
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

    /// 退出前停下主对话角色当前一轮。后台任务不主动取消：保留运行标记，重启后作为“被中断”汇报交回，
    /// 而不是误报成“被要求停止”；其子进程随服务的 cgroup 一起结束。
    pub async fn shutdown(&self) {
        self.stop_current();
    }
}

fn model_info<'a>(c: &'a Config, model: &str) -> Result<&'a ModelInfo> {
    c.provider(&c.persona.provider)
        .and_then(|p| p.models.get(model))
        .with_context(|| format!("模型 {model} 不在模型目录中"))
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

/// 给用户看的错误说明：只说原因类别，原始响应只写日志。
fn friendly_error(e: &anyhow::Error) -> String {
    friendly(e.chain().find_map(|c| match c.downcast_ref::<lyrabot_agent::AgentError>() {
        Some(lyrabot_agent::AgentError::Llm(l)) => Some(l.kind),
        _ => c.downcast_ref::<lyrabot_llm::LlmError>().map(|l| l.kind),
    }))
}

fn friendly(kind: Option<lyrabot_llm::ErrorKind>) -> String {
    use lyrabot_llm::ErrorKind as K;
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
    use super::{Notify, Relay, State, requeue, settle};
    use lyrabot_core::Message;

    fn relay(id: i64, notify: Notify) -> Relay {
        Relay { id, msg: Message::user(format!("〔后台结果 {id}〕")), notify }
    }

    #[test]
    fn requeue_drops_relay_copies_and_keeps_order() {
        let m = |t: &str| Message::user(t);
        // 本轮输入 = [汇报, 用户1]，未落库被打断；插话与新消息依次排在后面
        let out = requeue(vec![m("〔汇报〕"), m("用户1")], 1, vec![m("插话")], vec![m("用户2")]);
        assert_eq!(out.iter().map(Message::text).collect::<Vec<_>>(), vec!["用户1", "插话", "用户2"]);
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
        assert!(!st.has_work(), "静默汇报等用户下次说话");
    }
}
