//! 编排行为测试：假 Provider + 临时配置，覆盖 fork/brief、模型校验、专员排队、并发上限、停止与续接、
//! 汇报先落库、重启后发现中断任务、need_decision 结构化提问、续接使用预留名额。
use async_trait::async_trait;
use lyrabot_agent::{Kernel, KernelConfig, ToolRegistry};
use lyrabot_config::ConfigHandle;
use lyrabot_core::{Message, Role, ToolCall, Usage};
use lyrabot_llm::{ChatRequest, Completion, EventRx, FinishReason, LlmError, Provider, RemoteModel, StreamEvent};
use lyrabot_orchestra::{ContextMode, DelegateRequest, Notify, Supervisor, Target, WorkerReport, WorkerStatus};
use lyrabot_store::{EntryKind, NewSession, SessionKind, SessionStatus, Store};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 每次请求延迟 `delay` 后回复“完成：<最后一条用户消息前 10 字>”，并记录请求。
/// 任务里含“【问】”且还没有工具结果时，先调用 need_decision。
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
            _ = cancel.cancelled() => return Err(LlmError::new(lyrabot_llm::ErrorKind::Cancelled, "cancel")),
        }
        let (tx, rx) = mpsc::channel(2);
        let asked = self.requests.lock().unwrap().last().is_some_and(|r| r.messages.iter().any(|m| m.role == Role::Tool));
        let c = if last.contains("【问】") && !asked {
            let mut m = Message::with_parts(Role::Assistant, vec![]);
            m.tool_calls.push(ToolCall { id: "q1".into(), name: "need_decision".into(), arguments: r#"{"question":"用方案A还是B？"}"#.into() });
            Completion { message: m, usage: Usage::default(), finish: FinishReason::ToolCalls }
        } else {
            let text = format!("完成：{}", last.chars().take(10).collect::<String>());
            Completion { message: Message::assistant(text), usage: Usage::default(), finish: FinishReason::Stop }
        };
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
    dir: std::path::PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn env(max_concurrent: usize, delay_ms: u64) -> Env {
    let dir = std::env::temp_dir().join(format!("lyrabot-orch-{}-{}", std::process::id(), rand_suffix()));
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
    std::fs::write(dir.join("lyrabot.toml"), cfg).unwrap();
    let handle = Arc::new(ConfigHandle::open(dir.join("lyrabot.toml")).unwrap());
    let store = Store::in_memory().unwrap();
    let kernel = Kernel::new(store.clone(), KernelConfig { compaction_ratio: 0.9, compaction_keep_tokens: 1000, max_tool_calls: 10 });
    let fake = Arc::new(Fake { delay: Duration::from_millis(delay_ms), requests: Mutex::default() });
    let f2 = fake.clone();
    let (tx, reports) = mpsc::unbounded_channel();
    let sup = Supervisor::new(handle, kernel, ToolRegistry::default(), tx, Arc::new(move |_, _| Ok(f2.clone() as Arc<dyn Provider>)));
    let persona = store.create_session(NewSession { model: "m1".into(), effort: "low".into(), ..Default::default() }).await.unwrap();
    Env { sup, store, fake, reports, persona, dir }
}

fn rand_suffix() -> String {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let s = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{n}-{s}")
}

fn req(persona: i64, task: &str, context: ContextMode, target: Target) -> DelegateRequest {
    DelegateRequest {
        parent: persona,
        parent_call_id: "call_d".into(),
        task: task.into(),
        context,
        context_note: Some("用户在赶周报".into()),
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
    // 主对话角色会话：用户的话 + 主对话角色发起的 delegate 调用
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
    // 主对话角色会话不受影响，worker 已归档
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
    ok.effort = Some(lyrabot_core::Effort::Low);
    e.sup.delegate(ok).await.unwrap();
    let r = next(&mut e.reports).await;
    assert!(r.summary.starts_with("完成：任务：写周报提纲"), "{}", r.summary);
    let sent = e.fake.requests.lock().unwrap()[0].clone();
    assert_eq!(sent.model, "m2");
    assert_eq!(sent.messages.len(), 1);
    assert!(sent.messages[0].text().contains("背景：用户在赶周报"));
}

#[tokio::test]
async fn specialist_uses_fresh_session_per_task_while_keeping_memory_and_serial_queue() {
    let mut e = env(3, 150).await;
    let a1 = e.sup.delegate(req(e.persona, "任务一", ContextMode::Fork, Target::Specialist("ops".into()))).await.unwrap();
    assert!(a1.contains("fork 不适用"), "{a1}");
    let a2 = e.sup.delegate(req(e.persona, "任务二", ContextMode::Brief, Target::Specialist("ops".into()))).await.unwrap();
    assert!(a2.contains("排在"), "{a2}");
    let r1 = next(&mut e.reports).await;
    let r2 = next(&mut e.reports).await;
    assert_eq!(r1.status, WorkerStatus::Done);
    assert_eq!(r2.status, WorkerStatus::Done);
    assert_ne!(r1.id, r2.id, "每次派给专员的新任务应使用独立的新会话");
    assert_eq!(e.store.session(r1.id).await.unwrap().status, SessionStatus::Archived);
    assert_eq!(e.store.session(r2.id).await.unwrap().status, SessionStatus::Archived);

    let reqs = e.fake.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].model, "m2");
    assert!(reqs[0].system[1].contains("专员「ops」") && reqs[0].system[1].contains("- 博客同步用固定脚本"));
    assert!(
        !reqs[1].messages.iter().any(|m| m.text().contains("任务一")),
        "专员新任务不应携带上一次任务的对话历史"
    );

    // 针对第一次任务明确续接追问时，仍在 r1.id 原会话上带着任务一上下文运行
    e.sup.continue_worker(r1.id, "追问任务一细节", e.persona).await.unwrap();
    let r3 = next(&mut e.reports).await;
    assert_eq!(r3.id, r1.id);
    let reqs = e.fake.requests.lock().unwrap().clone();
    assert!(reqs[2].messages.iter().any(|m| m.text().contains("任务一")), "明确续接某次任务时保留该任务上下文");
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
    let ack = e.sup.continue_worker(id, "换个思路再试", e.persona).await.unwrap();
    assert!(ack.contains("续接"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    e.sup.stop(id).unwrap();
    let r2 = next(&mut e.reports).await;
    assert_eq!(r2.id, id);
    let reqs = e.fake.requests.lock().unwrap().clone();
    assert!(reqs.last().unwrap().messages.iter().any(|m| m.text() == "任务：很慢的活"));
    assert!(e.sup.continue_worker(e.persona, "x", e.persona).await.is_err(), "主对话角色会话不是后台任务");
}

#[tokio::test]
async fn report_is_mailed_and_restart_recovers_interrupted() {
    let mut e = env(3, 200).await;
    e.sup.delegate(req(e.persona, "查日志", ContextMode::Fresh, Target::Worker)).await.unwrap();
    // delegate 返回时任务正文与运行标记已经落库：此刻退出也能在重启后发现
    let marked = e.store.running_markers().await.unwrap();
    assert_eq!(marked.len(), 1);
    assert!(e.store.branch(marked[0].worker).await.unwrap()[0].payload.to_string().contains("任务：查日志"));
    let r = next(&mut e.reports).await;
    assert!(e.store.running_markers().await.unwrap().is_empty(), "汇报落库时清掉标记");
    // 汇报先落库，确认前一直在信箱里，可原样还原
    let mail = e.store.pending_mail().await.unwrap();
    assert_eq!((mail.len(), mail[0].id), (1, r.mail));
    let restored = WorkerReport::from_mail(&mail[0]).unwrap();
    assert_eq!((restored.id, restored.status, restored.summary.clone()), (r.id, WorkerStatus::Done, r.summary.clone()));
    e.store.ack_mail(vec![r.mail]).await.unwrap();

    // 模拟进程在执行中退出：运行标记残留，任务已写进会话；同时存在旧版遗留的闲置 active 专员会话与 specialist:ops kv 键
    let lost = e.store.create_session(NewSession { kind: Some(SessionKind::Worker), name: Some("发周报".into()), model: "m1".into(), effort: "high".into(), ..Default::default() }).await.unwrap();
    let legacy_spec = e.store.create_session(NewSession { kind: Some(SessionKind::Specialist), name: Some("ops".into()), model: "m2".into(), effort: "high".into(), ..Default::default() }).await.unwrap();
    e.store.kv_set("specialist:ops", &legacy_spec.to_string()).await.unwrap();
    e.store.begin_run(lost, e.persona, vec![]).await.unwrap();
    assert_eq!(e.sup.recover().await.unwrap(), 1);
    assert_eq!(e.sup.recover().await.unwrap(), 0, "只报告一次");
    let mail = e.store.pending_mail().await.unwrap();
    let r = WorkerReport::from_mail(&mail[0]).unwrap();
    assert_eq!((r.id, r.status, r.title.as_str()), (lost, WorkerStatus::Interrupted, "发周报"));
    assert_eq!(e.store.session(lost).await.unwrap().status, SessionStatus::Archived);
    assert_eq!(e.store.session(legacy_spec).await.unwrap().status, SessionStatus::Archived, "旧版遗留的闲置专员会话自动归档");
    assert_eq!(e.store.kv_get("specialist:ops").await.unwrap(), None, "旧版 specialist:* kv 键自动清理");
}

#[tokio::test]
async fn need_decision_is_reported_as_question() {
    let mut e = env(3, 10).await;
    e.sup.delegate(req(e.persona, "迁移数据库【问】", ContextMode::Fresh, Target::Worker)).await.unwrap();
    let r = next(&mut e.reports).await;
    assert_eq!(r.status, WorkerStatus::Waiting);
    assert_eq!(r.question.as_deref(), Some("用方案A还是B？"));
    // 续接时问题清空，正常完成
    e.sup.continue_worker(r.id, "用A", e.persona).await.unwrap();
    let r2 = next(&mut e.reports).await;
    assert_eq!((r2.status, r2.question), (WorkerStatus::Done, None));
}

#[tokio::test]
async fn continuation_uses_spare_slot_when_full() {
    let mut e = env(1, 300).await;
    e.sup.delegate(req(e.persona, "旧任务", ContextMode::Fresh, Target::Worker)).await.unwrap();
    let old = next(&mut e.reports).await.id;
    e.sup.delegate(req(e.persona, "占满并发", ContextMode::Fresh, Target::Worker)).await.unwrap();
    e.sup.delegate(req(e.persona, "排队新任务", ContextMode::Fresh, Target::Worker)).await.unwrap();
    e.sup.continue_worker(old, "接着做", e.persona).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    let list = e.sup.list().await;
    let status = |id| list.iter().find(|w| w.id == id).unwrap().status;
    assert_eq!(status(old), WorkerStatus::Running, "用户的续接不排在新任务后面");
    assert_eq!(list.iter().filter(|w| w.status == WorkerStatus::Queued).count(), 1);
    for _ in 0..3 {
        next(&mut e.reports).await;
    }
}

#[tokio::test]
async fn queued_task_for_busy_specialist_runs_serially_in_own_session() {
    let mut e = env(3, 300).await;
    e.sup.delegate(req(e.persona, "第一件事", ContextMode::Fresh, Target::Specialist("ops".into()))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ack = e.sup.delegate(req(e.persona, "第二件事", ContextMode::Brief, Target::Specialist("ops".into()))).await.unwrap();
    assert!(ack.contains("排在"), "{ack}");
    let r1 = next(&mut e.reports).await;
    let r2 = next(&mut e.reports).await;
    assert!(r1.summary.contains("第一件事"), "{}", r1.summary);
    assert!(r2.summary.contains("第二件事"), "{}", r2.summary);
    assert_ne!(r1.id, r2.id);
    assert_eq!(e.sup.running(), 0);
}

#[tokio::test]
async fn stopped_run_keeps_late_messages_without_rerunning() {
    let mut e = env(3, 2_000).await;
    e.sup.delegate(req(e.persona, "很慢的活", ContextMode::Fresh, Target::Worker)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let id = e.sup.list().await[0].id;
    // 运行中插话（同时落库），随后停止：插话不应再触发新一轮
    assert!(e.sup.continue_worker(id, "顺便查下磁盘", e.persona).await.unwrap().contains("插给"));
    e.sup.stop(id).unwrap();
    assert_eq!(next(&mut e.reports).await.status, WorkerStatus::Cancelled);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(e.sup.running(), 0, "停止后不自动续跑");
    assert!(e.reports.try_recv().is_err());
    let branch = e.store.branch(id).await.unwrap();
    assert!(branch.iter().any(|en| en.payload.to_string().contains("顺便查下磁盘")), "插话写进了会话");
}

#[tokio::test]
async fn recover_restores_queued_messages_into_session() {
    let e = env(3, 10).await;
    let w = e.store.create_session(NewSession { kind: Some(SessionKind::Worker), name: Some("整理日志".into()), model: "m1".into(), effort: "high".into(), ..Default::default() }).await.unwrap();
    e.store.begin_run(w, e.persona, vec![(serde_json::to_value(Message::user("任务：整理日志")).unwrap(), 1)]).await.unwrap();
    e.store.queue_input(w, e.persona, serde_json::to_value(Message::user("只看昨天的")).unwrap(), 1).await.unwrap();
    assert_eq!(e.sup.recover().await.unwrap(), 1);
    let texts: Vec<String> = e.store.branch(w).await.unwrap().iter().map(|en| en.payload.to_string()).collect();
    assert!(texts.last().unwrap().contains("只看昨天的"), "{texts:?}");
    let r = WorkerReport::from_mail(&e.store.pending_mail().await.unwrap()[0]).unwrap();
    assert!(r.summary.contains("1 条补充"), "{}", r.summary);
}
