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
use qya_core::{AgentEvent, Effort, Message, Role, SessionId, StopReason, Usage};
use qya_persona::Persona;
use qya_persona::learner::{self, Index};
use qya_persona::pacer::{self, Disposition, PacerConfig, Phase};
use qya_orchestra::{AgentCtlTool, ContextMode, DelegateRequest, DelegateTool, Notify, Supervisor, Target, WorkerKind, WorkerReport};
use qya_tools::{Memory, MemoryTarget};
use qya_platform::{Inbound, Outbound, Platform};
use qya_store::{NewSession, SessionKind, SessionRow, Store};
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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
    fn load(c: &Config, sup: &Supervisor) -> Result<Self> {
        let pc = &c.persona;
        let persona = Persona::load(&pc.assets_dir).context("装载人格资产")?;
        let memory = Memory::new(
            vec![
                MemoryTarget { name: "user".into(), file: pc.memory_dir.join("user.md"), about: "关于阿颜的事实与偏好".into() },
                MemoryTarget { name: "memory".into(), file: pc.memory_dir.join("memory.md"), about: "你自己的备忘（你们之间的约定、共同的事）".into() },
            ],
            "阿颜明确让你记住、或你确认是长期偏好时再写。",
        );
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
    state: Mutex<State>,
    queues: Arc<Queues>,
    platform: OnceLock<Arc<dyn Platform>>,
    started: Instant,
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
        let kit = Arc::new(PersonaKit::load(&c, &sup)?);
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
                let reply = self.command(cmd).await.unwrap_or_else(|e| format!("命令失败：{e:#}"));
                let _ = self.platform().send(Outbound::text(reply)).await;
            }
            Some(Err(ParseError::Unknown(n))) => {
                let _ = self.platform().send(Outbound::text(format!("没有 /{n} 这个命令，发 /help 看全部"))).await;
            }
            Some(Err(ParseError::BadArgs(n))) => {
                let _ = self.platform().send(Outbound::text(format!("参数不对：{}", qya_command::help(Some(&n))))).await;
            }
            None => self.on_chat(Message::with_parts(Role::User, msg.parts), true),
        }
    }

    /// 消费后台结果：转为〔后台结果〕消息交给唐语歌，按 notify 决定立即转述还是等阿颜下次说话。
    fn spawn_report_consumer(self: Arc<Self>) {
        let Some(mut rx) = self.reports.lock().expect("reports 锁中毒").take() else { return };
        tokio::spawn(async move {
            while let Some(r) = rx.recv().await {
                let msg = Message::user(relay_text(&r));
                let mut st = self.state();
                st.relay_media.extend(r.media.iter().cloned());
                match r.notify {
                    Notify::Silent => st.pending.push(msg),
                    Notify::Relay => {
                        drop(st);
                        self.clone().on_chat(msg, false);
                    }
                }
            }
        });
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
                if now - last < WEEK_MS || self.state().phase != Phase::Idle {
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
        let keep = vec![self.session()];
        let r = self.store.gc_sessions(days, keep).await?;
        self.sup.forget(&r.deleted_ids);
        let mb = |b: i64| b as f64 / 1048576.0;
        let mut s = format!(
            "已清理 {} 天前的 {} 个会话、{} 条记录；数据库 {:.2} MB → {:.2} MB",
            days,
            r.sessions,
            r.entries,
            mb(r.bytes_before),
            mb(r.bytes_after)
        );
        if r.kept_referenced > 0 {
            s.push_str(&format!("（另有 {} 个会话仍被较新的会话引用，暂时保留）", r.kept_referenced));
        }
        Ok(s)
    }

    async fn sessions_report(&self) -> Result<String> {
        let cur = self.session();
        let list = self.store.persona_sessions(10).await?;
        let stats = self.store.stats().await?;
        let tz = jiff::tz::TimeZone::get("Asia/Shanghai").unwrap_or(jiff::tz::TimeZone::fixed(jiff::tz::offset(8)));
        let fmt = |ms: i64| {
            jiff::Timestamp::from_millisecond(ms).map(|t| t.to_zoned(tz.clone()).strftime("%m-%d %H:%M").to_string()).unwrap_or_default()
        };
        let mut s = String::from("唐语歌的会话（最近 10 个）：\n");
        for x in &list {
            let mark = if x.id == cur { "▶ 当前" } else if x.status == "closed" { "已结束" } else { "历史" };
            s.push_str(&format!("#{} {mark} · {} 条消息 · 最后 {}（创建 {}）\n", x.id, x.messages, fmt(x.updated_at), fmt(x.created_at)));
        }
        let label = |k: &str, st: &str| match (k, st) {
            ("persona", "active") => "唐语歌·进行中",
            ("persona", "closed") => "唐语歌·已结束",
            ("worker", "active") => "后台任务·进行中",
            ("worker", "archived") => "后台任务·已归档",
            ("specialist", _) => "常驻专员",
            _ => "其他",
        };
        let parts: Vec<String> = stats.sessions.iter().map(|(k, st, n)| format!("{} {n}", label(k, st))).collect();
        s.push_str(&format!(
            "合计：{}\n记录 {} 条 · 数据库 {:.2} MB · 自动清理每周一次，保留 {} 天",
            parts.join("，"),
            stats.entries,
            stats.bytes as f64 / 1048576.0,
            self.cfg.get().agent.gc_days
        ));
        Ok(s)
    }

    /// 媒体缓存清理：每小时删除媒体目录下超过 24 小时的文件（沿用 Hermes 的保留期）。
    fn spawn_media_gc(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tick.tick().await;
                let dir = self.cfg.get().tools.media_dir.clone();
                let removed = tokio::task::spawn_blocking(move || gc_dir(&dir, Duration::from_secs(24 * 3600))).await.unwrap_or(0);
                if removed > 0 {
                    tracing::info!(removed, "已清理过期媒体文件");
                }
            }
        });
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
                    if let Err(e) = self.store.cron_mark_run(job.id, now.as_millisecond()).await {
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
                if st.timer_gen != gen_ || st.phase != Phase::Debouncing || st.pending.is_empty() {
                    return;
                }
                st.first_pending = None;
                st.phase = Phase::Generating;
                let c = CancellationToken::new();
                st.running = Some(c.clone());
                (std::mem::take(&mut st.pending), std::mem::take(&mut st.relay_media), c)
            };
            let result = self.run_persona_turn(inputs, cancel).await;
            self.finish_turn(result, media).await;
        });
    }

    /// 一轮结束：投递结果，然后回到空闲；期间积累的消息开始新一轮去抖。
    async fn finish_turn(self: Arc<Self>, mut result: Result<TurnOutcome>, relay_media: Vec<PathBuf>) {
        {
            let mut st = self.state();
            st.phase = Phase::Delivering;
            st.running = None;
            let discard = std::mem::take(&mut st.discard_unconsumed);
            match &mut result {
                // 被打断的一轮不投递：未得到回应的输入排回缓冲最前面，后台媒体留给下一轮
                Ok(o) if o.reason == StopReason::Cancelled => {
                    if !discard && !o.unconsumed.is_empty() {
                        let mut merged = std::mem::take(&mut o.unconsumed);
                        merged.append(&mut st.pending);
                        st.pending = merged;
                    }
                    st.relay_media.extend(relay_media);
                }
                Ok(o) => o.media.extend(relay_media),
                Err(_) => st.relay_media.extend(relay_media),
            }
            if let Ok(o) = &result {
                st.last_usage = o.usage;
                st.total_usage.add(&o.usage);
            }
        }
        self.deliver(result).await;
        let has_pending = {
            let mut st = self.state();
            st.phase = Phase::Idle;
            !st.pending.is_empty()
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
            StopReason::Done | StopReason::Error => {}
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
        let wanted: Vec<&str> = c.persona.tools.iter().map(String::as_str).filter(|n| self.tools.get(n).is_some()).collect();
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
                format!("已开启新会话 #{id}（记忆与关系不变；旧会话 #{old} 已结束，{} 天后可被清理）", c.agent.gc_days)
            }
            Command::Sessions => self.sessions_report().await?,
            Command::Gc(days) => self.gc(days.unwrap_or(c.agent.gc_days)).await?,
            Command::Stop => {
                if self.stop_current() {
                    "已停止".into()
                } else {
                    "当前没有进行中的回复".into()
                }
            }
            Command::Compact => {
                if self.state().phase != Phase::Idle {
                    return Ok("正在回复中，稍后再压缩".into());
                }
                let s = self.store.session(self.session()).await?;
                let mut spec = self.persona_spec(&c, &s, "").await?;
                spec.tail = None;
                let ctx = qya_agent::context::build(&self.store.branch(s.id).await?)?;
                let est = qya_agent::context::estimate_messages(&ctx);
                if self.kernel.compact(&spec, s.id, &ctx, est, &CancellationToken::new()).await? {
                    format!("已压缩（压缩前约 {est} token）")
                } else {
                    "内容太少，无需压缩".into()
                }
            }
            Command::Model(None) => {
                let s = self.store.session(self.session()).await?;
                let p = c.provider(&c.persona.provider).context("persona provider 未配置")?;
                let list: Vec<_> = p.models.keys().map(|m| if *m == s.model { format!("*{m}") } else { m.clone() }).collect();
                format!("当前模型：{}\n可选：{}", s.model, list.join(" "))
            }
            Command::Model(Some(m)) => {
                model_info(&c, &m)?;
                self.store.update_session(self.session(), Some(m.clone()), None, None).await?;
                format!("唐语歌模型已切换为 {m}")
            }
            Command::Effort(None) => {
                let s = self.store.session(self.session()).await?;
                format!("当前思考等级：{}（可选 low/medium/high/xhigh/max）", s.effort)
            }
            Command::Effort(Some(e)) => {
                let e: Effort = e.parse().map_err(anyhow::Error::msg)?;
                self.store.update_session(self.session(), None, Some(e.to_string()), None).await?;
                let p = c.provider(&c.persona.provider).context("persona provider 未配置")?;
                format!("思考等级已切换为 {e}（实际请求参数 {}）", p.effort_param(e))
            }
            Command::Learn(cmd) => self.learn_command(cmd).await?,
            Command::Agents => qya_orchestra::render_list(&self.sup.list().await),
            Command::Provider(None) => qya_plugins::provider_cmd::list(&self.cfg),
            Command::Provider(Some((cat, field, value))) => {
                qya_plugins::provider_cmd::apply(&self.cfg, &cat, &field, &value).unwrap_or_else(|e| e)
            }
            Command::Agent { id, action } => match action {
                AgentAction::Show => self.agent_detail(id).await?,
                AgentAction::Stop => self.sup.stop(id).unwrap_or_else(|e| e),
                AgentAction::Say(t) => self.sup.continue_worker(id, &t).await.unwrap_or_else(|e| e),
            },
            Command::Reload => {
                let c = self.cfg.reload()?;
                let kit = PersonaKit::load(&c, &self.sup)?;
                *self.kit.lock().expect("kit 锁中毒") = Arc::new(kit);
                "配置与人格资产已重新加载（记忆快照在新会话或压缩后生效）".into()
            }
        })
    }

    async fn learning_enabled(&self) -> Result<bool> {
        Ok(self.cfg.get().persona.learner.enabled && self.store.kv_get(KV_LEARN_ENABLED).await?.as_deref() != Some("off"))
    }

    /// 后台学习循环：每分钟检查一次是否达到触发条件。
    fn spawn_learner(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                match self.learn_once(false).await {
                    Ok(Some(msg)) => tracing::info!(%msg, "表达学习完成"),
                    Ok(None) => {}
                    Err(e) => tracing::warn!(error = %e, "表达学习失败"),
                }
            }
        });
    }

    /// 执行一次学习。`force` 时忽略开关与阈值（/learn now）。未执行返回 `None`。
    async fn learn_once(&self, force: bool) -> Result<Option<String>> {
        let lc = self.cfg.get().persona.learner.clone();
        if !force && !self.learning_enabled().await? {
            return Ok(None);
        }
        let (session, phase, idle) = {
            let st = self.state();
            (st.session, st.phase, st.last_activity.map(|t| t.elapsed()))
        };
        let ckpt: i64 = self.store.kv_get(&kv_learn_ckpt(session)).await?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let count = self.store.count_user_messages_since(session, ckpt).await?;
        if !force {
            let idle_ok =
                idle.is_some_and(|d| d >= Duration::from_secs(lc.idle_minutes * 60)) && count >= i64::from(lc.min_messages);
            // 只在空闲时学习，不与回复抢占
            if phase != Phase::Idle || !(count >= i64::from(lc.batch_messages) || idle_ok) {
                return Ok(None);
            }
        }
        if count == 0 {
            return Ok(force.then(|| "没有新的消息可学习".to_owned()));
        }
        if self.learning.swap(true, Ordering::AcqRel) {
            return Ok(force.then(|| "正在学习中".to_owned()));
        }
        let result = self.learn_batch(session, ckpt, &lc).await;
        self.learning.store(false, Ordering::Release);
        result.map(Some)
    }

    async fn learn_batch(&self, session: SessionId, ckpt: i64, lc: &qya_config::LearnerSettings) -> Result<String> {
        let c = self.cfg.get();
        let entries = self.store.messages_since(session, ckpt, 200).await?;
        let last = entries.last().map(|e| e.id).unwrap_or(ckpt);
        let msgs: Vec<Message> = entries.into_iter().filter_map(|e| serde_json::from_value(e.payload).ok()).collect();
        let kit = self.kit();
        let text = learner::transcript(&msgs, kit.persona.user_name(), "唐语歌");
        let pcfg = c.provider(&c.persona.provider).context("persona provider 未配置")?;
        let provider = qya_llm::build(pcfg)?;
        let items = learner::learn(provider.as_ref(), &lc.model, lc.effort, &text, CancellationToken::new()).await?;
        let n = items.len();
        let (added, reinforced, removed) = self.store.learn_expressions(items, lc.max_items).await?;
        self.store.kv_set(&kv_learn_ckpt(session), &last.to_string()).await?;
        self.rebuild_index().await?;
        Ok(format!("学习了 {} 条新消息：提取 {n} 条，新增 {added}，强化 {reinforced}，淘汰 {removed}", msgs.len()))
    }

    async fn rebuild_index(&self) -> Result<()> {
        let index = Arc::new(Index::build(self.store.expressions().await?));
        *self.index.lock().expect("index 锁中毒") = index;
        Ok(())
    }

    async fn learn_command(&self, cmd: LearnCmd) -> Result<String> {
        Ok(match cmd {
            LearnCmd::List => {
                let all = self.store.expressions().await?;
                let on = if self.learning_enabled().await? { "开启" } else { "关闭" };
                if all.is_empty() {
                    format!("表达学习：{on}；还没有学到任何说法")
                } else {
                    let mut s = format!("表达学习：{on}；共 {} 条（按权重，最多显示 20 条）\n", all.len());
                    for e in all.iter().take(20) {
                        let kind = if e.kind == "habit" { "习惯" } else { "黑话" };
                        s.push_str(&format!(
                            "#{} [{kind}] {} — {}（权重 {:.1}）\n",
                            e.id,
                            e.text,
                            e.meaning.as_deref().unwrap_or("-"),
                            e.weight
                        ));
                    }
                    s.trim_end().to_owned()
                }
            }
            LearnCmd::Forget(id) => {
                if self.store.forget_expression(id).await? {
                    self.rebuild_index().await?;
                    format!("已删除 #{id}")
                } else {
                    format!("没有 #{id}")
                }
            }
            LearnCmd::On => {
                self.store.kv_set(KV_LEARN_ENABLED, "on").await?;
                if self.cfg.get().persona.learner.enabled {
                    "表达学习已开启".into()
                } else {
                    "已记录开启，但配置 persona.learner.enabled=false，仍不会自动学习".into()
                }
            }
            LearnCmd::Off => {
                self.store.kv_set(KV_LEARN_ENABLED, "off").await?;
                "表达学习已关闭（已学到的说法仍会注入，可用 /learn forget 删除）".into()
            }
            LearnCmd::Now => self.learn_once(true).await?.unwrap_or_default(),
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
        Ok(format!(
            "qya {} · 会话 #{} · 模型 {} · 思考 {}\n对话上下文 ≈{} / {} token（{:.0}%，不含系统段）\n上一轮：输入 {} · 缓存命中 {:.0}% · 输出 {}\n累计：输入 {} · 缓存命中 {:.0}%\n状态：{} · 待合并 {} · 后台进行中 {}\n内存 {} MB · 运行 {}h{}m",
            crate::VERSION,
            s.id,
            s.model,
            s.effort,
            est,
            info.context_length,
            est as f64 * 100.0 / info.context_length as f64,
            last.prompt_tokens,
            hit(&last),
            last.completion_tokens,
            total.prompt_tokens,
            hit(&total),
            phase,
            pending,
            workers,
            rss,
            up / 3600,
            up % 3600 / 60
        ))
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

    pub async fn shutdown(&self) {
        self.stop_current();
        self.sup.stop_all();
    }

    /// /agent <id>：基本信息 + 该执行者最近一条回复。
    async fn agent_detail(&self, id: SessionId) -> Result<String> {
        let row = self.store.session(id).await.with_context(|| format!("没有 #{id}"))?;
        if row.kind == SessionKind::Persona {
            anyhow::bail!("#{id} 是唐语歌的会话，不是后台任务");
        }
        let info = self.sup.list().await.into_iter().find(|w| w.id == id);
        let status = info.as_ref().map(|w| w.status.label()).unwrap_or("已归档");
        let mut s = format!("#{id} {} · {status} · {}/{}", row.name.unwrap_or_default(), row.model, row.effort);
        if let Some(p) = info.and_then(|w| w.progress) {
            s.push_str(&format!("\n进度：{p}"));
        }
        let msgs = self.store.messages_since(id, 0, 50).await?;
        let last = msgs
            .iter()
            .rev()
            .filter_map(|e| serde_json::from_value::<Message>(e.payload.clone()).ok())
            .find(|m| m.role == Role::Assistant && !m.text().trim().is_empty());
        if let Some(m) = last {
            let t: String = m.text().chars().take(800).collect();
            s.push_str(&format!("\n最近回复：\n{t}"));
        }
        Ok(s)
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
    format!(
        "〔后台结果：{who}{}，用时 {} 秒〕\n{}\n〔这是后台交回的结果，不是阿颜的新消息。用你自己的话把要点告诉阿颜；没做成就如实说。〕",
        r.status.label(),
        r.elapsed.as_secs(),
        r.summary
    )
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

/// 递归删除目录下修改时间早于 `max_age` 的普通文件（不跟随符号链接），返回删除数量。
fn gc_dir(dir: &std::path::Path, max_age: Duration) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for e in rd.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else { continue };
        if meta.is_dir() {
            n += gc_dir(&e.path(), max_age);
        } else if meta.is_file() && meta.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|age| age > max_age) && std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

/// 给阿颜看的错误说明：只说原因类别，原始响应只写日志。
fn friendly_error(e: &anyhow::Error) -> String {
    use qya_llm::ErrorKind as K;
    let kind = e.chain().find_map(|c| match c.downcast_ref::<qya_agent::AgentError>() {
        Some(qya_agent::AgentError::Llm(l)) => Some(l.kind),
        _ => c.downcast_ref::<qya_llm::LlmError>().map(|l| l.kind),
    });
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
    #[test]
    fn gc_removes_only_old_files() {
        let d = std::env::temp_dir().join(format!("qya-gc-{}", std::process::id()));
        std::fs::create_dir_all(d.join("inbox")).unwrap();
        std::fs::write(d.join("inbox/new.jpg"), b"x").unwrap();
        assert_eq!(super::gc_dir(&d, std::time::Duration::from_secs(3600)), 0);
        assert_eq!(super::gc_dir(&d, std::time::Duration::ZERO), 1);
        assert!(!d.join("inbox/new.jpg").exists());
    }
}
