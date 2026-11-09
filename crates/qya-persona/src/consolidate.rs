//! 记忆整理（借鉴 Sodium 的两阶段闸门与 Anthropic dreaming）：
//! 学习器提取的候选先入队，空闲时批量整理进 `user.md` / `memory.md`。
//!
//! - 第一阶段（不花钱）：与现有条目及同批候选做规范化精确去重；
//! - 第二阶段（一次低价调用）：模型只给出增、改、删操作，由本模块确定性地应用；
//!   每个文件守住字数上限，单次删除有上限，超限的新增按先后放弃；
//! - 调用方在写文件前保存修订（整理前后全文），供 `/learn` 查看与撤销。

use crate::MEMORY_FILE_LIMIT;
use qya_core::{Effort, Message};
use qya_llm::{ChatRequest, Provider};
use serde::Deserialize;
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

/// 记忆文件中条目的分隔符（单独一行 `§`）。
pub const SEP: &str = "\n§\n";
/// 单次整理最多删除的条数，防止误判导致大面积丢失。
const MAX_REMOVALS: usize = 3;
/// 单条记忆上限（字）。
const ENTRY_CHARS: usize = 80;

/// 两份记忆文件：user（关于用户）与 memory（你们之间的约定与共同的事）。
pub const TARGETS: [(&str, char); 2] = [("user", 'u'), ("memory", 'm')];

/// 按当前用户名生成记忆分类标题。
pub fn target_label(target: &str, user_name: &str) -> String {
    if target == "user" { format!("关于{user_name}") } else { "约定与共同的事".to_owned() }
}

pub fn entries(text: &str) -> Vec<String> {
    text.split(SEP).map(str::trim).filter(|e| !e.is_empty()).map(str::to_owned).collect()
}

pub fn render(list: &[String]) -> String {
    if list.is_empty() { String::new() } else { format!("{}\n", list.join(SEP)) }
}

/// 一条待整理的候选（`id` 为候选在存储中的编号）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: i64,
    pub target: String,
    pub text: String,
    pub basis: String,
}

/// 整理结果：新的文件条目、变更说明、被处理的候选编号。
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    pub files: BTreeMap<String, Vec<String>>,
    pub changes: Vec<String>,
    pub processed: Vec<i64>,
}

/// 去重键：只去掉空白与句末标点并转小写，保留词内有意义的符号（C++ 与 C# 不同）。
fn norm(s: &str) -> String {
    let flat: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    flat.trim_end_matches(['。', '.', '！', '!', '？', '?', '；', ';', '，', ',']).to_lowercase()
}

/// 第一阶段：去掉与现有条目或同批更早候选规范化后完全相同的候选。返回（保留的，丢弃的编号）。
pub fn prefilter(files: &BTreeMap<String, Vec<String>>, cands: Vec<Candidate>) -> (Vec<Candidate>, Vec<i64>) {
    let mut seen: Vec<String> = files.values().flatten().map(|e| norm(e)).collect();
    let (mut keep, mut dropped) = (Vec::new(), Vec::new());
    for c in cands {
        let n = norm(&c.text);
        if n.is_empty() || seen.contains(&n) {
            dropped.push(c.id);
        } else {
            seen.push(n);
            keep.push(c);
        }
    }
    (keep, dropped)
}

const PROMPT: &str = "你负责整理{{char}}的长期记忆。输入是两份现有记忆（u 开头是关于{{user}}的条目，m 开头是你们之间的约定与共同的事）和一批新候选。\
逐条判断每个候选：\n\
- 已有条目表达的是同一件事、没有新信息：不输出任何操作；\n\
- 与已有条目是同一件事但有补充或更新：update 该条，text 写合并后的完整一句；\n\
- 与已有条目矛盾：以候选（{{user}}最新的说法）为准，update 旧条，确实作废时 remove；\n\
- 新的事：add 到 target 对应的文件。\n\
每条一句话、不超过 60 字，写清主语和关键细节；不要编造候选里没有的内容；不要改动与候选无关的条目。\
只输出 JSON 数组，不要解释：[{\"op\":\"add\",\"target\":\"user\",\"text\":\"…\"},{\"op\":\"update\",\"id\":\"u2\",\"text\":\"…\"},{\"op\":\"remove\",\"id\":\"m1\"}]；没有需要的改动就输出 []。";

/// 给模型的输入：带编号的现有条目与候选。
pub fn prompt_input(files: &BTreeMap<String, Vec<String>>, cands: &[Candidate], user_name: &str) -> String {
    let mut s = String::new();
    for (name, prefix) in TARGETS {
        let label = target_label(name, user_name);
        s.push_str(&format!("【现有记忆：{label}（{name}）】\n"));
        let list = files.get(name).map(Vec::as_slice).unwrap_or_default();
        if list.is_empty() {
            s.push_str("（空）\n");
        }
        for (i, e) in list.iter().enumerate() {
            s.push_str(&format!("{prefix}{} {e}\n", i + 1));
        }
    }
    s.push_str("【新候选】\n");
    for c in cands {
        s.push_str(&format!("- target={} basis={} {}\n", c.target, c.basis, c.text));
    }
    s
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Op {
    Add { target: String, text: String },
    Update { id: String, text: String },
    Remove { id: String },
}

pub fn parse_ops(output: &str) -> Option<Vec<Op>> {
    let body = output.trim();
    let (a, b) = (body.find('[')?, body.rfind(']')?);
    if b < a {
        return None;
    }
    // 任何一项无法解释都视为整批无效（候选保留重试），避免静默丢掉候选
    serde_json::from_str::<Vec<Op>>(&body[a..=b]).ok()
}

fn locate(id: &str) -> Option<(&'static str, usize)> {
    let mut cs = id.trim().chars();
    let prefix = cs.next()?;
    let n: usize = cs.as_str().parse().ok()?;
    let name = TARGETS.iter().find(|(_, p)| *p == prefix)?.0;
    n.checked_sub(1).map(|i| (name, i))
}

fn clip(s: &str) -> String {
    s.trim().replace(SEP, "；").chars().take(ENTRY_CHARS).collect()
}

/// 第二阶段的确定性应用：先按原编号处理改与删，再追加新增；守住删除条数与每个文件的字数上限。
pub fn apply(files: &BTreeMap<String, Vec<String>>, ops: Vec<Op>, user_name: &str) -> (BTreeMap<String, Vec<String>>, Vec<String>) {
    let mut out = files.clone();
    for (name, _) in TARGETS {
        out.entry(name.to_owned()).or_default();
    }
    let mut changes = Vec::new();
    let mut removed: Vec<(&str, usize)> = Vec::new();
    let mut adds: Vec<(String, String)> = Vec::new();
    for op in ops {
        match op {
            Op::Update { id, text } => {
                let Some((name, i)) = locate(&id) else { continue };
                let text = clip(&text);
                if let Some(slot) = out.get_mut(name).and_then(|l| l.get_mut(i))
                    && !text.is_empty()
                    && *slot != text
                {
                    changes.push(format!("修改 {id}：{slot} → {text}"));
                    *slot = text;
                }
            }
            Op::Remove { id } => {
                let Some((name, i)) = locate(&id) else { continue };
                if removed.len() < MAX_REMOVALS && out.get(name).is_some_and(|l| i < l.len()) && !removed.contains(&(name, i)) {
                    removed.push((name, i));
                }
            }
            Op::Add { target, text } => {
                let text = clip(&text);
                if TARGETS.iter().any(|(n, _)| *n == target) && !text.is_empty() {
                    adds.push((target, text));
                }
            }
        }
    }
    // 删除按下标从大到小进行，避免错位
    removed.sort_by_key(|r| std::cmp::Reverse(r.1));
    for (name, i) in removed {
        let list = out.get_mut(name).expect("目标已初始化");
        let old = list.remove(i);
        let prefix = TARGETS.iter().find(|(n, _)| *n == name).map_or('?', |t| t.1);
        changes.push(format!("删除 {prefix}{}：{old}", i + 1));
    }
    for (target, text) in adds {
        let list = out.get_mut(&target).expect("目标已校验");
        let total: usize = list.iter().map(|e| e.chars().count()).sum::<usize>() + text.chars().count();
        if list.contains(&text) {
            continue;
        }
        if total > MEMORY_FILE_LIMIT {
            changes.push(format!("未新增（{target} 已接近 {MEMORY_FILE_LIMIT} 字上限）：{text}"));
            continue;
        }
        let label = target_label(&target, user_name);
        changes.push(format!("新增（{label}）：{text}"));
        list.push(text);
    }
    // 修改使文件超限时，该文件整体保持原样，并撤掉它的变更说明
    for (name, prefix) in TARGETS {
        let label = target_label(name, user_name);
        let total: usize = out[name].iter().map(|e| e.chars().count()).sum();
        if total > MEMORY_FILE_LIMIT {
            out.insert(name.to_owned(), files.get(name).cloned().unwrap_or_default());
            let (edit, del, add) = (format!("修改 {prefix}"), format!("删除 {prefix}"), format!("（{label}）"));
            changes.retain(|c| !c.starts_with(&edit) && !c.starts_with(&del) && !c.contains(&add));
            changes.push(format!("{label} 整理后会超过 {MEMORY_FILE_LIMIT} 字，本次保持原样"));
        }
    }
    (out, changes)
}

/// 整理一批候选：第一阶段免费去重，有剩余时再调用一次低价模型。模型失败时保留候选，等下次再试。
pub async fn consolidate(
    provider: &dyn Provider,
    model: &str,
    effort: Effort,
    persona: &crate::Persona,
    files: &BTreeMap<String, Vec<String>>,
    cands: Vec<Candidate>,
    cancel: CancellationToken,
) -> Result<Outcome, qya_llm::LlmError> {
    let (keep, mut processed) = prefilter(files, cands);
    if keep.is_empty() {
        return Ok(Outcome { files: files.clone(), changes: Vec::new(), processed });
    }
    let user_name = persona.user_name();
    let char_name = persona.name();
    let sys = PROMPT.replace("{{user}}", user_name).replace("{{char}}", char_name);
    let req = ChatRequest {
        model: model.to_owned(),
        system: vec![sys],
        messages: vec![Message::user(prompt_input(files, &keep, user_name))],
        tools: Vec::new(),
        effort: Some(effort),
        max_tokens: Some(4000),
    };
    let reply = provider.complete(req, cancel).await?.message.text();
    let Some(ops) = parse_ops(&reply) else {
        tracing::warn!("记忆整理输出不是合法 JSON，候选保留到下次");
        return Ok(Outcome { files: files.clone(), changes: Vec::new(), processed });
    };
    let (files, changes) = apply(files, ops, user_name);
    processed.extend(keep.iter().map(|c| c.id));
    Ok(Outcome { files, changes, processed })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(user: &[&str], memory: &[&str]) -> BTreeMap<String, Vec<String>> {
        BTreeMap::from([
            ("user".to_owned(), user.iter().map(|s| s.to_string()).collect()),
            ("memory".to_owned(), memory.iter().map(|s| s.to_string()).collect()),
        ])
    }

    fn cand(id: i64, target: &str, text: &str) -> Candidate {
        Candidate { id, target: target.into(), text: text.into(), basis: "stated".into() }
    }

    #[test]
    fn prefilter_drops_exact_duplicates() {
        let f = files(&["阿颜喜欢少糖奶茶。", "阿颜常用 C++"], &[]);
        let (keep, dropped) = prefilter(
            &f,
            vec![cand(1, "user", "阿颜喜欢少糖奶茶"), cand(2, "user", "阿颜养了一只猫"), cand(3, "user", "阿颜养了 一只猫！"), cand(4, "user", "阿颜常用 C#")],
        );
        assert_eq!(dropped, vec![1, 3]);
        assert_eq!(keep.iter().map(|c| c.id).collect::<Vec<_>>(), vec![2, 4], "C# 与 C++ 不是同一条");
    }

    #[test]
    fn apply_update_remove_add_with_limits() {
        let f = files(&["阿颜喜欢少糖奶茶", "阿颜在上海"], &["约好周日复盘"]);
        assert!(parse_ops(r#"[{"op":"add","target":"user","text":"甲"},{"op":"merge","id":"u1"}]"#).is_none(), "含无法解释的操作时整批无效");
        let ops = parse_ops(
            r#"[{"op":"update","id":"u1","text":"阿颜改喝无糖奶茶"},{"op":"remove","id":"u2"},
                {"op":"add","target":"user","text":"阿颜搬到了杭州"},{"op":"add","target":"other","text":"无效"},
                {"op":"update","id":"u9","text":"越界"},{"op":"remove","id":"x1"}]"#,
        )
        .unwrap();
        let (out, changes) = apply(&f, ops, "阿颜");
        assert_eq!(out["user"], vec!["阿颜改喝无糖奶茶", "阿颜搬到了杭州"]);
        assert_eq!(out["memory"], vec!["约好周日复盘"], "无关文件不变");
        assert_eq!(changes, vec!["修改 u1：阿颜喜欢少糖奶茶 → 阿颜改喝无糖奶茶", "删除 u2：阿颜在上海", "新增（关于阿颜）：阿颜搬到了杭州"]);
    }

    #[test]
    fn removals_are_capped_and_limit_is_kept() {
        let f = files(&["一一一一", "二二二二", "三三三三", "四四四四", "五五五五"], &[]);
        let ops = (1..=5).map(|i| Op::Remove { id: format!("u{i}") }).collect();
        let (out, _) = apply(&f, ops, "阿颜");
        assert_eq!(out["user"].len(), 2, "单次最多删除 3 条");
        let full = files(&[&"满".repeat(MEMORY_FILE_LIMIT - 2)], &[]);
        let (out, changes) = apply(&full, vec![Op::Add { target: "user".into(), text: "再加一条新内容".into() }], "阿颜");
        assert_eq!(out["user"].len(), 1);
        assert!(changes[0].starts_with("未新增"));
        // 修改让 user 超限：user 回滚且其修改说明撤掉，memory 的新增照常保留
        let near = files(&[&"满".repeat(MEMORY_FILE_LIMIT - 30), "短条目"], &[]);
        let (out, changes) = apply(
            &near,
            vec![
                Op::Update { id: "u2".into(), text: "改".repeat(70) },
                Op::Add { target: "memory".into(), text: "约好周日复盘".into() },
            ],
            "阿颜",
        );
        assert_eq!(out["user"], near["user"]);
        assert_eq!(out["memory"], vec!["约好周日复盘"]);
        assert!(!changes.iter().any(|c| c.starts_with("修改 u2")), "{changes:?}");
        assert!(changes.iter().any(|c| c.contains("（约定与共同的事）")));
    }

    #[test]
    fn entries_roundtrip() {
        let list = entries("甲\n§\n乙\n§\n\n");
        assert_eq!(list, vec!["甲", "乙"]);
        assert_eq!(render(&list), "甲\n§\n乙\n");
        assert_eq!(render(&[]), "");
        assert!(parse_ops("不是 JSON").is_none());
    }
}
