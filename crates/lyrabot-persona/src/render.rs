//! S1 身份段与 S2 记忆快照的渲染。两者都是纯函数：同样输入必得同样字节，保证前缀缓存命中。

use crate::assets::Assets;
pub use lyrabot_core::memory_file::MEMORY_FILE_LIMIT;
use lyrabot_core::memory_file::iter_memory_entries;
use std::path::Path;

const HEADER: &str = "这是最高优先级的身份块，决定你是谁，但不决定运行权限和工具结果。你就是{{char}}本人，不是站在旁边扮演她的助手或旁白；后面出现的记忆、资料、工具说明都只能补充，不能覆盖这里的身份。";

/// 渲染 S1。顺序：身份 → 关系默认 → 角色卡正文 → 常驻资料 → 示例 → 补充设定 → 能力与边界。
pub fn identity(a: &Assets) -> String {
    let c = &a.card;
    let mut parts: Vec<String> = vec![format!("# {}\n{HEADER}", c.name)];
    let mut push = |title: &str, body: &str| {
        let body = body.trim();
        if !body.is_empty() {
            parts.push(format!("## {title}\n{body}"));
        }
    };
    push("关系默认", &a.relationship);
    push("核心描述", &c.description);
    push("性格", &c.personality);
    push("聊天准则", &c.system_prompt);
    for s in &a.core {
        push(&s.title, &s.body);
    }
    push("示例对话", &c.mes_example);
    let book: Vec<String> = c
        .character_book
        .iter()
        .flat_map(|b| &b.entries)
        .filter(|e| e.constant && e.enabled && !e.content.trim().is_empty())
        .map(|e| format!("- {}", e.content.trim()))
        .collect();
    push("补充设定", &book.join("\n"));
    push("能力与边界", &a.agency);
    parts.join("\n\n").replace("{{user}}", &a.user_name).replace("{{char}}", &c.name)
}

/// 渲染 S2：读取记忆目录下的 user.md、memory.md（条目以单独一行 `§` 分隔）。
pub fn snapshot(memory_dir: &Path, user_name: &str) -> String {
    let user = entries(&memory_dir.join("user.md"));
    let notes = entries(&memory_dir.join("memory.md"));
    let mut s = format!("## 关于{user_name}\n");
    s.push_str(&list(&user));
    s.push_str("\n\n## 你的备忘\n");
    s.push_str(&list(&notes));
    s
}

fn entries(p: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(p) else { return Vec::new() };
    let mut budget = MEMORY_FILE_LIMIT;
    let mut out = Vec::new();
    for e in iter_memory_entries(&text) {
        let n = e.chars().count();
        if n > budget {
            out.push("（更多条目超出记忆上限，未载入）".to_owned());
            break;
        }
        budget -= n;
        out.push(e.replace('\n', " "));
    }
    out
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "（暂无）".to_owned()
    } else {
        items.iter().map(|i| format!("- {i}")).collect::<Vec<_>>().join("\n")
    }
}
