//! persona_notes：唐语歌按需查阅自己的细节资料（替代 Hermes 的 skill_view(tangyuge-roleplay)）。

use crate::assets::Note;
use async_trait::async_trait;
use qya_agent::{Concurrency, Tool, ToolCtx, ToolOutput};
use qya_core::ToolSpec;
use serde_json::json;

pub struct PersonaNotes {
    spec: ToolSpec,
    notes: Vec<Note>,
}

impl PersonaNotes {
    pub fn new(notes: Vec<Note>) -> Self {
        let ids: Vec<&str> = notes.iter().map(|n| n.id.as_str()).collect();
        let menu: Vec<String> = notes.iter().map(|n| format!("{}：{}", n.id, n.usage)).collect();
        let spec = ToolSpec {
            name: "persona_notes".into(),
            description: format!("查阅你自己的细节资料，用来让回复更像你。条目——{}", menu.join("；")),
            parameters: json!({
                "type": "object",
                "properties": {"topic": {"type": "string", "enum": ids}},
                "required": ["topic"],
                "additionalProperties": false
            }),
        };
        Self { spec, notes }
    }
}

#[async_trait]
impl Tool for PersonaNotes {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn call(&self, _ctx: ToolCtx, args: serde_json::Value) -> ToolOutput {
        let topic = args.get("topic").and_then(|v| v.as_str()).unwrap_or_default();
        match self.notes.iter().find(|n| n.id == topic) {
            Some(n) => ToolOutput::ok(n.body.clone()),
            None => {
                let ids: Vec<&str> = self.notes.iter().map(|n| n.id.as_str()).collect();
                ToolOutput::err(format!("没有条目 `{topic}`，可选：{}", ids.join("、")))
            }
        }
    }
}
