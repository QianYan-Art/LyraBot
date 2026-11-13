//! pi 式内核循环：请求模型 → 执行工具 → 回填结果 → 直到模型不再调用工具。
//!
//! 内核不知道主对话角色，也不知道平台；人格、编排都通过 [`TurnSpec`] 注入提示词与工具。

use crate::context::{self, Compaction, CtxMessage, FOLDED_TOOL, Fold};
use crate::tool::{Concurrency, ToolCtx, ToolOutput, Toolset};
use futures_util::future::join_all;
use lyrabot_core::{AgentEvent, Effort, Message, Role, SessionId, StopReason, ToolCall, Usage};
use lyrabot_llm::{ChatRequest, ErrorKind, LlmError, Provider, StreamEvent};
use lyrabot_store::{EntryKind, Store};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("模型：{0}")]
    Llm(#[from] LlmError),
    #[error("存储：{0}")]
    Store(#[from] lyrabot_store::StoreError),
    #[error("数据：{0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, AgentError>;

#[derive(Debug, Clone)]
pub struct KernelConfig {
    pub max_tool_calls: u32,
}

impl From<&lyrabot_config::AgentConfig> for KernelConfig {
    fn from(c: &lyrabot_config::AgentConfig) -> Self {
        Self { max_tool_calls: c.max_tool_calls }
    }
}

/// 压缩稿风格：主对话角色关心聊天的连续性，后台执行者关心任务状态，两者要保留的信息不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactStyle {
    Chat,
    Task,
}

/// 会话级压缩策略，由主对话角色与后台执行者各自给出。
#[derive(Clone)]
pub struct CompactPolicy {
    /// 上下文占窗口的比例达到该值时自动压缩
    pub ratio: f64,
    /// 压缩时保留的最近消息估算 token 数
    pub keep_tokens: u64,
    pub style: CompactStyle,
    /// 压缩稿中给消息标注时间（落库毫秒时间戳 → 文本，如按用户时区格式化）；`None` 不标注
    pub time_label: Option<Arc<dyn Fn(i64) -> String + Send + Sync>>,
}

impl Default for CompactPolicy {
    fn default() -> Self {
        Self { ratio: 0.9, keep_tokens: 20_000, style: CompactStyle::Task, time_label: None }
    }
}

/// 一次压缩的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactOutcome {
    /// 生成了摘要并写入 compaction 条目
    Summarized,
    /// 只把较早的工具结果折叠成占位，未调用模型
    Folded,
    /// 没有可压缩的内容：切分点前只剩上次的摘要，或全部内容都在保留尾部内
    NothingToCompact,
    /// 调用了压缩模型，但输出解析后摘要为空
    EmptySummary,
}

/// 自动压缩没压成后，上下文增长到失败时的这一倍数之前不再自动重试，避免每轮都白调一次模型。
const COMPACT_RETRY_GROWTH: f64 = 1.1;

/// 一轮对话所需的全部"人格/编排"输入。
#[derive(Clone)]
pub struct TurnSpec {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub effort: Option<Effort>,
    pub context_window: u64,
    pub max_output_tokens: u32,
    /// 系统段（稳定在前）
    pub system: Vec<String>,
    pub tools: Toolset,
    /// 本轮附加在最后一条 user 输入末尾的动态内容，随该消息落库（见 [`Message::tail`]）。
    pub tail: Option<String>,
    pub compaction: CompactPolicy,
}

/// 会话级消息队列（与 pi 语义一致）。
#[derive(Default)]
pub struct Queues {
    /// 工具执行间隙插入，立即影响当前轮
    steer: Mutex<VecDeque<Message>>,
    /// 当前轮结束后再处理
    follow_up: Mutex<VecDeque<Message>>,
}

impl Queues {
    pub fn steer(&self, m: Message) {
        self.steer.lock().expect("steer 锁中毒").push_back(m);
    }
    pub fn follow_up(&self, m: Message) {
        self.follow_up.lock().expect("follow_up 锁中毒").push_back(m);
    }
    fn drain(q: &Mutex<VecDeque<Message>>) -> Vec<Message> {
        q.lock().expect("队列锁中毒").drain(..).collect()
    }
    /// 取出两个队列中尚未处理的全部消息（插话在前）。用于被打断的一轮把它们并回缓冲。
    pub fn take_all(&self) -> Vec<Message> {
        let mut v = Self::drain(&self.steer);
        v.extend(Self::drain(&self.follow_up));
        v
    }
    pub fn pending(&self) -> usize {
        self.steer.lock().map(|q| q.len()).unwrap_or(0) + self.follow_up.lock().map(|q| q.len()).unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub reason: StopReason,
    /// 本轮最后一条 assistant 文本
    pub text: String,
    pub usage: Usage,
    /// 工具产出的待投递媒体
    pub media: Vec<std::path::PathBuf>,
    /// 尚未得到任何模型回应就被取消的输入（未落库），由调用方决定是否重新排队
    pub unconsumed: Vec<Message>,
    /// 本轮开始时传入的输入是否已经落库（得到过模型回应，或模型出错时为保留记录而写入）
    pub inputs_saved: bool,
    /// `reason == Error` 时的模型错误；存储等内部错误仍以 `Err` 返回
    pub error: Option<LlmError>,
}

#[derive(Clone)]
pub struct Kernel {
    store: Store,
    events: broadcast::Sender<AgentEvent>,
    cfg: KernelConfig,
    /// 各会话最近一次完成模型请求的时刻（用于计算并记录请求间隔）。
    last_req: Arc<Mutex<BTreeMap<SessionId, Instant>>>,
    /// 各会话最近一次自动压缩没压成时的估算 token 数（退避用）
    compact_backoff: Arc<Mutex<BTreeMap<SessionId, u64>>>,
}

impl Kernel {
    pub fn new(store: Store, cfg: KernelConfig) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self { store, events, cfg, last_req: Arc::default(), compact_backoff: Arc::default() }
    }

    /// 返回距该会话上次模型请求完成的空闲时长，并顺带清理超过 24 小时的陈旧记录。
    pub fn session_idle(&self, session: SessionId) -> Option<Duration> {
        let now = Instant::now();
        let mut map = self.last_req.lock().expect("last_req 锁中毒");
        map.retain(|_, t| now.saturating_duration_since(*t) < Duration::from_secs(86_400));
        map.get(&session).map(|prev| now.saturating_duration_since(*prev))
    }

    /// 记录会话刚刚完成一次模型请求。
    pub fn touch_session(&self, session: SessionId) {
        let now = Instant::now();
        let mut map = self.last_req.lock().expect("last_req 锁中毒");
        map.retain(|_, t| now.saturating_duration_since(*t) < Duration::from_secs(86_400));
        map.insert(session, now);
    }

    /// 查询该会话最近一次完成模型请求的时刻。
    pub fn session_last_req(&self, session: SessionId) -> Option<Instant> {
        self.last_req.lock().expect("last_req 锁中毒").get(&session).copied()
    }

    /// 清理已删除会话的请求时间记录。
    pub fn forget_sessions(&self, ids: &[SessionId]) {
        let mut map = self.last_req.lock().expect("last_req 锁中毒");
        let mut backoff = self.compact_backoff.lock().expect("compact_backoff 锁中毒");
        for id in ids {
            map.remove(id);
            backoff.remove(id);
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    fn emit(&self, ev: AgentEvent) {
        let _ = self.events.send(ev);
    }

    /// 直接把消息写入会话（例如把被取消一轮的未处理输入落库，保留任务记录）。
    pub async fn append_messages(&self, session: SessionId, msgs: &[Message]) -> Result<()> {
        for m in msgs {
            self.persist(session, m).await?;
        }
        Ok(())
    }

    async fn persist(&self, session: SessionId, m: &Message) -> Result<()> {
        let tokens = context::estimate_tokens(m.char_len()) as i64;
        self.store.append(session, EntryKind::Message, serde_json::to_value(m)?, tokens).await?;
        Ok(())
    }

    /// 运行一轮：循环直到模型停止、被取消或触发防护。
    ///
    /// 输入（以及轮中到达的插话/追问）只在模型给出回应时才与回应一起落库；
    /// 若在那之前被取消，这些输入原样通过 `TurnOutcome::unconsumed` 交回，不会被当成“已处理”。
    pub async fn run_turn(
        &self,
        spec: &TurnSpec,
        session: SessionId,
        input: Vec<Message>,
        queues: &Queues,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome> {
        self.emit(AgentEvent::TurnStart { session });
        let mut inputs_saved = input.is_empty();
        let mut unpersisted = input;
        // 尾注入挂在本轮最后一条 user 输入上并随它落库（被取消后重新排队的输入先清掉旧的）；
        // 本轮没有 user 输入时退回为只在请求里临时附加
        let mut transient_tail = None;
        if let Some(t) = spec.tail.as_deref().filter(|t| !t.is_empty()) {
            for m in &mut unpersisted {
                m.tail = None;
            }
            match unpersisted.iter_mut().rev().find(|m| m.role == Role::User) {
                Some(m) => m.tail = Some(t.to_owned()),
                None => transient_tail = Some(t),
            }
        }
        let mut usage = Usage::default();
        let mut media = Vec::new();
        let mut last_text = String::new();
        let mut total_calls = 0u32;
        let mut last_sig: Option<String> = None;
        let mut repeats = 0u32;
        let mut last_prompt_tokens = 0u64;
        let mut error = None;

        let outcome_reason = loop {
            if cancel.is_cancelled() {
                break StopReason::Cancelled;
            }
            let mut ctx = self.context(session).await?;
            let pending_est: u64 = unpersisted.iter().map(|m| context::estimate_tokens(m.char_len())).sum();
            let mut est = context::estimate_messages(&ctx).max(last_prompt_tokens) + pending_est;
            if (est as f64) >= spec.context_window as f64 * spec.compaction.ratio && !self.compact_backed_off(session, est) {
                let result = self.compact(spec, session, &ctx, est, &cancel).await;
                // 压缩期间被取消：与生成被打断同样处理，未落库的输入经 unconsumed 交回
                if cancel.is_cancelled() {
                    break StopReason::Cancelled;
                }
                match result {
                    Ok(CompactOutcome::Summarized | CompactOutcome::Folded) => {
                        ctx = self.context(session).await?;
                        est = context::estimate_messages(&ctx) + pending_est;
                    }
                    Ok(outcome) => {
                        self.compact_backoff.lock().expect("compact_backoff 锁中毒").insert(session, est);
                        tracing::warn!(session, ?outcome, est, "自动压缩没压成，上下文再增长约 10% 之前不再重试");
                    }
                    // 压缩本身失败不终止本轮，按原上下文继续（超长时由模型请求报错并保存输入）
                    Err(e) => tracing::warn!(session, error = %e, "会话压缩失败，本轮跳过压缩"),
                }
            }
            // 后台任务的最终上下文仍超出窗口（压不下来、处于退避期、或保留尾部本身就超窗）：明确停止并汇报，
            // 不带着超长上下文去请求。退避只控制是否重试压缩，不绕过这道检查。
            if spec.compaction.style == CompactStyle::Task && est >= spec.context_window {
                for m in unpersisted.drain(..) {
                    self.persist(session, &m).await?;
                }
                inputs_saved = true;
                error = Some(LlmError::new(ErrorKind::ContextOverflow, "上下文已超出窗口且无法再压缩，任务停止"));
                break StopReason::Error;
            }
            ctx.extend(unpersisted.iter().cloned().map(|message| CtxMessage { entry: None, at: None, message }));

            let req = ChatRequest {
                model: spec.model.clone(),
                system: spec.system.clone(),
                messages: wire_messages(ctx, transient_tail),
                tools: spec.tools.specs(),
                effort: spec.effort,
                max_tokens: Some(spec.max_output_tokens),
                cache_key: Some(format!("lyrabot-s-{session}")),
            };
            let idle_s = self.session_idle(session).map(|d| d.as_secs());
            let completion = match self.stream_once(spec, session, req, &cancel).await {
                Ok(c) => c,
                Err(e) if e.kind == ErrorKind::Cancelled => break StopReason::Cancelled,
                Err(e) => {
                    // 出错时保留输入，避免对话记录丢失；错误随结果交回，调用方据 inputs_saved 判断输入去向
                    for m in unpersisted.drain(..) {
                        self.persist(session, &m).await?;
                    }
                    inputs_saved = true;
                    error = Some(e);
                    break StopReason::Error;
                }
            };
            self.touch_session(session);
            let u = &completion.usage;
            let hit_pct = (u.cached_tokens * 100).checked_div(u.prompt_tokens).unwrap_or(0);
            tracing::info!(
                session,
                model = %spec.model,
                prompt_tokens = u.prompt_tokens,
                cached_tokens = u.cached_tokens,
                hit_pct = %format!("{hit_pct}%"),
                completion_tokens = u.completion_tokens,
                idle_s,
                "模型请求完成"
            );
            usage.add(&completion.usage);
            last_prompt_tokens = completion.usage.prompt_tokens + completion.usage.completion_tokens;
            self.emit(AgentEvent::Usage { session, usage: completion.usage });
            let assistant = completion.message;
            for m in unpersisted.drain(..) {
                self.persist(session, &m).await?;
            }
            inputs_saved = true;
            self.persist(session, &assistant).await?;
            let text = assistant.text();
            if !text.is_empty() {
                last_text = text;
            }

            if assistant.tool_calls.is_empty() {
                // 工具阶段之后才到达的插话没有机会被注入，转为追问一并处理
                let mut more = Queues::drain(&queues.steer);
                more.extend(Queues::drain(&queues.follow_up));
                if more.is_empty() {
                    break StopReason::Done;
                }
                unpersisted.extend(more);
                continue;
            }

            // 工具循环防护：同一批调用签名连续重复
            let sig: String = assistant.tool_calls.iter().map(|c| format!("{}:{}", c.name, c.arguments)).collect::<Vec<_>>().join("|");
            repeats = if last_sig.as_deref() == Some(sig.as_str()) { repeats + 1 } else { 1 };
            last_sig = Some(sig);
            // 执行前按剩余预算截断：超出部分不执行，但仍补上结果以保持工具消息结构完整
            let budget = self.cfg.max_tool_calls.saturating_sub(total_calls) as usize;
            let run_n = assistant.tool_calls.len().min(budget);
            total_calls += assistant.tool_calls.len() as u32;

            let mut results = self.exec_tools(spec, session, &assistant.tool_calls[..run_n], &cancel).await;
            results.extend((run_n..assistant.tool_calls.len()).map(|_| ToolOutput::err("未执行：超过本轮工具调用上限")));
            for (call, out) in assistant.tool_calls.iter().zip(results) {
                media.extend(out.media);
                self.persist(session, &Message::tool_result(&call.id, out.text, out.is_error)).await?;
            }
            if cancel.is_cancelled() {
                break StopReason::Cancelled;
            }
            if repeats >= 3 || total_calls >= self.cfg.max_tool_calls {
                tracing::warn!(session, repeats, total_calls, "触发工具循环防护");
                break StopReason::LoopGuard;
            }
            unpersisted.extend(Queues::drain(&queues.steer));
        };

        let end_text = error.as_ref().map_or_else(|| last_text.clone(), ToString::to_string);
        self.emit(AgentEvent::TurnEnd { session, reason: outcome_reason, text: end_text });
        Ok(TurnOutcome { reason: outcome_reason, text: last_text, usage, media, unconsumed: unpersisted, inputs_saved, error })
    }

    /// 读取会话在最近一次压缩点之后的有效上下文。
    pub async fn context(&self, session: SessionId) -> Result<Vec<CtxMessage>> {
        let branch = self.store.context_branch(session).await?;
        Ok(context::build(&branch)?)
    }

    async fn stream_once(
        &self,
        spec: &TurnSpec,
        session: SessionId,
        req: ChatRequest,
        cancel: &CancellationToken,
    ) -> std::result::Result<lyrabot_llm::Completion, LlmError> {
        let mut rx = spec.provider.stream(req, cancel.clone()).await?;
        while let Some(ev) = rx.recv().await {
            match ev? {
                StreamEvent::TextDelta(text) => self.emit(AgentEvent::TextDelta { session, text }),
                StreamEvent::ReasoningDelta(text) => self.emit(AgentEvent::ReasoningDelta { session, text }),
                StreamEvent::Completed(c) => return Ok(c),
            }
        }
        Err(LlmError::new(ErrorKind::Transient, "流提前结束"))
    }

    /// 连续的 Shared 工具并行执行，Exclusive 工具独占串行；结果顺序与调用顺序一致。
    async fn exec_tools(&self, spec: &TurnSpec, session: SessionId, calls: &[ToolCall], cancel: &CancellationToken) -> Vec<ToolOutput> {
        let mut out: Vec<Option<ToolOutput>> = vec![None; calls.len()];
        let mut i = 0;
        while i < calls.len() {
            let shared = |c: &ToolCall| spec.tools.get(&c.name).is_some_and(|t| t.concurrency() == Concurrency::Shared);
            let end = if shared(&calls[i]) {
                i + calls[i..].iter().take_while(|c| shared(c)).count()
            } else {
                i + 1
            };
            let batch = join_all(calls[i..end].iter().map(|c| self.exec_one(spec, session, c, cancel))).await;
            for (k, r) in batch.into_iter().enumerate() {
                out[i + k] = Some(r);
            }
            i = end;
        }
        out.into_iter().map(|o| o.unwrap_or_else(|| ToolOutput::err("内部错误：缺少工具结果"))).collect()
    }

    async fn exec_one(&self, spec: &TurnSpec, session: SessionId, call: &ToolCall, cancel: &CancellationToken) -> ToolOutput {
        if cancel.is_cancelled() {
            return ToolOutput::err("已取消：未执行");
        }
        let Some(tool) = spec.tools.get(&call.name) else {
            return ToolOutput::err(format!("没有名为 `{}` 的工具", call.name));
        };
        let args: serde_json::Value = if call.arguments.trim().is_empty() {
            serde_json::Value::Object(Default::default())
        } else {
            match serde_json::from_str(&call.arguments) {
                Ok(v) => v,
                Err(e) => return ToolOutput::err(format!("参数不是合法 JSON：{e}")),
            }
        };
        self.emit(AgentEvent::ToolStart { session, call_id: call.id.clone(), name: call.name.clone() });
        let started = Instant::now();
        let ctx = ToolCtx { session, call_id: call.id.clone(), cancel: cancel.child_token() };
        let fut = tool.call(ctx, args);
        tokio::pin!(fut);
        let out = tokio::select! {
            o = &mut fut => o,
            // 工具持有子令牌会自行收尾（如终止进程组）；等它最多 2 秒，避免直接丢弃 future 跳过清理
            _ = cancel.cancelled() => match tokio::time::timeout(Duration::from_secs(2), &mut fut).await {
                Ok(o) => o,
                Err(_) => ToolOutput::err("已取消"),
            },
        };
        self.emit(AgentEvent::ToolEnd {
            session,
            call_id: call.id.clone(),
            name: call.name.clone(),
            is_error: out.is_error,
            ms: started.elapsed().as_millis() as u64,
        });
        out
    }

    /// 该会话是否处于压缩退避期：上次自动压缩没压成，且上下文尚未增长到当时的 [`COMPACT_RETRY_GROWTH`] 倍。
    fn compact_backed_off(&self, session: SessionId, est: u64) -> bool {
        self.compact_backoff.lock().expect("compact_backoff 锁中毒").get(&session).is_some_and(|&failed| (est as f64) < failed as f64 * COMPACT_RETRY_GROWTH)
    }

    /// 压缩：先把切分点之前的工具结果折叠成占位（不调模型），若已降到窗口的 [`FOLD_TARGET`] 以下就到此为止；
    /// 否则按会话的压缩风格对切分点之前的上下文做摘要，写入 compaction 条目。
    /// 压成（摘要或折叠）时解除该会话的退避；手动压缩不受退避限制。
    pub async fn compact(
        &self,
        spec: &TurnSpec,
        session: SessionId,
        ctx: &[CtxMessage],
        tokens_before: u64,
        cancel: &CancellationToken,
    ) -> Result<CompactOutcome> {
        let outcome = self.compact_inner(spec, session, ctx, tokens_before, cancel).await?;
        if matches!(outcome, CompactOutcome::Summarized | CompactOutcome::Folded) {
            self.compact_backoff.lock().expect("compact_backoff 锁中毒").remove(&session);
        }
        Ok(outcome)
    }

    async fn compact_inner(
        &self,
        spec: &TurnSpec,
        session: SessionId,
        ctx: &[CtxMessage],
        tokens_before: u64,
        cancel: &CancellationToken,
    ) -> Result<CompactOutcome> {
        let Some(split) = context::split_point(ctx, spec.compaction.keep_tokens) else {
            return Ok(CompactOutcome::NothingToCompact);
        };
        let first_kept = ctx[split].entry;
        if let Some(before) = first_kept {
            let stub = context::estimate_tokens(FOLDED_TOOL.chars().count());
            let saved: u64 = ctx[..split]
                .iter()
                // 只统计确实落库、编号在折叠线之前的工具结果；请求时临时补的占位不会被折叠，不能算作收益
                .filter(|m| m.message.role == Role::Tool && m.entry.is_some_and(|id| id < before) && m.message.text() != FOLDED_TOOL)
                .map(|m| context::estimate_tokens(m.message.char_len()).saturating_sub(stub))
                .sum();
            if saved > 0 && (tokens_before.saturating_sub(saved) as f64) < spec.context_window as f64 * FOLD_TARGET {
                self.store.append(session, EntryKind::Fold, serde_json::to_value(Fold { before })?, 0).await?;
                self.emit(AgentEvent::Compacted { session, before_tokens: tokens_before, summarized: false });
                tracing::info!(session, tokens_before, saved, "已折叠较早的工具结果");
                return Ok(CompactOutcome::Folded);
            }
        }
        let transcript = context::transcript(&ctx[..split], &spec.compaction);
        let prompt = match spec.compaction.style {
            CompactStyle::Chat => COMPACT_PROMPT_CHAT,
            CompactStyle::Task => COMPACT_PROMPT_TASK,
        };
        let req = ChatRequest {
            model: spec.model.clone(),
            system: vec![prompt.to_owned()],
            messages: vec![Message::user(transcript)],
            tools: Vec::new(),
            effort: Some(Effort::Low),
            max_tokens: Some(6000),
            cache_key: None,
        };
        let c = spec.provider.complete(req, cancel.clone()).await?;
        let summary = context::render_compaction(&c.message.text());
        if summary.trim().is_empty() {
            tracing::warn!(session, "压缩摘要为空，跳过");
            return Ok(CompactOutcome::EmptySummary);
        }
        let payload = Compaction { summary, first_kept, tokens_before };
        self.store.append(session, EntryKind::Compaction, serde_json::to_value(&payload)?, 0).await?;
        self.emit(AgentEvent::Compacted { session, before_tokens: tokens_before, summarized: true });
        tracing::info!(session, tokens_before, "会话已压缩");
        Ok(CompactOutcome::Summarized)
    }
}

/// 折叠后需降到窗口的这一比例以下才视为够用，否则直接做摘要（避免刚折叠又触发压缩）。
const FOLD_TARGET: f64 = 0.6;

/// 主对话角色（聊天）的压缩提示：保住聊天的连续性与时间线，任务胶囊只在确有进行中的事时写。
const COMPACT_PROMPT_CHAT: &str = "你负责压缩一段私聊记录，供同一个聊天角色继续和对方聊下去。记录里的〔〕内容是系统信息（时间、后台结果等）。\
先在心里逐条清点：对方说过的事实与近况、双方的约定与要提醒的事、称呼与偏好、对方纠正过的地方、还没转述完的后台结果、日期与数字——这些必须全部写进结果，不许合并丢弃。\
记录中标了时间的，摘要里保留具体日期或时间（如“11 月 13 日晚上说……”），不要改写成“刚才”“最近”。\
只输出一个 JSON 对象，不要代码块或解释：\
{\"summary\": \"分点摘要，按时间先后，保留上面清点的内容与双方关系、情绪的进展，不超过 800 字\", \
\"capsule\": {\"objective\": \"进行中的事\", \"constraints\": [\"对方明确提出的要求或禁忌\"], \"decisions\": [\"已确认的决定\"], \
\"verification\": [\"已确认或尚未确认的事项\"], \"next_steps\": [\"接下来要做或要提醒的事\"]}}。\
没有进行中的事时 capsule 为 null；某项没有内容就给空数组。不要编造，不要写客套话，不要模仿角色口吻。";

/// 后台执行者（任务）的压缩提示：保住任务状态、精确细节与已执行的外部操作。
const COMPACT_PROMPT_TASK: &str = "你负责压缩一个后台任务的执行记录，供同一个执行者继续完成任务。\
先在心里逐条清点：精确的文件路径、命令、报错原文、编号与链接、数字与日期、用户的要求与禁忌、已确认的决定——这些必须原样写进结果，不许合并丢弃。\
特别列出已经执行过的、对外部产生影响的操作（发信、部署、写文件、调接口、改配置等）及其结果，续接时据此避免重复执行。\
只输出一个 JSON 对象，不要代码块或解释：\
{\"summary\": \"分点摘要，保留任务进度与上面清点的关键事实，不超过 800 字\", \
\"capsule\": {\"objective\": \"任务目标\", \"constraints\": [\"用户明确提出的要求或禁忌\"], \"decisions\": [\"已确认的决定\"], \
\"side_effects\": [\"已执行的外部操作及结果\"], \"verification\": [\"已验证或未验证的事项及结论\"], \"next_steps\": [\"接下来要做的事\"]}}。\
某项没有内容就给空数组。不要编造，不要写客套话。";

/// 组装发给模型的消息：每条消息按 [`Message::wire`] 拼回落库的尾注入；
/// `transient` 是本轮没有 user 输入时临时附在最后一条 user 消息上的尾注入（不落库）。
/// 正式请求与保活共用，保证两者的前缀逐字一致。
pub fn wire_messages(mut ctx: Vec<CtxMessage>, transient: Option<&str>) -> Vec<Message> {
    if let Some(t) = transient
        && let Some(c) = ctx.iter_mut().rev().find(|c| c.message.role == Role::User)
    {
        // 只改这次请求里的副本，不影响已落库的内容
        c.message.tail = Some(t.to_owned());
    }
    ctx.into_iter().map(|c| c.message.wire()).collect()
}
