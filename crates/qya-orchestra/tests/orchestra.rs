//! 编排行为测试：假 Provider + 临时配置，覆盖 fork/brief、模型校验、专员排队、并发上限、停止与续接。
use async_trait::async_trait;
use qya_agent::{Kernel, KernelConfig, ToolRegistry};
use qya_config::ConfigHandle;
use qya_core::{Message, Role, ToolCall, Usage};
use qya_llm::{ChatRequest, Completion, EventRx, FinishReason, LlmError, Provider, RemoteModel, StreamEvent};
use qya_orchestra::{ContextMode, DelegateRequest, Notify, Supervisor, Target, WorkerReport, WorkerStatus};
use qya_store::{EntryKind, NewSession, SessionStatus, Store};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 每次请求延迟 `delay` 后回复“完成：<最后一条用户消息前 10 字>”，并记录请求。
struct Fake {
    delay: Duration,
    requests: Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl Provider for Fake {
    fn id(&self) -> &str {
        "fake"
    }
    async fn stream(&self, req: ChatRequest, cancel: CancellationToken) -> Result<EventRx, LlmError> {
        let last: String = req.messages.iter().rev().find(|m| m.role == Role::User).map(|m| m.text()).unwrap_or_default();
        self.requests.lock().unwrap().push(req);
        tokio::select! {
            _ = tokio::time::sleep(self.delay) => {}
            _ = cancel.cancelled() => return Err(LlmError::new(qya_llm::ErrorKind::Cancelled, "cancel")),
        }
        let (tx, rx) = mpsc::channel(2);
        let text = format!("完成：{}", last.chars().take(10).collect::<String>());
        let c = Completion { message: Message::assistant(text), usage: Usage::default(), finish: FinishReason::Stop };
        tx.send(Ok(StreamEvent::Completed(c))).await.unwrap();
        Ok(rx)
    }
    async fn list_models(&self) -> Result<Vec<RemoteModel>, LlmError> {
        Ok(vec![])
    }
}

struct Env {
    sup: Supervisor,
    store: Store,
    fake: Arc<Fake>,
    reports: mpsc::UnboundedReceiver<WorkerReport>,
    persona: i64,
}

async fn env(max_concurrent: usize, delay_ms: u64) -> Env {
    let dir = std::env::temp_dir().join(format!("qya-orch-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    std::fs::write(dir.join("k.txt"), "k").unwrap();
    std::fs::write(dir.join("agents/worker.md"), "你是后台执行者。").unwrap();
    std::fs::create_dir_all(dir.join("data/memory")).unwrap();
    std::fs::write(dir.join("data/memory/ops.md"), "博客同步用固定脚本\n§\n流量日报只读").unwrap();
    let cfg = format!(
        r#"
[llm]
default_provider = "p"
[llm.providers.p]
base_url = "http://x"
key_file = "k.txt"
transport = "chat_completions"
[llm.providers.p.models.m1]
context_length = 100000
[llm.providers.p.models.m2]
context_length = 100000
[llm.providers.p.models.kimi-for-coding]
context_length = 100000
[persona]
model = "m1"
effort = "low"
assets_dir = "persona"
[orchestra]
default_model = "m1"
default_effort = "high"
max_concurrent = {max_concurrent}
worker_prompt = "agents/worker.md"
[orchestra.specialists.ops]
description = "运维"
model = "m2"
memory = "ops.md"
"#
    );
    std::fs::write(dir.join("qya.toml"), cfg).unwrap();
    let handle = Arc::new(ConfigHandle::open(dir.join("qya.toml")).unwrap());
    let store = Store::in_memory().unwrap();
    let kernel = Kernel::new(store.clone(), KernelConfig { compaction_ratio: 0.9, compaction_keep_tokens: 1000, max_tool_calls: 10 });
    let fake = Arc::new(Fake { delay: Duration::from_millis(delay_ms), requests: Mutex::default() });
    let f2 = fake.clone();
    let (tx, reports) = mpsc::unbounded_channel();
    let sup = Supervisor::new(handle, kernel, ToolRegistry::default(), tx, Arc::new(move |_, _| Ok(f2.clone() as Arc<dyn Provider>)));
    let persona = store.create_session(NewSession { model: "m1".into(), effort: "low".into(), ..Default::default() }).await.unwrap();
    Env { sup, store, fake, reports, persona }
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}

fn req(persona: i64, task: &str, context: ContextMode, target: Target) -> DelegateRequest {
    DelegateRequest {
        parent: persona,
        parent_call_id: "call_d".into(),
        task: task.into(),
        context,
        context_note: Some("阿颜在赶周报".into()),
        target,
        model: None,
        effort: None,
        notify: Notify::Relay,
    }
}

async fn next(rx: &mut mpsc::UnboundedReceiver<WorkerReport>) -> WorkerReport {
    tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("等待汇报超时").unwrap()
}

#[tokio::test]
async fn fork_inherits_persona_branch_with_valid_structure() {
    let mut e = env(3, 10).await;
    // 唐语歌会话：阿颜的话 + 她发起的 delegate 调用
    e.store.append(e.persona, EntryKind::Message, serde_json::to_value(Message::user("帮我查下刚才那个报错")).unwrap(), 1).await.unwrap();
    let mut call = Message::with_parts(Role::Assistant, vec![]);
    call.tool_calls.push(ToolCall { id: "call_d".into(), name: "delegate".into(), arguments: "{}".into() });
    e.store.append(e.persona, EntryKind::Message, serde_json::to_value(&call).unwrap(), 1).await.unwrap();

    let ack = e.sup.delegate(req(e.persona, "查报错原因", ContextMode::Fork, Target::Worker)).await.unwrap();
    assert!(ack.contains("模型 m1") && ack.contains("思考 high"), "{ack}");
    let r = next(&mut e.reports).await;
    assert_eq!(r.status, WorkerStatus::Done);
    let sent = e.fake.requests.lock().unwrap()[0].clone();
    assert_eq!(sent.system, vec!["你是后台执行者。".to_owned()]);
    let roles: Vec<Role> = sent.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant, Role::Tool, Role::User]);
    assert_eq!(sent.messages[2].tool_call_id.as_deref(), Some("call_d"));
    assert!(sent.messages[3].text().contains("任务：查报错原因"));
    // 唐语歌会话不受影响，worker 已归档
    assert_eq!(e.store.branch(e.persona).await.unwrap().len(), 2);
    assert_eq!(e.store.session(r.id).await.unwrap().status, SessionStatus::Archived);
}

#[tokio::test]
async fn brief_and_model_validation() {
    let mut e = env(3, 10).await;
    let mut bad = req(e.persona, "x", ContextMode::Brief, Target::Worker);
    bad.model = Some("nope".into());
    assert!(e.sup.delegate(bad).await.unwrap_err().contains("不可用"));
    assert!(e.sup.delegate(req(e.persona, "x", ContextMode::Brief, Target::Specialist("ghost".into()))).await.is_err());

    let mut ok = req(e.persona, "写周报提纲", ContextMode::Brief, Target::Worker);
    ok.model = Some("m2".into());
    ok.effort = Some(qya_core::Effort::Low);
    e.sup.delegate(ok).await.unwrap();
    let r = next(&mut e.reports).await;
    assert!(r.summary.starts_with("完成：任务：写周报提纲"), "{}", r.summary);
    let sent = e.fake.requests.lock().unwrap()[0].clone();
    assert_eq!(sent.model, "m2");
    assert_eq!(sent.messages.len(), 1);
    assert!(sent.messages[0].text().contains("背景：阿颜在赶周报"));
}

#[tokio::test]
async fn specialist_keeps_session_memory_and_queues() {
    let mut e = env(3, 150).await;
    let a1 = e.sup.delegate(req(e.persona, "任务一", ContextMode::Fork, Target::Specialist("ops".into()))).await.unwrap();
    assert!(a1.contains("fork 不适用"), "{a1}");
    let a2 = e.sup.delegate(req(e.persona, "任务二", ContextMode::Brief, Target::Specialist("ops".into()))).await.unwrap();
    assert!(a2.contains("排在"), "{a2}");
    let r = next(&mut e.reports).await;
    assert_eq!(r.status, WorkerStatus::Done);
    // 同一会话里先后处理了两项任务（第二项通过 follow-up）
    let reqs = e.fake.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].model, "m2");
    assert!(reqs[0].system[1].contains("常驻专员「ops」") && reqs[0].system[1].contains("- 博客同步用固定脚本"));
    assert!(reqs[1].messages.iter().any(|m| m.text().contains("任务一")), "专员保有之前的上下文");
    // 再派一次：复用同一个专员会话
    e.sup.delegate(req(e.persona, "任务三", ContextMode::Brief, Target::Specialist("ops".into()))).await.unwrap();
    let r2 = next(&mut e.reports).await;
    assert_eq!(r2.id, r.id);
}

#[tokio::test]
async fn concurrency_limit_queues_extra_workers() {
    let mut e = env(1, 200).await;
    e.sup.delegate(req(e.persona, "甲", ContextMode::Fresh, Target::Worker)).await.unwrap();
    e.sup.delegate(req(e.persona, "乙", ContextMode::Fresh, Target::Worker)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let statuses: Vec<WorkerStatus> = e.sup.list().await.iter().map(|w| w.status).collect();
    assert!(statuses.contains(&WorkerStatus::Queued) && statuses.contains(&WorkerStatus::Running), "{statuses:?}");
    next(&mut e.reports).await;
    next(&mut e.reports).await;
    assert_eq!(e.sup.running(), 0);
}

#[tokio::test]
async fn stop_and_continue() {
    let mut e = env(3, 2_000).await;
    e.sup.delegate(req(e.persona, "很慢的活", ContextMode::Fresh, Target::Worker)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let id = e.sup.list().await[0].id;
    e.sup.stop(id).unwrap();
    let r = next(&mut e.reports).await;
    assert_eq!(r.status, WorkerStatus::Cancelled);
    assert!(e.sup.stop(id).is_err());
    // 已结束的任务可以续接，在原会话上继续
    let ack = e.sup.continue_worker(id, "换个思路再试").await.unwrap();
    assert!(ack.contains("续接"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    e.sup.stop(id).unwrap();
    let r2 = next(&mut e.reports).await;
    assert_eq!(r2.id, id);
    let reqs = e.fake.requests.lock().unwrap().clone();
    assert!(reqs.last().unwrap().messages.iter().any(|m| m.text() == "任务：很慢的活"));
    assert!(e.sup.continue_worker(e.persona, "x").await.is_err(), "唐语歌会话不是后台任务");
}
