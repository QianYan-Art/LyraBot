//! todo：执行者的多步任务清单（按会话保存在内存中，整表覆盖写）。

use async_trait::async_trait;
use qya_agent::{Concurrency, Tool, ToolCtx, ToolOutput};
use qya_core::{SessionId, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone, Deserialize)]
struct Item {
    text: String,
    #[serde(default)]
    status: String,
}

pub struct Todo {
    spec: ToolSpec,
    lists: Mutex<HashMap<SessionId, Vec<Item>>>,
}

impl Default for Todo {
    fn default() -> Self {
        Self {
            spec: ToolSpec {
                name: "todo".into(),
                description: "维护本任务的步骤清单：每次传入完整列表（覆盖旧的），status 取 pending / doing / done。适合三步以上的任务。不传 items 则查看当前清单。".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "items": {"type": "array", "items": {"type": "object", "properties": {
                            "text": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "doing", "done"]}
                        }, "required": ["text", "status"], "additionalProperties": false}}
                    },
                    "additionalProperties": false
                }),
            },
            lists: Mutex::default(),
        }
    }
}

fn render(items: &[Item]) -> String {
    if items.is_empty() {
        return "（清单为空）".into();
    }
    items
        .iter()
        .map(|i| {
            let mark = match i.status.as_str() {
                "done" => "[x]",
                "doing" => "[>]",
                _ => "[ ]",
            };
            format!("{mark} {}", i.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl Tool for Todo {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn call(&self, ctx: ToolCtx, args: Value) -> ToolOutput {
        let mut lists = self.lists.lock().expect("todo 锁中毒");
        match args.get("items") {
            Some(v) => match serde_json::from_value::<Vec<Item>>(v.clone()) {
                Ok(items) => {
                    let text = render(&items);
                    lists.insert(ctx.session, items);
                    ToolOutput::ok(text)
                }
                Err(e) => ToolOutput::err(format!("items 格式不对：{e}")),
            },
            None => ToolOutput::ok(render(lists.get(&ctx.session).map(Vec::as_slice).unwrap_or_default())),
        }
    }
}
