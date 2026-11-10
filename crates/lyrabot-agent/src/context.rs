//! 从会话树分支构建模型上下文，以及 token 估算。

use lyrabot_core::{EntryId, Message, Role};
use lyrabot_store::{Entry, EntryKind};
use serde::{Deserialize, Serialize};

/// 压缩条目的载荷：摘要覆盖 `first_kept` 之前的全部消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compaction {
    pub summary: String,
    pub first_kept: Option<EntryId>,
    pub tokens_before: u64,
}

pub const SUMMARY_PREFIX: &str = "【更早对话的摘要，供你延续上下文；不是对方的新消息】\n";

/// 折叠条目的载荷：`before` 之前的工具结果在上下文中替换为一行占位（原文仍在记录里）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fold {
    pub before: EntryId,
}

pub const FOLDED_TOOL: &str = "〔较早的工具结果已折叠以节省上下文，原文仍保存在记录中〕";

/// 上下文中的一条消息及其来源条目。
#[derive(Debug, Clone)]
pub struct CtxMessage {
    pub entry: Option<EntryId>,
    pub message: Message,
}

/// 取最近一次压缩后的有效上下文：摘要 + first_kept 起的消息；其后若有折叠，较早的工具结果替换为占位。
pub fn build(branch: &[Entry]) -> Result<Vec<CtxMessage>, serde_json::Error> {
    let last_compaction = branch.iter().rposition(|e| e.kind == EntryKind::Compaction);
    let fold_before = branch[last_compaction.map_or(0, |i| i + 1)..]
        .iter()
        .rev()
        .find(|e| e.kind == EntryKind::Fold)
        .map(|e| serde_json::from_value::<Fold>(e.payload.clone()))
        .transpose()?
        .map(|f| f.before);
    let (summary, start) = match last_compaction {
        None => (None, 0),
        Some(ci) => {
            let c: Compaction = serde_json::from_value(branch[ci].payload.clone())?;
            let start = c
                .first_kept
                .and_then(|id| branch.iter().position(|e| e.id == id))
                .unwrap_or(ci + 1);
            (Some(c.summary), start)
        }
    };
    let mut out = Vec::with_capacity(branch.len() - start + 1);
    if let Some(s) = summary {
        out.push(CtxMessage { entry: None, message: Message::user(format!("{SUMMARY_PREFIX}{s}")) });
    }
    for e in &branch[start..] {
        if e.kind == EntryKind::Message {
            let mut message: Message = serde_json::from_value(e.payload.clone())?;
            if message.role == Role::Tool && fold_before.is_some_and(|b| e.id < b) {
                message.content = vec![lyrabot_core::ContentPart::text(FOLDED_TOOL)];
            }
            out.push(CtxMessage { entry: Some(e.id), message });
        }
    }
    Ok(out)
}

/// 摘要胶囊各列表的条数与单条字数上限、渲染总长上限。
const CAPSULE_ITEMS: usize = 8;
const CAPSULE_ITEM_CHARS: usize = 150;
const CAPSULE_CHARS: usize = 1200;
const SUMMARY_CHARS: usize = 2000;

/// 把压缩模型的输出渲染成确定性的摘要文本：`{summary, capsule}` JSON 按固定标签渲染并限长；
/// 不是 JSON 时当作纯文本摘要；JSON 残缺时只取能解析出的 summary，绝不把 JSON 残骸写进摘要。
pub fn render_compaction(output: &str) -> String {
    let body = output.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    if !body.starts_with('{') {
        return clip(body, SUMMARY_CHARS);
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        tracing::warn!("压缩输出不是合法 JSON，按无摘要处理");
        return String::new();
    };
    let mut out = clip(v["summary"].as_str().unwrap_or_default().trim(), SUMMARY_CHARS);
    let cap = &v["capsule"];
    let mut capsule = String::new();
    if let Some(o) = cap["objective"].as_str().map(str::trim).filter(|o| !o.is_empty()) {
        capsule.push_str(&format!("【当前目标】{}\n", clip(o, CAPSULE_ITEM_CHARS)));
    }
    for (key, label) in [("constraints", "用户的要求与约束"), ("decisions", "已确认的决定"), ("verification", "验证状态"), ("next_steps", "下一步")] {
        let items: Vec<String> = cap[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str().map(str::trim).filter(|x| !x.is_empty()))
            .take(CAPSULE_ITEMS)
            .map(|x| format!("- {}", clip(x, CAPSULE_ITEM_CHARS)))
            .collect();
        if !items.is_empty() {
            capsule.push_str(&format!("【{label}】\n{}\n", items.join("\n")));
        }
    }
    if !capsule.is_empty() {
        out.push_str("\n\n");
        out.push_str(&clip(capsule.trim_end(), CAPSULE_CHARS));
    }
    out.trim().to_owned()
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_owned() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// 本地 token 估算：中文约 1 字 ≈ 1 token、英文约 4 字符 ≈ 1 token，按保守值取整。
pub fn estimate_tokens(text_chars: usize) -> u64 {
    (text_chars as u64 * 3).div_ceil(4) + 4
}

pub fn estimate_messages(msgs: &[CtxMessage]) -> u64 {
    msgs.iter().map(|m| estimate_tokens(m.message.char_len())).sum()
}

/// 选择压缩切分点：保留尾部约 `keep_tokens`，优先落在 user 消息上；
/// 若没有后续 user 消息（如后台单次任务的长工具循环已超过 `keep_tokens`），
/// 则回退到 assistant 工具轮次起点（`i >= 2`），保证工具调用与其结果不会被拆开。
pub fn split_point(msgs: &[CtxMessage], keep_tokens: u64) -> Option<usize> {
    let mut acc = 0;
    let mut user_cand = None;
    for (i, m) in msgs.iter().enumerate().rev() {
        acc += estimate_tokens(m.message.char_len());
        if m.message.role == Role::User && m.entry.is_some() && i > 0 {
            user_cand = Some(i);
            if acc >= keep_tokens {
                break;
            }
        }
    }
    if user_cand.is_some() {
        return user_cand;
    }
    let mut acc = 0;
    for (i, m) in msgs.iter().enumerate().rev() {
        acc += estimate_tokens(m.message.char_len());
        if i >= 2 && m.message.role == Role::Assistant && m.entry.is_some() && acc >= keep_tokens {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: i64, kind: EntryKind, payload: serde_json::Value) -> Entry {
        Entry { id, session_id: 1, parent_id: None, kind, payload, tokens: 0, created_at: 0 }
    }
    fn msg(id: i64, m: Message) -> Entry {
        entry(id, EntryKind::Message, serde_json::to_value(m).unwrap())
    }

    #[test]
    fn compaction_replaces_prefix() {
        let branch = vec![
            msg(1, Message::user("a")),
            msg(2, Message::assistant("b")),
            msg(3, Message::user("c")),
            msg(4, Message::assistant("d")),
            entry(5, EntryKind::Compaction, json!({"summary":"S","first_kept":3,"tokens_before":10})),
            msg(6, Message::user("e")),
        ];
        let ctx = build(&branch).unwrap();
        let texts: Vec<_> = ctx.iter().map(|c| c.message.text()).collect();
        assert_eq!(texts, vec![format!("{SUMMARY_PREFIX}S"), "c".into(), "d".into(), "e".into()]);
    }

    #[test]
    fn fold_replaces_older_tool_results() {
        let mut call = Message::assistant("");
        call.tool_calls.push(lyrabot_core::ToolCall { id: "x".into(), name: "t".into(), arguments: "{}".into() });
        let branch = vec![
            msg(1, Message::user("查一下")),
            msg(2, call.clone()),
            msg(3, Message::tool_result("x", "很长的旧结果", false)),
            msg(4, Message::user("再查")),
            msg(5, call),
            msg(6, Message::tool_result("x", "新结果", false)),
            entry(7, EntryKind::Fold, json!({"before": 4})),
        ];
        let ctx = build(&branch).unwrap();
        assert_eq!(ctx.len(), 6, "折叠不删除消息，结构保持合法");
        assert_eq!(ctx[2].message.text(), FOLDED_TOOL);
        assert_eq!(ctx[2].message.tool_call_id.as_deref(), Some("x"));
        assert_eq!(ctx[5].message.text(), "新结果", "折叠点之后的工具结果保留原文");
    }

    #[test]
    fn compaction_output_is_rendered_deterministically() {
        let out = r#"```json
{"summary": "- 1 月 5 日前交稿\n- 用户纠正：用北京时间", "capsule": {"objective": "整理周报", "constraints": ["不要用表格"], "decisions": [], "next_steps": ["补数据", ""]}}
```"#;
        assert_eq!(
            render_compaction(out),
            "- 1 月 5 日前交稿\n- 用户纠正：用北京时间\n\n【当前目标】整理周报\n【用户的要求与约束】\n- 不要用表格\n【下一步】\n- 补数据"
        );
        assert_eq!(render_compaction("只是一段纯文本摘要"), "只是一段纯文本摘要");
        assert_eq!(render_compaction(r#"{"summary": "半截"#), "", "残缺 JSON 不进摘要");
        assert_eq!(render_compaction(r#"{"summary": "无任务", "capsule": null}"#), "无任务");
    }

    #[test]
    fn split_on_user_boundary() {
        let mut call = Message::assistant("");
        call.tool_calls.push(lyrabot_core::ToolCall { id: "x".into(), name: "t".into(), arguments: "{}".into() });
        let msgs: Vec<CtxMessage> = [
            Message::user("一".repeat(100)),
            Message::assistant("二".repeat(100)),
            Message::user("三".repeat(100)),
            call.clone(),
            Message::tool_result("x", "四".repeat(100), false),
            Message::assistant("五"),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, m)| CtxMessage { entry: Some(i as i64), message: m })
        .collect();
        assert_eq!(split_point(&msgs, 50), Some(2));
        // 只有开头一条 user 消息（单次后台任务多轮工具调用）时，回退到 assistant 轮次边界切分，不拆散 call 与 tool_result
        let single_user_loop: Vec<CtxMessage> = [
            Message::user("任务：排查日志".repeat(20)),
            call.clone(),
            Message::tool_result("x", "第一轮工具输出".repeat(30), false),
            call,
            Message::tool_result("x", "第二轮工具输出".repeat(30), false),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, m)| CtxMessage { entry: Some(i as i64), message: m })
        .collect();
        assert_eq!(split_point(&single_user_loop, 50), Some(3));
        // 保留量超过全部内容时，没有可压缩前缀
        assert_eq!(split_point(&msgs[2..], 1_000_000), None);
    }
}
