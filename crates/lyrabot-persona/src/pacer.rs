//! 回复节奏（纯逻辑，无 IO）：入站消息在不同阶段的处置、回复分段与段间延迟。
//! 计时与发送由运行时负责，这里只做可测试的决策。

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct PacerConfig {
    /// 去抖窗口：最后一条消息后等待多久再开始一轮
    pub debounce: Duration,
    /// 从第一条消息算起最多等待多久
    pub max_wait: Duration,
    /// 一次回复最多拆成几段（受 QQ 被动回复次数限制）
    pub max_segments: usize,
}

impl Default for PacerConfig {
    fn default() -> Self {
        Self { debounce: Duration::from_millis(1800), max_wait: Duration::from_secs(6), max_segments: 4 }
    }
}

impl PacerConfig {
    /// 距第一条消息已过 `since_first`，下一次触发还需等待多久。
    pub fn wait(&self, since_first: Duration) -> Duration {
        self.debounce.min(self.max_wait.saturating_sub(since_first))
    }
}

/// 主对话角色会话当前所处阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    /// 已收到消息，等待去抖结束
    Debouncing,
    /// 模型生成中，尚未调用工具、尚未发出任何内容
    Generating,
    /// 本轮已进入工具执行
    Tools,
    /// 本轮已结束，正在分段发送
    Delivering,
}

/// 新消息到达时的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// 放入待处理缓冲并（重新）计时
    Buffer,
    /// 取消当前生成，与缓冲合并后重来
    CancelAndMerge,
    /// 作为插话注入当前轮
    Steer,
}

pub fn on_message(phase: Phase) -> Disposition {
    match phase {
        Phase::Idle | Phase::Debouncing | Phase::Delivering => Disposition::Buffer,
        Phase::Generating => Disposition::CancelAndMerge,
        Phase::Tools => Disposition::Steer,
    }
}

/// 把一条回复拆成至多 `max` 段：优先按空行分段；单段过长再按句末标点切；
/// 含代码块时不拆，避免破坏格式。
pub fn segments(text: &str, max: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    if text.contains("```") || max <= 1 {
        return vec![text.to_owned()];
    }
    let mut parts: Vec<String> = text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned).collect();
    if parts.len() == 1 && parts[0].chars().count() > 120 {
        parts = split_sentences(&parts[0]);
    }
    merge_to(parts, max)
}

/// 按句末标点切句（保留标点）。
fn split_sentences(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        cur.push(c);
        if matches!(c, '。' | '！' | '？' | '!' | '?' | '~' | '～') {
            // 连续标点（如“？！”）、右引号与右括号归到当前句
            while let Some(&n) = chars.peek() {
                if matches!(n, '。' | '！' | '？' | '!' | '?' | '”' | '’' | '）' | ')' | '~' | '～') {
                    cur.push(n);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(std::mem::take(&mut cur).trim().to_owned());
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_owned());
    }
    out.retain(|p| !p.is_empty());
    out
}

/// 把若干小段合并到不超过 `max` 段：每次合并相邻且合计最短的两段，尽量均衡。
fn merge_to(mut parts: Vec<String>, max: usize) -> Vec<String> {
    while parts.len() > max {
        let (i, _) = parts
            .windows(2)
            .enumerate()
            .map(|(i, w)| (i, w[0].chars().count() + w[1].chars().count()))
            .min_by_key(|&(_, n)| n)
            .expect("至少两段");
        let next = parts.remove(i + 1);
        let sep = if parts[i].ends_with(['。', '！', '？', '!', '?', '~', '～', '”']) { "" } else { "\n" };
        parts[i] = format!("{}{sep}{next}", parts[i]);
    }
    parts
}

/// 发送下一段前的停顿：模拟打字，基础 400ms + 每字 30ms，上限 2.5s。
pub fn segment_delay(next: &str) -> Duration {
    let ms = 400 + 30 * next.chars().count() as u64;
    Duration::from_millis(ms.min(2500))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispositions() {
        assert_eq!(on_message(Phase::Idle), Disposition::Buffer);
        assert_eq!(on_message(Phase::Generating), Disposition::CancelAndMerge);
        assert_eq!(on_message(Phase::Tools), Disposition::Steer);
        assert_eq!(on_message(Phase::Delivering), Disposition::Buffer);
    }

    #[test]
    fn wait_respects_cap() {
        let c = PacerConfig::default();
        assert_eq!(c.wait(Duration::ZERO), Duration::from_millis(1800));
        assert_eq!(c.wait(Duration::from_secs(5)), Duration::from_secs(1));
        assert_eq!(c.wait(Duration::from_secs(9)), Duration::ZERO);
    }

    #[test]
    fn short_reply_single_segment() {
        assert_eq!(segments("在呢，怎么啦？", 4), vec!["在呢，怎么啦？"]);
        assert!(segments("  ", 4).is_empty());
    }

    #[test]
    fn paragraphs_split_and_merge() {
        let s = segments("一。\n\n二。\n\n三。\n\n四。\n\n五五五五五五。", 4);
        assert_eq!(s.len(), 4);
        assert_eq!(s[0], "一。二。");
        assert_eq!(s[3], "五五五五五五。");
    }

    #[test]
    fn long_paragraph_split_by_sentence() {
        let p = "这是第一句话，稍微长一点点，让它超过阈值。".repeat(3) + "最后一句“对吧？”" + &"补充的内容很长很长很长。".repeat(6);
        let s = segments(&p, 4);
        assert!(s.len() > 1 && s.len() <= 4, "{s:?}");
        assert_eq!(s.concat(), p.replace('\n', ""), "拆分不能丢字");
    }

    #[test]
    fn code_block_not_split() {
        let t = "看这个：\n\n```\nfn main() {}\n```\n\n好了";
        assert_eq!(segments(t, 4).len(), 1);
    }

    #[test]
    fn delay_bounds() {
        assert_eq!(segment_delay(""), Duration::from_millis(400));
        assert_eq!(segment_delay(&"字".repeat(200)), Duration::from_millis(2500));
    }
}
