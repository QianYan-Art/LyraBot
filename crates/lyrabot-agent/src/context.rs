//! 从会话树分支构建模型上下文，以及 token 估算。

use crate::kernel::{CompactPolicy, CompactStyle};
use lyrabot_core::{ContentPart, EntryId, Message, Role};
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

/// 没有对应结果的工具调用在请求中补上的占位结果（进程在工具执行期间退出时会留下这种历史）。
pub const UNANSWERED_CALL: &str = "〔这次调用没有返回结果：服务在执行期间重启或中断，不确定是否已执行；需要时先核对再决定是否重做〕";

/// 上下文中的一条消息及其来源条目。
#[derive(Debug, Clone)]
pub struct CtxMessage {
    pub entry: Option<EntryId>,
    /// 落库时间（毫秒）；摘要、占位与尚未落库的输入为 `None`
    pub at: Option<i64>,
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
        out.push(CtxMessage { entry: None, at: None, message: Message::user(format!("{SUMMARY_PREFIX}{s}")) });
    }
    for e in &branch[start..] {
        if e.kind == EntryKind::Message {
            let mut message: Message = serde_json::from_value(e.payload.clone())?;
            if message.role == Role::Tool && fold_before.is_some_and(|b| e.id < b) {
                message.content = vec![ContentPart::text(FOLDED_TOOL)];
            }
            out.push(CtxMessage { entry: Some(e.id), at: Some(e.created_at), message });
        }
    }
    Ok(pad_unanswered_calls(out))
}

/// 为没有对应结果的工具调用补上占位结果，保证“assistant 调用 → 每个调用一条 tool 结果”的协议结构。
/// 只影响本次构建出的上下文，不改动记录；正常运行时内核总会写入全部结果，缺失只来自执行期间的进程退出。
fn pad_unanswered_calls(msgs: Vec<CtxMessage>) -> Vec<CtxMessage> {
    fn flush(out: &mut Vec<CtxMessage>, pending: &mut Vec<String>) {
        for id in pending.drain(..) {
            out.push(CtxMessage { entry: None, at: None, message: Message::tool_result(id, UNANSWERED_CALL, true) });
        }
    }
    let mut out = Vec::with_capacity(msgs.len());
    let mut pending: Vec<String> = Vec::new();
    for m in msgs {
        if m.message.role == Role::Tool {
            if let Some(id) = &m.message.tool_call_id {
                pending.retain(|p| p != id);
            }
        } else {
            flush(&mut out, &mut pending);
            if m.message.role == Role::Assistant {
                pending = m.message.tool_calls.iter().map(|c| c.id.clone()).collect();
            }
        }
        out.push(m);
    }
    flush(&mut out, &mut pending);
    out
}

/// 生成交给压缩模型的记录稿：图片、视频写成占位而不是留空；按策略给用户消息标注时间；
/// 聊天风格下工具结果截得更短（主对话角色的工具输出本就不是重点）。
pub fn transcript(msgs: &[CtxMessage], policy: &CompactPolicy) -> String {
    let mut out = String::new();
    for m in msgs {
        let who = match m.message.role {
            Role::User => "用户",
            Role::Assistant => "助手",
            Role::Tool => "工具结果",
        };
        let when = match (&policy.time_label, m.at) {
            (Some(label), Some(at)) if m.message.role == Role::User => format!("（{}）", label(at)),
            _ => String::new(),
        };
        let mut body = String::new();
        for p in &m.message.content {
            match p {
                ContentPart::Text { text } => body.push_str(text),
                ContentPart::Image { .. } => body.push_str("[图片]"),
                ContentPart::Video { url } => body.push_str(&format!("[视频：{}]", url.trim_start_matches("file://"))),
            }
        }
        for c in &m.message.tool_calls {
            body.push_str(&format!("\n[调用 {} {}]", c.name, clip(&c.arguments, 400)));
        }
        let limit = if policy.style == CompactStyle::Chat && m.message.role == Role::Tool { 1000 } else { 4000 };
        out.push_str(&format!("{who}{when}：{}\n", clip(&body, limit)));
    }
    out
}

/// 摘要胶囊各列表的条数与单条字数上限、渲染总长上限。
const CAPSULE_ITEMS: usize = 8;
const CAPSULE_ITEM_CHARS: usize = 150;
const CAPSULE_CHARS: usize = 1200;
const SUMMARY_CHARS: usize = 2000;

/// 把压缩模型的输出渲染成确定性的摘要文本：`{summary, capsule}` JSON 按固定标签渲染并限长；
/// 不是 JSON 时当作纯文本摘要；JSON 残缺时只取能解析出的 summary 字段，绝不把 JSON 残骸写进摘要。
pub fn render_compaction(output: &str) -> String {
    let body = output.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    if !body.starts_with('{') {
        return clip(body, SUMMARY_CHARS);
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        let salvaged = salvage_summary(body);
        if salvaged.is_empty() {
            tracing::warn!("压缩输出不是合法 JSON，也取不出 summary，按无摘要处理");
        } else {
            tracing::warn!("压缩输出不是完整的 JSON，只取用其中的 summary");
        }
        return clip(&salvaged, SUMMARY_CHARS);
    };
    let mut out = clip(v["summary"].as_str().unwrap_or_default().trim(), SUMMARY_CHARS);
    let cap = &v["capsule"];
    let mut capsule = String::new();
    if let Some(o) = cap["objective"].as_str().map(str::trim).filter(|o| !o.is_empty()) {
        capsule.push_str(&format!("【当前目标】{}\n", clip(o, CAPSULE_ITEM_CHARS)));
    }
    // 已执行的外部操作排在最前：胶囊整体限长截断时优先保住它
    for (key, label) in [
        ("side_effects", "已执行的外部操作（续接时不要重复）"),
        ("constraints", "用户的要求与约束"),
        ("decisions", "已确认的决定"),
        ("verification", "验证状态"),
        ("next_steps", "下一步"),
    ] {
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

/// 从残缺的 JSON 里取出 `"summary"` 字段的字符串值（如输出被截断、胶囊部分写坏）；取不到返回空串。
fn salvage_summary(body: &str) -> String {
    let Some(rest) = body.find("\"summary\"").map(|i| body[i + "\"summary\"".len()..].trim_start()) else {
        return String::new();
    };
    let Some(rest) = rest.strip_prefix(':').map(str::trim_start) else {
        return String::new();
    };
    serde_json::Deserializer::from_str(rest).into_iter::<String>().next().and_then(Result::ok).map(|s| s.trim().to_owned()).unwrap_or_default()
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
/// 往前找 user 边界会让保留的尾部超过 `keep_tokens` 两倍时（如续接任务里较早的 user 之后跟着大量工具输出），
/// 改在离预算最近的 assistant 工具轮次起点（`i >= 2`）切分，保证工具调用与其结果不会被拆开。
/// 切分点之前必须有真实条目：只剩上次压缩的合成摘要时，再压一次也不会变短，返回 `None` 放弃本次压缩。
pub fn split_point(msgs: &[CtxMessage], keep_tokens: u64) -> Option<usize> {
    let first_stored = msgs.iter().position(|m| m.entry.is_some())?;
    let mut acc = 0;
    let mut user_cand = None;
    // 第一个满足保留预算的 assistant 轮次边界
    let mut turn_cand = None;
    for (i, m) in msgs.iter().enumerate().rev() {
        acc += estimate_tokens(m.message.char_len());
        if m.entry.is_none() || i <= first_stored {
            continue;
        }
        match m.message.role {
            Role::User => {
                if acc >= keep_tokens {
                    return Some(i);
                }
                user_cand = Some(i);
            }
            Role::Assistant if i >= 2 && acc >= keep_tokens => {
                turn_cand.get_or_insert(i);
                if acc >= keep_tokens.saturating_mul(2) {
                    return turn_cand;
                }
            }
            _ => {}
        }
    }
    // 尾部始终不足预算时沿用最早的 user 边界（只压缩很短的前缀）
    turn_cand.or(user_cand)
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
        .map(|(i, m)| CtxMessage { at: None, entry: Some(i as i64), message: m })
        .collect();
        assert_eq!(split_point(&msgs, 50), Some(2));
        // 只有开头一条 user 消息（单次后台任务多轮工具调用）时，回退到 assistant 轮次边界切分，不拆散 call 与 tool_result
        let single_user_loop: Vec<CtxMessage> = [
            Message::user("任务：排查日志".repeat(20)),
            call.clone(),
            Message::tool_result("x", "第一轮工具输出".repeat(30), false),
            call.clone(),
            Message::tool_result("x", "第二轮工具输出".repeat(30), false),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, m)| CtxMessage { at: None, entry: Some(i as i64), message: m })
        .collect();
        assert_eq!(split_point(&single_user_loop, 50), Some(3));
        // 较早有 user、其后是大量工具输出：不能为了落在 user 上而保留整段巨大尾部
        let late_tools: Vec<CtxMessage> = [
            Message::user("开头"),
            Message::assistant("好"),
            Message::user("继续排查"),
            call.clone(),
            Message::tool_result("x", "第一轮工具输出".repeat(300), false),
            call,
            Message::tool_result("x", "第二轮工具输出".repeat(30), false),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, m)| CtxMessage { at: None, entry: Some(i as i64), message: m })
        .collect();
        assert_eq!(split_point(&late_tools, 50), Some(5));
        // 合成摘要 + 超长 user + 较短工具往返：切分点前只剩摘要时放弃，避免反复压缩旧摘要
        let after_summary = vec![
            CtxMessage { at: None, entry: None, message: Message::user(format!("{SUMMARY_PREFIX}旧摘要")) },
            CtxMessage { at: None, entry: Some(10), message: Message::user("长".repeat(300)) },
            CtxMessage { at: None, entry: Some(11), message: Message::assistant("好") },
        ];
        assert_eq!(split_point(&after_summary, 50), None);
        // 保留量超过全部内容时，没有可压缩前缀
        assert_eq!(split_point(&msgs[2..], 1_000_000), None);
    }

    fn call_msg(ids: &[&str]) -> Message {
        let mut m = Message::assistant("");
        for id in ids {
            m.tool_calls.push(lyrabot_core::ToolCall { id: (*id).into(), name: "t".into(), arguments: "{}".into() });
        }
        m
    }

    #[test]
    fn unanswered_calls_get_placeholder_results() {
        // 同批两个调用只落库了一个结果，随后是新的 user；以及末尾一条完全没有结果的调用（执行期间重启）
        let branch = vec![
            msg(1, Message::user("查一下")),
            msg(2, call_msg(&["a", "b"])),
            msg(3, Message::tool_result("a", "结果 a", false)),
            msg(4, Message::user("怎么样了")),
            msg(5, call_msg(&["c"])),
        ];
        let ctx = build(&branch).unwrap();
        let shape: Vec<(Role, Option<String>)> = ctx.iter().map(|c| (c.message.role, c.message.tool_call_id.clone())).collect();
        assert_eq!(
            shape,
            vec![
                (Role::User, None),
                (Role::Assistant, None),
                (Role::Tool, Some("a".into())),
                (Role::Tool, Some("b".into())),
                (Role::User, None),
                (Role::Assistant, None),
                (Role::Tool, Some("c".into())),
            ]
        );
        assert_eq!(ctx[3].message.text(), UNANSWERED_CALL);
        assert!(ctx[3].entry.is_none() && ctx[3].message.is_error, "占位不对应任何记录条目");
        // 结构完整的历史不受影响
        let ok = vec![msg(1, Message::user("q")), msg(2, call_msg(&["x"])), msg(3, Message::tool_result("x", "r", false))];
        assert_eq!(build(&ok).unwrap().len(), 3);
    }

    #[test]
    fn transcript_marks_media_and_user_times() {
        let mut img = Message::with_parts(Role::User, vec![ContentPart::Image { url: "file:///m/a.png".into() }]);
        img.tail = Some("〔当前时间〕".into());
        let msgs = vec![
            CtxMessage { entry: Some(1), at: Some(1_000), message: img },
            CtxMessage { entry: Some(2), at: Some(2_000), message: Message::with_parts(Role::User, vec![ContentPart::Video { url: "file:///m/v.mp4".into() }, ContentPart::text("看看")]) },
            CtxMessage { entry: Some(3), at: Some(3_000), message: Message::assistant("好") },
            CtxMessage { entry: Some(4), at: Some(4_000), message: Message::tool_result("x", "长".repeat(2000), false) },
        ];
        let chat = CompactPolicy { style: CompactStyle::Chat, time_label: Some(std::sync::Arc::new(|ms| format!("t{ms}"))), ..Default::default() };
        let t = transcript(&msgs, &chat);
        assert!(t.contains("用户（t1000）：[图片]\n"), "{t}");
        assert!(t.contains("用户（t2000）：[视频：/m/v.mp4]看看\n"), "{t}");
        assert!(t.contains("助手：好\n"), "只给用户消息标时间：{t}");
        assert!(!t.contains("〔当前时间〕"), "记录稿用原话，不含尾注入");
        let tool_line = t.lines().find(|l| l.starts_with("工具结果")).unwrap();
        assert!(tool_line.chars().count() < 1100, "聊天风格下工具结果截短");
        let task = transcript(&msgs, &CompactPolicy::default());
        assert!(task.contains("用户：[图片]") && task.lines().find(|l| l.starts_with("工具结果")).unwrap().chars().count() > 1900);
    }

    #[test]
    fn broken_json_keeps_summary_and_renders_side_effects() {
        assert_eq!(render_compaction(r#"{"summary": "聊了猫咪年糕", "capsule": {"objective": "#), "聊了猫咪年糕");
        assert_eq!(render_compaction(r#"{"capsule": {"#), "", "取不出 summary 时按无摘要处理");
        let full = render_compaction(r#"{"summary": "s", "capsule": {"objective": "部署", "side_effects": ["已重启服务"]}}"#);
        assert!(full.contains("【已执行的外部操作（续接时不要重复）】\n- 已重启服务"), "{full}");
    }
}
