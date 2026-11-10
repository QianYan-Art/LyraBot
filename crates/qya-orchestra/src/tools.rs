//! 编排相关工具：唐语歌用的 delegate / agent_ctl，执行者用的 report_progress / need_decision。

use crate::supervisor::{ContextMode, DelegateRequest, Handle, Notify, Supervisor, Target, WorkerKind};
use async_trait::async_trait;
use qya_agent::{Concurrency, Tool, ToolCtx, ToolOutput};
use qya_config::Config;
use qya_core::card::Card;
use qya_core::{Effort, ToolSpec};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct DelegateTool {
    sup: Supervisor,
    spec: ToolSpec,
}

impl DelegateTool {
    /// 工具说明由配置生成（专员、可选模型），配置不变则字节稳定。
    pub fn new(sup: Supervisor, c: &Config) -> Self {
        let o = &c.orchestra;
        let models: Vec<String> = c.provider(&o.provider).map(|p| p.models.keys().cloned().collect()).unwrap_or_default();
        let mut targets = vec!["worker".to_owned()];
        targets.extend(o.specialists.keys().cloned());
        let specialists: Vec<String> = o.specialists.iter().map(|(n, s)| format!("{n}（{}）", s.description)).collect();
        let description = format!(
            "把需要真正动手的活交给后台执行者：查资料、运维、写代码、生成内容等多步或耗时的事。交出去后先简单告诉对方你安排了，不要说已经做完；结果会以〔后台结果〕交回给你，再由你告诉对方。\
             to：worker 是临时执行者{}。context：brief（默认，只给任务和你写的 context_note）、fork（继承你们当前的完整对话，适合“刚才说的那个”）、fresh（空白开始）。\
             model 与 effort 一般不用填（默认 {} / {}），对方指定时再填。",
            if specialists.is_empty() { String::new() } else { format!("；常驻专员：{}", specialists.join("；")) },
            o.default_model,
            o.default_effort
        );
        let spec = ToolSpec {
            name: "delegate".into(),
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "要做的事，写清楚目标和要求"},
                    "to": {"type": "string", "enum": targets},
                    "context": {"type": "string", "enum": ["brief", "fork", "fresh"]},
                    "context_note": {"type": "string", "description": "brief 时给执行者的背景说明"},
                    "model": {"type": "string", "enum": models},
                    "effort": {"type": "string", "enum": ["low", "medium", "high", "xhigh", "max"]},
                    "notify": {"type": "string", "enum": ["relay", "silent"], "description": "relay（默认）完成后立刻交回；silent 等对方下次说话时再带上"}
                },
                "required": ["task"],
                "additionalProperties": false
            }),
        };
        Self { sup, spec }
    }
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

#[async_trait]
impl Tool for DelegateTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, ctx: ToolCtx, args: Value) -> ToolOutput {
        let Some(task) = s(&args, "task") else { return ToolOutput::err("缺少 task") };
        let context = match s(&args, "context").map(ContextMode::parse) {
            None => ContextMode::Brief,
            Some(Some(c)) => c,
            Some(None) => return ToolOutput::err("context 只能是 brief / fork / fresh"),
        };
        let target = match s(&args, "to") {
            None | Some("worker") => Target::Worker,
            Some(n) => Target::Specialist(n.to_owned()),
        };
        let effort = match s(&args, "effort").map(str::parse::<Effort>) {
            None => None,
            Some(Ok(e)) => Some(e),
            Some(Err(e)) => return ToolOutput::err(e),
        };
        let req = DelegateRequest {
            parent: ctx.session,
            parent_call_id: ctx.call_id,
            task: task.to_owned(),
            context,
            context_note: s(&args, "context_note").map(str::to_owned),
            target,
            model: s(&args, "model").map(str::to_owned),
            effort,
            notify: if s(&args, "notify") == Some("silent") { Notify::Silent } else { Notify::Relay },
        };
        match self.sup.delegate(req).await {
            Ok(t) => ToolOutput::ok(t),
            Err(e) => ToolOutput::err(e),
        }
    }
}

pub struct AgentCtlTool {
    sup: Supervisor,
    spec: ToolSpec,
}

impl AgentCtlTool {
    pub fn new(sup: Supervisor) -> Self {
        let spec = ToolSpec {
            name: "agent_ctl".into(),
            description: "查看或管理后台执行者。status：列出后台任务和进度（对方问进度时用）；continue：给某个任务补充要求或在已完成的任务上继续追问；stop：停下某个任务。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["status", "continue", "stop"]},
                    "id": {"type": "integer", "description": "后台任务编号（continue / stop 必填）"},
                    "message": {"type": "string", "description": "continue 时要交代的话"}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        };
        Self { sup, spec }
    }
}

/// /agent 与 agent_ctl(status) 共用的列表渲染。
pub fn render_list(list: &[crate::WorkerInfo]) -> String {
    if list.is_empty() {
        return "🤖 目前没有后台任务".into();
    }
    let mut c = Card::new("🤖", "后台任务");
    for w in list.iter().take(12) {
        let who = match &w.kind {
            WorkerKind::Worker => format!("任务：{}", w.title),
            WorkerKind::Specialist(n) if w.title.is_empty() || w.title == *n => format!("专员 {n}"),
            WorkerKind::Specialist(n) => format!("专员 {n}：{}", w.title),
        };
        let mut head = format!("#{}｜{}", w.id, w.status.label());
        if let Some(e) = w.elapsed {
            head.push_str(&format!("｜{} 秒", e.as_secs()));
        }
        c.section(head).item(who).item(format!("{}｜思考 {}", w.model, w.effort));
        if let Some(p) = &w.progress {
            c.item(format!("进度：{p}"));
        }
    }
    c.hint("查看或操作：/agent 编号 [stop|话]").build()
}

#[async_trait]
impl Tool for AgentCtlTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, ctx: ToolCtx, args: Value) -> ToolOutput {
        let id = args.get("id").and_then(Value::as_i64);
        let r = match s(&args, "action") {
            Some("status") => Ok(render_list(&self.sup.list().await)),
            Some("continue") => match (id, s(&args, "message")) {
                (Some(id), Some(m)) => self.sup.continue_worker(id, m, ctx.session).await,
                _ => Err("continue 需要 id 和 message".into()),
            },
            Some("stop") => match id {
                Some(id) => self.sup.stop(id),
                None => Err("stop 需要 id".into()),
            },
            _ => Err("action 只能是 status / continue / stop".into()),
        };
        match r {
            Ok(t) => ToolOutput::ok(t),
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// 执行者汇报进度；每个执行者一份实例，绑定自己的句柄。
pub(crate) struct ReportProgress {
    handle: Arc<Handle>,
    spec: ToolSpec,
}

impl ReportProgress {
    pub(crate) fn new(handle: Arc<Handle>) -> Self {
        let spec = ToolSpec {
            name: "report_progress".into(),
            description: "简短汇报当前进度（一句话），查看后台任务时能看到。长任务每完成一个阶段汇报一次即可。".into(),
            parameters: json!({
                "type": "object",
                "properties": {"progress": {"type": "string"}},
                "required": ["progress"],
                "additionalProperties": false
            }),
        };
        Self { handle, spec }
    }
}

#[async_trait]
impl Tool for ReportProgress {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        match s(&args, "progress") {
            Some(p) => {
                self.handle.set_progress(p.chars().take(100).collect());
                ToolOutput::ok("已记录")
            }
            None => ToolOutput::err("缺少 progress"),
        }
    }
}

/// 执行者提出需要用户拍板的问题；汇报时单独交给主对话角色原样转达。
pub(crate) struct NeedDecision {
    handle: Arc<Handle>,
    spec: ToolSpec,
}

impl NeedDecision {
    pub(crate) fn new(handle: Arc<Handle>) -> Self {
        let spec = ToolSpec {
            name: "need_decision".into(),
            description: "只有必须由用户拍板才能继续时调用（例如二选一的方案、要不要执行不可撤回的操作）：写一个具体、能直接回答的问题，可列选项。调用后收尾本轮，汇报里写清已完成的部分，不必重复这个问题。已经被授权的事不要再问。".into(),
            parameters: json!({
                "type": "object",
                "properties": {"question": {"type": "string"}},
                "required": ["question"],
                "additionalProperties": false
            }),
        };
        Self { handle, spec }
    }
}

#[async_trait]
impl Tool for NeedDecision {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        match s(&args, "question").map(str::trim).filter(|q| !q.is_empty()) {
            Some(q) => {
                self.handle.set_question(q);
                ToolOutput::ok("已记录，会随汇报交回。现在收尾本轮。")
            }
            None => ToolOutput::err("缺少 question"),
        }
    }
}
