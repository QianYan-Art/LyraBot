//! pi 式内核循环：请求模型 → 执行工具 → 回填结果 → 直到模型不再调用工具。
//!
//! 内核不知道唐语歌，也不知道平台；人格、编排都通过 [`TurnSpec`] 注入提示词与工具。

use crate::context::{self, Compaction, CtxMessage, FOLDED_TOOL, Fold};
use crate::tool::{Concurrency, ToolCtx, ToolOutput, Toolset};
use futures_util::future::join_all;
use qya_core::{AgentEvent, ContentPart, Effort, Message, Role, SessionId, StopReason, ToolCall, Usage};
use qya_llm::{ChatRequest, ErrorKind, LlmError, Provider, StreamEvent};
use qya_store::{EntryKind, Store};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("模型：{0}")]
    Llm(#[from] LlmError),
    #[error("存储：{0}")]
    Store(#[from] qya_store::StoreError),
    #[error("数据：{0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, AgentError>;

#[derive(Debug, Clone)]
pub struct KernelConfig {
    pub compaction_ratio: f64,
    pub compaction_keep_tokens: u64,
    pub max_tool_calls: u32,
}

impl From<&qya_config::AgentConfig> for KernelConfig {
    fn from(c: &qya_config::AgentConfig) -> Self {
        Self {
            compaction_ratio: c.compaction_ratio,
            compaction_keep_tokens: c.compaction_keep_tokens,
            max_tool_calls: c.max_tool_calls,
        }
    }
}

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
    /// 每次请求附加在最后一条 user 消息末尾的动态内容；不落库。
    pub tail: Option<String>,
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
}

impl Kernel {
    pub fn new(store: Store, cfg: KernelConfig) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self { store, events, cfg }
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
            let est = context::estimate_messages(&ctx).max(last_prompt_tokens)
                + unpersisted.iter().map(|m| context::estimate_tokens(m.char_len())).sum::<u64>();
            if (est as f64) >= spec.context_window as f64 * self.cfg.compaction_ratio {
                match self.compact(spec, session, &ctx, est, &cancel).await {
                    Ok(true) => ctx = self.context(session).await?,
                    Ok(false) => {}
                    // 压缩被打断：与生成被打断同样处理，未落库的输入经 unconsumed 交回
                    Err(_) if cancel.is_cancelled() => break StopReason::Cancelled,
                    // 压缩本身失败不终止本轮，按原上下文继续（超长时由模型请求报错并保存输入）
                    Err(e) => tracing::warn!(session, error = %e, "会话压缩失败，本轮跳过压缩"),
                }
            }
            ctx.extend(unpersisted.iter().cloned().map(|message| CtxMessage { entry: None, message }));

            let req = ChatRequest {
                model: spec.model.clone(),
                system: spec.system.clone(),
                messages: with_tail(ctx, spec.tail.as_deref()),
                tools: spec.tools.specs(),
                effort: spec.effort,
                max_tokens: Some(spec.max_output_tokens),
            };
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

    async fn context(&self, session: SessionId) -> Result<Vec<CtxMessage>> {
        let branch = self.store.branch(session).await?;
        Ok(context::build(&branch)?)
    }

    async fn stream_once(
        &self,
        spec: &TurnSpec,
        session: SessionId,
        req: ChatRequest,
        cancel: &CancellationToken,
    ) -> std::result::Result<qya_llm::Completion, LlmError> {
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

    /// 压缩：先把切分点之前的工具结果折叠成占位（不调模型），若已降到窗口的 [`FOLD_TARGET`] 以下就到此为止；
    /// 否则对切分点之前的上下文做枚举式摘要 + 任务胶囊，写入 compaction 条目。返回是否发生了压缩。
    pub async fn compact(
        &self,
        spec: &TurnSpec,
        session: SessionId,
        ctx: &[CtxMessage],
        tokens_before: u64,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        let Some(split) = context::split_point(ctx, self.cfg.compaction_keep_tokens) else {
            return Ok(false);
        };
        let first_kept = ctx[split].entry;
        if let Some(before) = first_kept {
            let stub = context::estimate_tokens(FOLDED_TOOL.chars().count());
            let saved: u64 = ctx[..split]
                .iter()
                .filter(|m| m.message.role == Role::Tool && m.message.text() != FOLDED_TOOL)
                .map(|m| context::estimate_tokens(m.message.char_len()).saturating_sub(stub))
                .sum();
            if saved > 0 && (tokens_before.saturating_sub(saved) as f64) < spec.context_window as f64 * FOLD_TARGET {
                self.store.append(session, EntryKind::Fold, serde_json::to_value(Fold { before })?, 0).await?;
                self.emit(AgentEvent::Compacted { session, before_tokens: tokens_before });
                tracing::info!(session, tokens_before, saved, "已折叠较早的工具结果");
                return Ok(true);
            }
        }
        let mut transcript = String::new();
        for m in &ctx[..split] {
            let who = match m.message.role {
                Role::User => "用户",
                Role::Assistant => "助手",
                Role::Tool => "工具结果",
            };
            let mut body = m.message.text();
            for c in &m.message.tool_calls {
                body.push_str(&format!("\n[调用 {} {}]", c.name, truncate(&c.arguments, 400)));
            }
            transcript.push_str(&format!("{who}：{}\n", truncate(&body, 4000)));
        }
        let req = ChatRequest {
            model: spec.model.clone(),
            system: vec![COMPACT_PROMPT.to_owned()],
            messages: vec![Message::user(transcript)],
            tools: Vec::new(),
            effort: Some(Effort::Low),
            max_tokens: Some(6000),
        };
        let c = spec.provider.complete(req, cancel.clone()).await?;
        let summary = context::render_compaction(&c.message.text());
        if summary.trim().is_empty() {
            tracing::warn!(session, "压缩摘要为空，跳过");
            return Ok(false);
        }
        let payload = Compaction { summary, first_kept, tokens_before };
        self.store.append(session, EntryKind::Compaction, serde_json::to_value(&payload)?, 0).await?;
        self.emit(AgentEvent::Compacted { session, before_tokens: tokens_before });
        tracing::info!(session, tokens_before, "会话已压缩");
        Ok(true)
    }
}

/// 折叠后需降到窗口的这一比例以下才视为够用，否则直接做摘要（避免刚折叠又触发压缩）。
const FOLD_TARGET: f64 = 0.6;

const COMPACT_PROMPT: &str = "你负责压缩对话历史，供同一个助手继续对话。\
先在心里逐条清点：日期与截止时间、数字与金额、人名与账号、已确认的决定、用户纠正过的地方——这些必须全部写进结果，不许合并丢弃。\
只输出一个 JSON 对象，不要代码块或解释：\
{\"summary\": \"分点摘要，保留未完成的任务与进度、用户的偏好与要求、做出的承诺和上面清点的关键事实，不超过 800 字\", \
\"capsule\": {\"objective\": \"当前进行中的任务目标\", \"constraints\": [\"用户明确提出的要求或禁忌\"], \"decisions\": [\"已确认的决定\"], \
\"verification\": [\"已验证或未验证的事项及结论\"], \"next_steps\": [\"接下来要做的事\"]}}。\
没有进行中的任务时 capsule 为 null；某项没有内容就给空数组。不要编造，不要写客套话。";

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(max_chars).collect();
        t.push('…');
        t
    }
}

/// 把动态尾注入附到最后一条 user 消息（仅本次请求，不落库）。
fn with_tail(ctx: Vec<CtxMessage>, tail: Option<&str>) -> Vec<Message> {
    let mut msgs: Vec<Message> = ctx.into_iter().map(|c| c.message).collect();
    if let Some(t) = tail.filter(|t| !t.is_empty())
        && let Some(m) = msgs.iter_mut().rev().find(|m| m.role == Role::User)
    {
        m.content.push(ContentPart::text(format!("\n\n{t}")));
    }
    msgs
}
