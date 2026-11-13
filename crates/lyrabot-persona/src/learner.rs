//! 学习（docs/04-persona-memory.md 第 3 节）：表达、黑话与长期记忆候选。
//!
//! - 后台：一段新对话交给低价模型，一次调用同时提取用户的专属说法与记忆候选；说法入库并衰减，
//!   记忆候选入队等待空闲整理（见 [`crate::consolidate`]）；
//! - 前台：[`Index`] 在本地做触发词 + 中文单字集合匹配，挑出至多 3 条渲染为尾注入，热路径不调用模型。

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use lyrabot_core::{Effort, Message, Role};
use lyrabot_llm::{ChatRequest, Provider};
use lyrabot_store::{Expression, LearnedExpression};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use tokio_util::sync::CancellationToken;

const LEARN_PROMPT: &str = "你在帮{{char}}整理与{{user}}的私聊，让{{char}}以后更懂{{user}}。只输出一个 JSON 对象，不要解释：\
{\"expressions\": [...], \"memories\": [...]}。\n\
expressions：{{user}}的专属说法，只从【{{user}}】的话里提取，每项字段 kind（\"jargon\" 或 \"habit\"）、text（原文，≤20 字）、\
meaning（含义或该怎么理解回应，≤40 字）、situation（通常在什么情境下说，≤30 字）、triggers（1~3 个会原样出现在消息里的关键词）。\
jargon 是网络梗、缩写、谐音、自造词、你们之间的专属叫法与玩笑；habit 是{{user}}稳定的表达习惯（口头禅、称呼偏好、用某个词表达某种情绪）。最多 8 条。\n\
memories：值得长期记住的事，每项字段 target（\"user\" 关于{{user}}本人，\"memory\" 你们之间的约定与共同的事）、\
text（一句话，≤60 字，写清主语与关键细节）、basis（\"stated\" {{user}}亲口陈述，\"agreed\" {{user}}明确认可的约定，\"corrected\" {{user}}纠正过的地方）。\
只记{{user}}说过或明确认可的：{{char}}的话只用来理解上下文，{{char}}对自己的描述与剧情发挥一律不记。最多 6 条。\n\
两类都不要提取：普通词汇、一次性的任务细节与过程、已解决的问题、带具体时间的提醒（那由定时任务负责）、\
隐私凭据（地址、电话、账号、密码、密钥）。宁缺毋滥，没有值得记的就给空数组。";

const MAX_TRANSCRIPT_CHARS: usize = 6000;
/// 单批记忆候选上限。
const MAX_MEMORIES: usize = 6;
const MAX_MSG_CHARS: usize = 300;

/// 把消息整理成学习用的对话稿（只取文本；保留最新部分）。
pub fn transcript(msgs: &[Message], user_name: &str, char_name: &str) -> String {
    let mut lines: Vec<String> = msgs
        .iter()
        .filter_map(|m| {
            let who = match m.role {
                Role::User => user_name,
                Role::Assistant => char_name,
                Role::Tool => return None,
            };
            let t = m.text();
            let t = t.trim();
            if t.is_empty() || t.starts_with(lyrabot_agent::context::SUMMARY_PREFIX.trim_end()) {
                return None;
            }
            let t: String = t.chars().take(MAX_MSG_CHARS).collect();
            Some(format!("【{who}】{}", t.replace('\n', " ")))
        })
        .collect();
    // 从最新往前保留，直到达到总长度上限
    let mut total = 0;
    let mut keep = lines.len();
    for (i, l) in lines.iter().enumerate().rev() {
        total += l.chars().count() + 1;
        if total > MAX_TRANSCRIPT_CHARS {
            break;
        }
        keep = i;
    }
    lines.drain(..keep);
    lines.join("\n")
}

#[derive(Deserialize)]
struct RawItem {
    kind: String,
    text: String,
    #[serde(default)]
    meaning: Option<String>,
    #[serde(default)]
    situation: Option<String>,
    #[serde(default)]
    triggers: Vec<String>,
}

fn clip(s: &str, n: usize) -> String {
    s.trim().chars().take(n).collect()
}

/// 一条记忆候选：目标文件（user / memory）、内容、依据（stated / agreed / corrected）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryNote {
    pub target: String,
    pub text: String,
    pub basis: String,
}

/// 一次学习的结果。
#[derive(Debug, Default, PartialEq)]
pub struct Learned {
    pub expressions: Vec<LearnedExpression>,
    pub memories: Vec<MemoryNote>,
}

#[derive(Deserialize)]
struct RawLearned {
    #[serde(default)]
    expressions: Vec<RawItem>,
    #[serde(default)]
    memories: Vec<RawMemory>,
}

#[derive(Deserialize)]
struct RawMemory {
    target: String,
    text: String,
    #[serde(default)]
    basis: Option<String>,
}

/// 解析模型输出：容忍代码块包裹；兼容旧版只有说法数组的输出；丢弃不合规的条目。
/// 输出不是合法 JSON 时返回 `None`，与“解析成功但没学到东西”区分开（后者不应推进衰减以外的任何判断）。
pub fn parse(output: &str) -> Option<Learned> {
    let body = output.trim();
    let raw = match (body.find('{'), body.find('[')) {
        // 对象在数组之前出现：新格式
        (Some(o), a) if a.is_none_or(|a| o < a) => body.rfind('}').filter(|&e| e > o).and_then(|e| serde_json::from_str::<RawLearned>(&body[o..=e]).ok()),
        (_, Some(a)) => body
            .rfind(']')
            .filter(|&e| e > a)
            .and_then(|e| serde_json::from_str::<Vec<RawItem>>(&body[a..=e]).ok())
            .map(|expressions| RawLearned { expressions, memories: Vec::new() }),
        _ => None,
    };
    let Some(raw) = raw else {
        tracing::warn!("学习输出不是合法 JSON");
        return None;
    };
    Some(Learned { expressions: parse_expressions(raw.expressions), memories: parse_memories(raw.memories) })
}

fn parse_memories(raw: Vec<RawMemory>) -> Vec<MemoryNote> {
    raw.into_iter()
        .filter(|m| matches!(m.target.as_str(), "user" | "memory"))
        .filter_map(|m| {
            // 依据必须是三种来源之一；缺失或无法识别的一律不记
            let basis = m.basis.filter(|b| matches!(b.as_str(), "stated" | "agreed" | "corrected"))?;
            let text = clip(&m.text, 60);
            (text.chars().count() >= 4).then_some(MemoryNote { target: m.target, text, basis })
        })
        .take(MAX_MEMORIES)
        .collect()
}

fn parse_expressions(raw: Vec<RawItem>) -> Vec<LearnedExpression> {
    raw.into_iter()
        .filter(|r| matches!(r.kind.as_str(), "jargon" | "habit"))
        .filter_map(|r| {
            let text = clip(&r.text, 20);
            if text.is_empty() {
                return None;
            }
            let mut triggers: Vec<String> =
                r.triggers.iter().map(|t| clip(t, 20)).filter(|t| t.chars().count() >= 2).take(3).collect();
            if triggers.is_empty() {
                triggers.push(text.clone());
            }
            Some(LearnedExpression {
                kind: r.kind,
                meaning: r.meaning.map(|m| clip(&m, 40)).filter(|m| !m.is_empty()),
                situation: r.situation.map(|s| clip(&s, 30)).unwrap_or_default(),
                triggers,
                text,
            })
        })
        .take(8)
        .collect()
}

/// 调用模型学习一段对话稿。
pub async fn learn(
    provider: &dyn Provider,
    model: &str,
    effort: Effort,
    persona: &crate::Persona,
    transcript: &str,
    cancel: CancellationToken,
) -> Result<Option<Learned>, lyrabot_llm::LlmError> {
    let sys = LEARN_PROMPT.replace("{{user}}", persona.user_name()).replace("{{char}}", persona.name());
    let req = ChatRequest {
        model: model.to_owned(),
        system: vec![sys],
        messages: vec![Message::user(transcript.to_owned())],
        tools: Vec::new(),
        effort: Some(effort),
        max_tokens: Some(4000),
        cache_key: None,
    };
    let c = provider.complete(req, cancel).await?;
    Ok(parse(&c.message.text()))
}

/// 本地检索索引（≤ 数百条，重建开销可忽略）。
#[derive(Default)]
pub struct Index {
    items: Vec<Expression>,
    ac: Option<AhoCorasick>,
    /// 模式下标 → 条目下标
    owner: Vec<usize>,
}

impl Index {
    pub fn build(items: Vec<Expression>) -> Self {
        let mut patterns = Vec::new();
        let mut owner = Vec::new();
        for (i, e) in items.iter().enumerate() {
            for t in &e.triggers {
                patterns.push(t.to_lowercase());
                owner.push(i);
            }
        }
        let ac = (!patterns.is_empty())
            .then(|| AhoCorasickBuilder::new().match_kind(MatchKind::LeftmostLongest).build(&patterns).ok())
            .flatten();
        Self { items, ac, owner }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 为本轮输入挑选至多 `max_items` 条提示，渲染后不超过 `max_chars` 字。
    pub fn hints(&self, query: &str, user_name: &str, max_items: usize, max_chars: usize) -> Option<String> {
        if self.items.is_empty() || query.trim().is_empty() {
            return None;
        }
        let q = query.to_lowercase();
        let mut hit: BTreeMap<usize, usize> = BTreeMap::new();
        if let Some(ac) = &self.ac {
            for m in ac.find_iter(&q) {
                *hit.entry(self.owner[m.pattern().as_usize()]).or_default() += 1;
            }
        }
        let qb = signal(&q);
        let mut scored: Vec<(f64, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let hits = hit.get(&i).copied().unwrap_or(0) as f64;
                let sim = jaccard(&qb, &signal(&format!("{}{}", e.situation, e.text).to_lowercase()));
                // 没有触发词命中时，要求情境相似度足够高，避免无关注入
                if hits == 0.0 && sim < 0.2 {
                    return None;
                }
                Some((hits * 2.0 + sim + e.weight * 0.1, i))
            })
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut parts = Vec::new();
        let mut used = 0;
        for (_, i) in scored.into_iter().take(max_items) {
            let e = &self.items[i];
            let p = match (e.kind.as_str(), &e.meaning) {
                ("habit", Some(m)) => format!("{user_name}习惯说“{}”（{m}）", e.text),
                ("habit", None) => format!("{user_name}习惯说“{}”", e.text),
                (_, Some(m)) => format!("「{}」={m}", e.text),
                (_, None) => format!("「{}」", e.text),
            };
            let n = p.chars().count();
            if used + n > max_chars {
                break;
            }
            used += n;
            parts.push(p);
        }
        (!parts.is_empty()).then(|| format!("〔你们之间的说法，自然理解即可，不必刻意套用：{}〕", parts.join("；")))
    }
}

/// 高频虚字，不作为匹配信号。
const STOP_CHARS: &str = "我你他她它们的了是不有在和就都很也吗吧呢啊呀哦嗯这那个要让被把从到于对向与及或之着过又还么什怎样一上下里会能可";

/// 匹配信号：中文取有意义的单字集合（对语序不敏感，“我家猫”能匹配“猫叫年糕”），英文取 3 字以上的词。
fn signal(s: &str) -> HashSet<String> {
    let mut out: HashSet<String> = s
        .chars()
        .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c) && !STOP_CHARS.contains(*c))
        .map(String::from)
        .collect();
    out.extend(s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| w.len() >= 3).map(str::to_owned));
    out
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = (a.len() + b.len()) as f64 - inter;
    inter / union
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(id: i64, kind: &str, text: &str, meaning: &str, triggers: &[&str]) -> Expression {
        Expression {
            id,
            kind: kind.into(),
            text: text.into(),
            meaning: Some(meaning.into()),
            situation: "说事情搞砸了".into(),
            triggers: triggers.iter().map(|s| s.to_string()).collect(),
            hits: 1,
            weight: 1.0,
        }
    }

    #[test]
    fn parse_tolerates_fences_and_filters() {
        let out = "```json\n[{\"kind\":\"jargon\",\"text\":\"寄了\",\"meaning\":\"失败\",\"situation\":\"搞砸\",\"triggers\":[\"寄了\",\"寄\"]},\
                   {\"kind\":\"other\",\"text\":\"x\"},{\"kind\":\"habit\",\"text\":\"  \"},\
                   {\"kind\":\"habit\",\"text\":\"小歌\",\"triggers\":[]}]\n```";
        let v = parse(out).unwrap().expressions;
        assert_eq!(v.len(), 2, "兼容旧版纯数组输出");
        assert_eq!(v[0].triggers, vec!["寄了"], "单字触发词被丢弃");
        assert_eq!(v[1].triggers, vec!["小歌"], "缺触发词时用原文");
        assert_eq!(parse("不是 JSON"), None, "解析失败与空结果区分开");
        assert_eq!(parse("[]"), Some(Learned::default()));
    }

    #[test]
    fn parse_expressions_and_memories() {
        let out = r#"{"expressions": [{"kind":"jargon","text":"寄了","triggers":["寄了"]}],
            "memories": [{"target":"user","text":"用户的猫叫年糕","basis":"stated"},
                         {"target":"memory","text":"约好每周日晚上一起复盘","basis":"agreed"},
                         {"target":"other","text":"无效目标"},
                         {"target":"user","text":"短"},
                         {"target":"user","text":"用户改用北京时间","basis":"guess"},
                         {"target":"user","text":"用户住在杭州"}]}"#;
        let l = parse(out).unwrap();
        assert_eq!(l.expressions.len(), 1);
        assert_eq!(
            l.memories.iter().map(|m| (m.target.as_str(), m.basis.as_str())).collect::<Vec<_>>(),
            vec![("user", "stated"), ("memory", "agreed")],
            "无效目标、过短条目、未知或缺失依据都丢弃"
        );
    }

    #[test]
    fn transcript_keeps_latest_and_skips_tools() {
        let mut msgs = vec![Message::user("早"), Message::assistant("早呀")];
        msgs.push(Message::tool_result("c", "工具输出", false));
        let t = transcript(&msgs, "用户", "主对话角色");
        assert_eq!(t, "【用户】早\n【主对话角色】早呀");
        let long: Vec<Message> = (0..100).map(|i| Message::user(format!("{i}{}", "字".repeat(200)))).collect();
        let t = transcript(&long, "用户", "主对话角色");
        assert!(t.chars().count() <= MAX_TRANSCRIPT_CHARS);
        assert!(t.ends_with(&"字".repeat(200)) && t.contains("【用户】99"));
    }

    #[test]
    fn hints_by_trigger_and_budget() {
        let idx = Index::build(vec![
            expr(1, "jargon", "寄了", "失败、搞砸", &["寄了"]),
            expr(2, "habit", "绷不住", "觉得好笑", &["绷不住"]),
            expr(3, "jargon", "毫无关系", "无关", &["火星文"]),
        ]);
        let h = idx.hints("今天的面试彻底寄了", "用户", 3, 200).unwrap();
        assert!(h.contains("「寄了」=失败、搞砸"), "{h}");
        assert!(!h.contains("火星文") && !h.contains("毫无关系"));
        assert!(idx.hints("天气不错", "用户", 3, 200).is_none());
        // 单字信号对语序不敏感：情境“说事情搞砸了”与“事情又搞砸”能匹配上
        assert!(idx.hints("这件事情又搞砸了", "用户", 3, 200).is_some());
        assert!(idx.hints("寄了 绷不住", "用户", 3, 5).is_none(), "超出字数预算时不注入");
        assert!(Index::default().hints("寄了", "用户", 3, 200).is_none());
    }
}
