//! 表达与黑话学习（docs/01-architecture.md 6.3）。
//!
//! - 后台：把一段新对话交给低价模型，只从阿颜说的话里提取专属说法，结果入库并衰减；
//! - 前台：[`Index`] 在本地做触发词 + 字符二元组匹配，挑出至多 3 条渲染为尾注入，热路径不调用模型。

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use qya_core::{Effort, Message, Role};
use qya_llm::{ChatRequest, Provider};
use qya_store::{Expression, LearnedExpression};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use tokio_util::sync::CancellationToken;

const LEARN_PROMPT: &str = "你在帮唐语歌整理她和阿颜私聊里的“专属说法”，让她以后更懂阿颜。\
只从【阿颜】说的话里提取，唐语歌自己的话不算。输出 JSON 数组，每项字段：\
kind（\"jargon\" 或 \"habit\"）、text（说法原文，≤20 字）、meaning（含义或该怎么理解回应，≤40 字）、\
situation（通常在什么情境下说，≤30 字）、triggers（1~3 个会原样出现在消息里的关键词）。\n\
- jargon：网络梗、缩写、谐音、自造词、你们之间的专属叫法与玩笑。\n\
- habit：阿颜稳定的表达习惯，比如口头禅、对称呼的偏好、用某个词表达某种情绪。\n\
不要提取：普通词汇、一次性的具体事件或任务细节、隐私（地址、电话、账号、密码）。\
最多 8 条，宁缺毋滥；没有值得记的就输出 []。只输出 JSON，不要解释。";

const MAX_TRANSCRIPT_CHARS: usize = 6000;
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
            if t.is_empty() || t.starts_with(qya_agent::context::SUMMARY_PREFIX.trim_end()) {
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

/// 解析模型输出：容忍代码块包裹，丢弃不合规的条目。
pub fn parse(output: &str) -> Vec<LearnedExpression> {
    let (Some(a), Some(b)) = (output.find('['), output.rfind(']')) else { return Vec::new() };
    if b < a {
        return Vec::new();
    }
    let Ok(raw) = serde_json::from_str::<Vec<RawItem>>(&output[a..=b]) else {
        tracing::warn!("表达学习输出不是合法 JSON，忽略本批");
        return Vec::new();
    };
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
    transcript: &str,
    cancel: CancellationToken,
) -> Result<Vec<LearnedExpression>, qya_llm::LlmError> {
    let req = ChatRequest {
        model: model.to_owned(),
        system: vec![LEARN_PROMPT.to_owned()],
        messages: vec![Message::user(transcript.to_owned())],
        tools: Vec::new(),
        effort: Some(effort),
        max_tokens: Some(4000),
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
    pub fn hints(&self, query: &str, max_items: usize, max_chars: usize) -> Option<String> {
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
        let qb = bigrams(&q);
        let mut scored: Vec<(f64, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let hits = hit.get(&i).copied().unwrap_or(0) as f64;
                let sim = jaccard(&qb, &bigrams(&format!("{}{}", e.situation, e.text).to_lowercase()));
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
                ("habit", Some(m)) => format!("阿颜习惯说“{}”（{m}）", e.text),
                ("habit", None) => format!("阿颜习惯说“{}”", e.text),
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

fn bigrams(s: &str) -> HashSet<(char, char)> {
    let cs: Vec<char> = s.chars().filter(|c| !c.is_whitespace() && !c.is_ascii_punctuation()).collect();
    cs.windows(2).map(|w| (w[0], w[1])).collect()
}

fn jaccard(a: &HashSet<(char, char)>, b: &HashSet<(char, char)>) -> f64 {
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
        let v = parse(out);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].triggers, vec!["寄了"], "单字触发词被丢弃");
        assert_eq!(v[1].triggers, vec!["小歌"], "缺触发词时用原文");
        assert!(parse("不是 JSON").is_empty());
        assert!(parse("[]").is_empty());
    }

    #[test]
    fn transcript_keeps_latest_and_skips_tools() {
        let mut msgs = vec![Message::user("早"), Message::assistant("早呀")];
        msgs.push(Message::tool_result("c", "工具输出", false));
        let t = transcript(&msgs, "阿颜", "唐语歌");
        assert_eq!(t, "【阿颜】早\n【唐语歌】早呀");
        let long: Vec<Message> = (0..100).map(|i| Message::user(format!("{i}{}", "字".repeat(200)))).collect();
        let t = transcript(&long, "阿颜", "唐语歌");
        assert!(t.chars().count() <= MAX_TRANSCRIPT_CHARS);
        assert!(t.ends_with(&"字".repeat(200)) && t.contains("【阿颜】99"));
    }

    #[test]
    fn hints_by_trigger_and_budget() {
        let idx = Index::build(vec![
            expr(1, "jargon", "寄了", "失败、搞砸", &["寄了"]),
            expr(2, "habit", "绷不住", "觉得好笑", &["绷不住"]),
            expr(3, "jargon", "毫无关系", "无关", &["火星文"]),
        ]);
        let h = idx.hints("今天的面试彻底寄了", 3, 200).unwrap();
        assert!(h.contains("「寄了」=失败、搞砸"), "{h}");
        assert!(!h.contains("火星文") && !h.contains("毫无关系"));
        assert!(idx.hints("天气不错", 3, 200).is_none());
        assert!(idx.hints("寄了 绷不住", 3, 5).is_none(), "超出字数预算时不注入");
        assert!(Index::default().hints("寄了", 3, 200).is_none());
    }
}
