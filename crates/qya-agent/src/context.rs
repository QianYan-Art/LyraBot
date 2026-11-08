//! 从会话树分支构建模型上下文，以及 token 估算。

use qya_core::{EntryId, Message, Role};
use qya_store::{Entry, EntryKind};
use serde::{Deserialize, Serialize};

/// 压缩条目的载荷：摘要覆盖 `first_kept` 之前的全部消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compaction {
    pub summary: String,
    pub first_kept: Option<EntryId>,
    pub tokens_before: u64,
}

pub const SUMMARY_PREFIX: &str = "【更早对话的摘要，供你延续上下文；不是阿颜的新消息】\n";

/// 上下文中的一条消息及其来源条目。
#[derive(Debug, Clone)]
pub struct CtxMessage {
    pub entry: Option<EntryId>,
    pub message: Message,
}

/// 取最近一次压缩后的有效上下文：摘要 + first_kept 起的消息。
pub fn build(branch: &[Entry]) -> Result<Vec<CtxMessage>, serde_json::Error> {
    let last_compaction = branch.iter().rposition(|e| e.kind == EntryKind::Compaction);
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
            out.push(CtxMessage { entry: Some(e.id), message: serde_json::from_value(e.payload.clone())? });
        }
    }
    Ok(out)
}

/// 本地 token 估算：中文约 1 字 ≈ 1 token、英文约 4 字符 ≈ 1 token，按保守值取整。
pub fn estimate_tokens(text_chars: usize) -> u64 {
    (text_chars as u64 * 3).div_ceil(4) + 4
}

pub fn estimate_messages(msgs: &[CtxMessage]) -> u64 {
    msgs.iter().map(|m| estimate_tokens(m.message.char_len())).sum()
}

/// 选择压缩切分点：保留尾部约 `keep_tokens`，且切分点必须落在 user 消息上，
/// 保证工具调用与其结果不会被拆开。返回保留部分在 `msgs` 中的起始下标。
pub fn split_point(msgs: &[CtxMessage], keep_tokens: u64) -> Option<usize> {
    let mut acc = 0;
    let mut candidate = None;
    for (i, m) in msgs.iter().enumerate().rev() {
        acc += estimate_tokens(m.message.char_len());
        if m.message.role == Role::User && m.entry.is_some() {
            candidate = Some(i);
            if acc >= keep_tokens {
                break;
            }
        }
    }
    // 至少要有可压缩的前缀
    candidate.filter(|&i| i > 0)
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
    fn split_on_user_boundary() {
        let mut call = Message::assistant("");
        call.tool_calls.push(qya_core::ToolCall { id: "x".into(), name: "t".into(), arguments: "{}".into() });
        let msgs: Vec<CtxMessage> = [
            Message::user("一".repeat(100)),
            Message::assistant("二".repeat(100)),
            Message::user("三".repeat(100)),
            call,
            Message::tool_result("x", "四".repeat(100), false),
            Message::assistant("五"),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, m)| CtxMessage { entry: Some(i as i64), message: m })
        .collect();
        assert_eq!(split_point(&msgs, 50), Some(2));
        // 保留量超过全部内容时，没有可压缩前缀
        assert_eq!(split_point(&msgs[2..], 1_000_000), None);
    }
}
