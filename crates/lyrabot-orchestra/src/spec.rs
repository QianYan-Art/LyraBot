//! 执行者一轮运行规格（`TurnSpec`）构建与执行者内部自用工具（`report_progress` / `need_decision`）。

use crate::types::{ExtrasFn, Handle, ProviderFn, WorkerKind};
use async_trait::async_trait;
use lyrabot_agent::{Concurrency, Tool, ToolCtx, ToolOutput, ToolRegistry, TurnSpec};
use lyrabot_config::Config;
use lyrabot_core::ToolSpec;
use lyrabot_core::memory_file::iter_memory_entries;
use lyrabot_store::SessionRow;
use serde_json::{Value, json};
use std::sync::Arc;

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
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

/// 组装执行者一轮的规格：执行者系统提示（+专员专长与记忆）、工具白名单。
pub(crate) fn build_turn_spec(
    c: &Config,
    row: &SessionRow,
    h: &Arc<Handle>,
    base_tools: &ToolRegistry,
    providers: &ProviderFn,
    extras: Option<&ExtrasFn>,
) -> Result<TurnSpec, String> {
    let o = &c.orchestra;
    let p = c.provider(&o.provider).ok_or("编排提供方未配置")?;
    let info = p.models.get(&row.model).ok_or_else(|| format!("模型 {} 不在目录中", row.model))?;
    let base = std::fs::read_to_string(&o.worker_prompt)
        .map_err(|e| format!("读取执行者提示 {} 失败：{e}", o.worker_prompt.display()))?;
    let mut system = vec![base.trim().to_owned()];
    let tool_names: Vec<String> = match &h.kind {
        WorkerKind::Worker => o.worker_tools.clone(),
        WorkerKind::Specialist(name) => {
            let s = o.specialists.get(name).ok_or_else(|| format!("专员 {name} 已从配置中移除"))?;
            let mut extra = format!(
                "## 你的身份\n你是专员「{name}」：{}。每次任务使用独立会话，请以当前任务与实际查询结果为准；跨任务需要长期记住的固定流程或环境事实，请参考下方长期记忆或用 memory 工具维护。",
                s.description
            );
            if let Some(m) = &s.memory {
                let path = c.persona.memory_dir.join(m);
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let items: Vec<String> = iter_memory_entries(&text).map(|e| format!("- {}", e.replace('\n', " "))).collect();
                    if !items.is_empty() {
                        extra.push_str(&format!("\n\n## 你的长期记忆\n{}", items.join("\n")));
                    }
                }
            }
            system.push(extra);
            if s.tools.is_empty() { o.worker_tools.clone() } else { s.tools.clone() }
        }
    };
    let names: Vec<&str> = tool_names.iter().map(String::as_str).filter(|n| base_tools.get(n).is_some()).collect();
    let missing: Vec<&String> = tool_names.iter().filter(|n| base_tools.get(n).is_none()).collect();
    if !missing.is_empty() {
        tracing::warn!(?missing, "配置的工具在本机未注册，已忽略");
    }
    let mut tools = base_tools
        .select(&names)
        .map_err(|e| e.to_string())?
        .with(Arc::new(ReportProgress::new(h.clone())) as Arc<dyn Tool>)
        .with(Arc::new(NeedDecision::new(h.clone())) as Arc<dyn Tool>);
    if let Some(extras) = extras {
        for t in extras(c, &h.kind) {
            tools = tools.with(t);
        }
    }
    Ok(TurnSpec {
        provider: providers(c, &o.provider)?,
        model: row.model.clone(),
        effort: row.effort.parse().ok(),
        context_window: info.context_length.min(o.context_window),
        max_output_tokens: info.max_output_tokens,
        system,
        tools,
        tail: None,
        compaction: lyrabot_agent::CompactPolicy {
            ratio: o.compaction_ratio,
            keep_tokens: o.compaction_keep_tokens,
            style: lyrabot_agent::CompactStyle::Task,
            time_label: None,
        },
    })
}
