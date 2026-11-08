//! 内核行为测试：用脚本化的假 Provider 驱动，覆盖工具并发、steer、取消、循环防护、压缩、尾注入。
use async_trait::async_trait;
use qya_agent::context::SUMMARY_PREFIX;
use qya_agent::{Concurrency, Kernel, KernelConfig, Queues, Tool, ToolCtx, ToolOutput, ToolRegistry, TurnSpec};
use qya_core::{Message, Role, StopReason, ToolCall, ToolSpec, Usage};
use qya_llm::{ChatRequest, Completion, EventRx, FinishReason, LlmError, Provider, RemoteModel, StreamEvent};
use qya_store::{EntryKind, NewSession, Store};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Fake {
    script: Mutex<Vec<Message>>,
    requests: Mutex<Vec<ChatRequest>>,
}
impl Fake {
    fn new(mut script: Vec<Message>) -> Arc<Self> {
        script.reverse();
        Arc::new(Self { script: Mutex::new(script), requests: Mutex::default() })
    }
    fn reqs(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}
#[async_trait]
impl Provider for Fake {
    fn id(&self) -> &str {
        "fake"
    }
    async fn stream(&self, req: ChatRequest, _c: CancellationToken) -> Result<EventRx, LlmError> {
        self.requests.lock().unwrap().push(req);
        let m = self.script.lock().unwrap().pop().unwrap_or_else(|| Message::assistant("（脚本耗尽）"));
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let text = m.text();
        if !text.is_empty() {
            tx.send(Ok(StreamEvent::TextDelta(text))).await.unwrap();
        }
        let finish = if m.tool_calls.is_empty() { FinishReason::Stop } else { FinishReason::ToolCalls };
        let usage = Usage { prompt_tokens: 10, ..Default::default() };
        tx.send(Ok(StreamEvent::Completed(Completion { message: m, usage, finish }))).await.unwrap();
        Ok(rx)
    }
    async fn list_models(&self) -> Result<Vec<RemoteModel>, LlmError> {
        Ok(vec![])
    }
}

fn call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), arguments: args.into() }
}
fn calls(cs: Vec<ToolCall>) -> Message {
    let mut m = Message::assistant("");
    m.content.clear();
    m.tool_calls = cs;
    m
}

struct Sleepy {
    spec: ToolSpec,
    ms: u64,
    mode: Concurrency,
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    steer: Option<Arc<Queues>>,
}
impl Sleepy {
    fn new(name: &str, ms: u64, mode: Concurrency) -> Self {
        Self {
            spec: ToolSpec { name: name.into(), description: "t".into(), parameters: serde_json::json!({"type":"object"}) },
            ms,
            mode,
            live: Arc::default(),
            peak: Arc::default(),
            steer: None,
        }
    }
}
#[async_trait]
impl Tool for Sleepy {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self) -> Concurrency {
        self.mode
    }
    async fn call(&self, _ctx: ToolCtx, _args: serde_json::Value) -> ToolOutput {
        let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        if let Some(q) = &self.steer {
            q.steer(Message::user("插话：改用方案B"));
        }
        tokio::time::sleep(Duration::from_millis(self.ms)).await;
        self.live.fetch_sub(1, Ordering::SeqCst);
        ToolOutput::ok(format!("{} ok", self.spec.name))
    }
}

async fn setup(provider: Arc<Fake>, tools: Vec<Arc<dyn Tool>>, window: u64) -> (Kernel, TurnSpec, i64) {
    let store = Store::in_memory().unwrap();
    let sid = store.create_session(NewSession { model: "m".into(), effort: "low".into(), ..Default::default() }).await.unwrap();
    let kernel = Kernel::new(store, KernelConfig { compaction_ratio: 0.9, compaction_keep_tokens: 30, max_tool_calls: 40 });
    let mut reg = ToolRegistry::default();
    for t in tools {
        reg.register(t);
    }
    let spec = TurnSpec {
        provider,
        model: "m".into(),
        effort: None,
        context_window: window,
        max_output_tokens: 100,
        system: vec!["sys".into()],
        tools: reg.all(),
        tail: None,
    };
    (kernel, spec, sid)
}

fn go() -> Vec<Message> {
    vec![Message::user("go")]
}

#[tokio::test]
async fn shared_tools_run_in_parallel_and_results_ordered() {
    let a = Sleepy::new("a", 150, Concurrency::Shared);
    let peak = a.peak.clone();
    let b = Sleepy { live: a.live.clone(), peak: a.peak.clone(), ..Sleepy::new("b", 150, Concurrency::Shared) };
    let p = Fake::new(vec![calls(vec![call("1", "a", "{}"), call("2", "b", "{}")]), Message::assistant("done")]);
    let (k, spec, sid) = setup(p.clone(), vec![Arc::new(a), Arc::new(b)], 100_000).await;
    let out = k.run_turn(&spec, sid, go(), &Queues::default(), CancellationToken::new()).await.unwrap();
    assert_eq!((out.reason, out.text.as_str()), (StopReason::Done, "done"));
    assert_eq!(peak.load(Ordering::SeqCst), 2, "两个 Shared 工具应并行");
    let roles: Vec<Role> = k
        .store()
        .branch(sid)
        .await
        .unwrap()
        .iter()
        .map(|e| serde_json::from_value::<Message>(e.payload.clone()).unwrap().role)
        .collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant, Role::Tool, Role::Tool, Role::Assistant]);
    let second = &p.reqs()[1];
    assert_eq!(second.messages[2].tool_call_id.as_deref(), Some("1"));
    assert_eq!(second.messages[3].text(), "b ok");
}

#[tokio::test]
async fn exclusive_tools_are_serial() {
    let a = Sleepy::new("w", 50, Concurrency::Exclusive);
    let peak = a.peak.clone();
    let p = Fake::new(vec![calls(vec![call("1", "w", "{}"), call("2", "w", r#"{"x":1}"#)]), Message::assistant("ok")]);
    let (k, spec, sid) = setup(p, vec![Arc::new(a)], 100_000).await;
    k.run_turn(&spec, sid, go(), &Queues::default(), CancellationToken::new()).await.unwrap();
    assert_eq!(peak.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn steer_is_injected_within_turn() {
    let q = Arc::new(Queues::default());
    let t = Sleepy { steer: Some(q.clone()), ..Sleepy::new("t", 10, Concurrency::Exclusive) };
    let p = Fake::new(vec![calls(vec![call("1", "t", "{}")]), Message::assistant("好的，用B")]);
    let (k, spec, sid) = setup(p.clone(), vec![Arc::new(t)], 100_000).await;
    k.run_turn(&spec, sid, vec![Message::user("做A")], &q, CancellationToken::new()).await.unwrap();
    let last_req = p.reqs().pop().unwrap();
    assert_eq!(last_req.messages.last().unwrap().text(), "插话：改用方案B");
}

#[tokio::test]
async fn follow_up_runs_after_stop() {
    let q = Queues::default();
    q.follow_up(Message::user("还有一件事"));
    let p = Fake::new(vec![Message::assistant("第一件"), Message::assistant("第二件")]);
    let (k, spec, sid) = setup(p.clone(), vec![], 100_000).await;
    let out = k.run_turn(&spec, sid, vec![Message::user("hi")], &q, CancellationToken::new()).await.unwrap();
    assert_eq!(out.text, "第二件");
    assert_eq!(p.reqs().len(), 2);
}

#[tokio::test]
async fn cancel_during_tool_keeps_structure_valid() {
    let t = Sleepy::new("slow", 10_000, Concurrency::Exclusive); // 不响应取消，超过宽限期
    let p = Fake::new(vec![calls(vec![call("1", "slow", "{}"), call("2", "slow", "{}")])]);
    let (k, spec, sid) = setup(p, vec![Arc::new(t)], 100_000).await;
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c2.cancel();
    });
    let out = k.run_turn(&spec, sid, go(), &Queues::default(), cancel).await.unwrap();
    assert_eq!(out.reason, StopReason::Cancelled);
    assert!(out.inputs_saved, "模型已回应（发起了工具调用），输入已落库");
    let msgs: Vec<Message> =
        k.store().branch(sid).await.unwrap().iter().map(|e| serde_json::from_value(e.payload.clone()).unwrap()).collect();
    assert_eq!(msgs.len(), 4, "每个工具调用都要有结果");
    assert!(msgs[2].is_error && msgs[3].is_error);
}

#[tokio::test]
async fn loop_guard_stops_repeats() {
    let t = Sleepy::new("t", 1, Concurrency::Exclusive);
    let same = || calls(vec![call("x", "t", r#"{"q":1}"#)]);
    let p = Fake::new(vec![same(), same(), same(), same()]);
    let (k, spec, sid) = setup(p.clone(), vec![Arc::new(t)], 100_000).await;
    let out = k.run_turn(&spec, sid, go(), &Queues::default(), CancellationToken::new()).await.unwrap();
    assert_eq!(out.reason, StopReason::LoopGuard);
    assert_eq!(p.reqs().len(), 3);
}

#[tokio::test]
async fn bad_args_and_unknown_tool_reported_to_model() {
    let t = Sleepy::new("t", 1, Concurrency::Exclusive);
    let p = Fake::new(vec![calls(vec![call("1", "t", "{oops"), call("2", "nope", "{}")]), Message::assistant("ok")]);
    let (k, spec, sid) = setup(p.clone(), vec![Arc::new(t)], 100_000).await;
    k.run_turn(&spec, sid, go(), &Queues::default(), CancellationToken::new()).await.unwrap();
    let r = &p.reqs()[1].messages;
    assert!(r[2].is_error && r[2].text().contains("JSON"));
    assert!(r[3].is_error && r[3].text().contains("nope"));
}

#[tokio::test]
async fn compaction_triggers_and_context_uses_summary() {
    let p = Fake::new(vec![
        Message::assistant("一"),
        Message::assistant("二"),
        Message::assistant("摘要内容"),
        Message::assistant("三"),
    ]);
    let (k, mut spec, sid) = setup(p.clone(), vec![], 100_000).await;
    let q = Queues::default();
    k.run_turn(&spec, sid, vec![Message::user("甲".repeat(200))], &q, CancellationToken::new()).await.unwrap();
    k.run_turn(&spec, sid, vec![Message::user("乙".repeat(200))], &q, CancellationToken::new()).await.unwrap();
    spec.context_window = 300; // 人为缩小窗口触发压缩
    let out = k.run_turn(&spec, sid, vec![Message::user("丙")], &q, CancellationToken::new()).await.unwrap();
    assert_eq!(out.text, "三");
    let branch = k.store().branch(sid).await.unwrap();
    assert!(branch.iter().any(|e| e.kind == EntryKind::Compaction));
    let last = p.reqs().pop().unwrap();
    assert!(last.messages[0].text().starts_with(SUMMARY_PREFIX));
    assert!(last.messages.iter().all(|m| !m.text().contains(&"甲".repeat(200))));
}

#[tokio::test]
async fn tail_is_sent_but_not_persisted() {
    let p = Fake::new(vec![Message::assistant("嗯")]);
    let (k, mut spec, sid) = setup(p.clone(), vec![], 100_000).await;
    spec.tail = Some("【现在 12:00】".into());
    k.run_turn(&spec, sid, vec![Message::user("在吗")], &Queues::default(), CancellationToken::new()).await.unwrap();
    assert!(p.reqs()[0].messages[0].text().ends_with("【现在 12:00】"));
    let stored: Message = serde_json::from_value(k.store().branch(sid).await.unwrap()[0].payload.clone()).unwrap();
    assert_eq!(stored.text(), "在吗");
}

/// 慢速 Provider：响应前可被取消。
struct Slow;
#[async_trait]
impl Provider for Slow {
    fn id(&self) -> &str {
        "slow"
    }
    async fn stream(&self, _req: ChatRequest, c: CancellationToken) -> Result<EventRx, LlmError> {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            _ = c.cancelled() => return Err(LlmError::new(qya_llm::ErrorKind::Cancelled, "cancel")),
        }
        unreachable!("测试中总会被取消")
    }
    async fn list_models(&self) -> Result<Vec<RemoteModel>, LlmError> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn cancel_before_reply_returns_unconsumed_inputs() {
    let (k, mut spec, sid) = setup(Fake::new(vec![]), vec![], 100_000).await;
    spec.provider = Arc::new(Slow);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        c2.cancel();
    });
    let out = k.run_turn(&spec, sid, vec![Message::user("一"), Message::user("二")], &Queues::default(), cancel).await.unwrap();
    assert_eq!(out.reason, StopReason::Cancelled);
    assert_eq!(out.unconsumed.iter().map(Message::text).collect::<Vec<_>>(), vec!["一", "二"]);
    assert!(!out.inputs_saved);
    assert!(k.store().branch(sid).await.unwrap().is_empty(), "未得到回应的输入不落库");
}

#[tokio::test]
async fn tool_budget_checked_before_execution() {
    let t = Sleepy::new("t", 1, Concurrency::Shared);
    let live = t.peak.clone();
    let many: Vec<ToolCall> = (0..45).map(|i| call(&i.to_string(), "t", &format!(r#"{{"i":{i}}}"#))).collect();
    let p = Fake::new(vec![calls(many), Message::assistant("不应到达")]);
    let (k, spec, sid) = setup(p.clone(), vec![Arc::new(t)], 100_000).await;
    let out = k.run_turn(&spec, sid, go(), &Queues::default(), CancellationToken::new()).await.unwrap();
    assert_eq!(out.reason, StopReason::LoopGuard);
    assert!(live.load(Ordering::SeqCst) <= 40);
    let msgs: Vec<Message> =
        k.store().branch(sid).await.unwrap().iter().map(|e| serde_json::from_value(e.payload.clone()).unwrap()).collect();
    let results: Vec<&Message> = msgs.iter().filter(|m| m.role == Role::Tool).collect();
    assert_eq!(results.len(), 45, "每个调用都要有结果");
    assert_eq!(results.iter().filter(|m| m.is_error && m.text().contains("超过本轮工具调用上限")).count(), 5);
}

/// 流式正常、压缩（complete）时阻塞直到取消。
struct SlowCompact(Arc<Fake>);
#[async_trait]
impl Provider for SlowCompact {
    fn id(&self) -> &str {
        "slow-compact"
    }
    async fn stream(&self, req: ChatRequest, c: CancellationToken) -> Result<EventRx, LlmError> {
        if req.tools.is_empty() && req.system.first().is_some_and(|s| s.contains("压缩")) {
            c.cancelled().await;
            return Err(LlmError::new(qya_llm::ErrorKind::Cancelled, "cancel"));
        }
        self.0.stream(req, c).await
    }
    async fn list_models(&self) -> Result<Vec<RemoteModel>, LlmError> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn cancel_during_compaction_returns_unconsumed_inputs() {
    let p = Fake::new(vec![Message::assistant("一"), Message::assistant("二")]);
    let (k, mut spec, sid) = setup(p.clone(), vec![], 100_000).await;
    let q = Queues::default();
    k.run_turn(&spec, sid, vec![Message::user("甲".repeat(200))], &q, CancellationToken::new()).await.unwrap();
    k.run_turn(&spec, sid, vec![Message::user("乙".repeat(200))], &q, CancellationToken::new()).await.unwrap();
    spec.context_window = 300;
    spec.provider = Arc::new(SlowCompact(p));
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        c2.cancel();
    });
    let out = k.run_turn(&spec, sid, vec![Message::user("丙")], &q, cancel).await.unwrap();
    assert_eq!(out.reason, StopReason::Cancelled);
    assert_eq!(out.unconsumed.iter().map(Message::text).collect::<Vec<_>>(), vec!["丙"]);
}

#[test]
fn queues_take_all_drains_both() {
    let q = Queues::default();
    q.steer(Message::user("插话"));
    q.follow_up(Message::user("追问"));
    assert_eq!(q.take_all().iter().map(Message::text).collect::<Vec<_>>(), vec!["插话", "追问"]);
    assert_eq!(q.pending(), 0);
}

/// 总是返回额度用尽的 Provider。
struct Broken;
#[async_trait]
impl Provider for Broken {
    fn id(&self) -> &str {
        "broken"
    }
    async fn stream(&self, _req: ChatRequest, _c: CancellationToken) -> Result<EventRx, LlmError> {
        Err(LlmError::new(qya_llm::ErrorKind::QuotaExhausted, "quota"))
    }
    async fn list_models(&self) -> Result<Vec<RemoteModel>, LlmError> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn model_error_is_reported_with_inputs_saved() {
    let (k, mut spec, sid) = setup(Fake::new(vec![]), vec![], 100_000).await;
    spec.provider = Arc::new(Broken);
    let out = k.run_turn(&spec, sid, vec![Message::user("在吗")], &Queues::default(), CancellationToken::new()).await.unwrap();
    assert_eq!(out.reason, StopReason::Error);
    assert!(out.inputs_saved, "出错时输入已落库保留记录");
    assert_eq!(out.error.map(|e| e.kind), Some(qya_llm::ErrorKind::QuotaExhausted));
    assert_eq!(k.store().branch(sid).await.unwrap().len(), 1);
}
