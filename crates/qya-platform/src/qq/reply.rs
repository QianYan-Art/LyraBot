//! 回复路由（纯逻辑）：被动回复 / 事件回复 / 主动消息 / 延迟投递。
//!
//! QQ 单聊规则（docs/01-architecture.md 9.1）：收到阿颜消息后一段时间内可以引用该消息 id 被动回复，
//! 单条消息有回复次数上限，msg_seq 在同一 msg_id 下递增；窗口外只能发主动消息，受许可与月度额度限制。

/// 一次发送应走的通道。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Passive { msg_id: String, seq: u32 },
    Event { event_id: String, seq: u32 },
    Proactive,
    /// 无可用通道：缓存到阿颜下次说话时再发
    Defer,
}

#[derive(Debug, Clone)]
struct Anchor {
    id: String,
    at_ms: i64,
    used: u32,
}

#[derive(Debug, Clone)]
pub struct ReplyState {
    msg: Option<Anchor>,
    event: Option<Anchor>,
    /// 最近一次主动消息许可事件的结论（默认允许，由接口错误纠正）
    pub proactive_allowed: bool,
    window_ms: i64,
    max_replies: u32,
}

impl ReplyState {
    pub fn new(window_min: u64, max_replies: u32) -> Self {
        Self { msg: None, event: None, proactive_allowed: true, window_ms: window_min as i64 * 60_000, max_replies }
    }

    /// 收到阿颜的新消息：以它为新的被动回复锚点。
    pub fn on_inbound(&mut self, msg_id: &str, now_ms: i64) {
        self.msg = Some(Anchor { id: msg_id.to_owned(), at_ms: now_ms, used: 0 });
    }

    /// 收到主动消息许可类事件（带 event_id，可在窗口内引用回复）。
    pub fn on_event(&mut self, event_id: &str, now_ms: i64, allowed: bool) {
        self.proactive_allowed = allowed;
        if !allowed {
            // 拒收或删除好友后，旧的事件锚点也不再可用
            self.event = None;
        } else if !event_id.is_empty() {
            self.event = Some(Anchor { id: event_id.to_owned(), at_ms: now_ms, used: 0 });
        }
    }

    /// 剩余的被动回复次数（窗口外为 0）。
    pub fn passive_left(&self, now_ms: i64) -> u32 {
        match &self.msg {
            Some(a) if now_ms - a.at_ms <= self.window_ms => self.max_replies.saturating_sub(a.used),
            _ => 0,
        }
    }

    /// 选择通道并占用一次额度。`quota_left` 表示本月主动消息是否还有余量。
    pub fn route(&mut self, now_ms: i64, quota_left: bool) -> Route {
        let (window, max) = (self.window_ms, self.max_replies);
        let fresh = |a: &Option<Anchor>| a.as_ref().is_some_and(|a| now_ms - a.at_ms <= window && a.used < max);
        if fresh(&self.msg) {
            let a = self.msg.as_mut().expect("已检查");
            a.used += 1;
            return Route::Passive { msg_id: a.id.clone(), seq: a.used };
        }
        if fresh(&self.event) {
            let a = self.event.as_mut().expect("已检查");
            a.used += 1;
            return Route::Event { event_id: a.id.clone(), seq: a.used };
        }
        if self.proactive_allowed && quota_left { Route::Proactive } else { Route::Defer }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passive_then_proactive_then_defer() {
        let mut s = ReplyState::new(60, 3);
        assert_eq!(s.route(0, true), Route::Proactive);
        s.on_inbound("m1", 1_000);
        assert_eq!(s.passive_left(1_000), 3);
        assert_eq!(s.route(1_000, true), Route::Passive { msg_id: "m1".into(), seq: 1 });
        assert_eq!(s.route(2_000, true), Route::Passive { msg_id: "m1".into(), seq: 2 });
        assert_eq!(s.route(3_000, true), Route::Passive { msg_id: "m1".into(), seq: 3 });
        assert_eq!(s.route(4_000, true), Route::Proactive, "次数用尽后转主动");
        assert_eq!(s.route(4_000, false), Route::Defer, "额度用尽则延迟");
        // 新消息重置锚点；超出窗口后不能再被动回复
        s.on_inbound("m2", 10_000);
        assert_eq!(s.route(10_000 + 60 * 60_000 + 1, true), Route::Proactive);
    }

    #[test]
    fn event_anchor_and_reject() {
        let mut s = ReplyState::new(60, 5);
        s.on_event("e1", 0, true);
        assert_eq!(s.route(1, true), Route::Event { event_id: "e1".into(), seq: 1 });
        s.on_event("", 2, false);
        assert_eq!(s.route(3, true), Route::Defer, "拒收主动消息后不再主动发");
    }
}
