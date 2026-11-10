//! QQ 出站：按 [`ReplyState`](super::reply::ReplyState) 选择被动/事件/主动通道发送；
//! 没有可用通道时写入发件箱（SQLite，进程重启不丢，文字与每个媒体各占一条），
//! 用户下次说话后在后台按顺序补发：相邻文字合并成不超过上限的批次，逐批确认，失败的留到下次。

use super::State;
use super::api::{Api, MSG_TYPE_MEDIA, MSG_TYPE_TEXT, file_type_for};
use super::reply::Route;
use crate::{Outbound, SendError, SendErrorKind, SendResult};
use lyrabot_store::Store;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 发件箱渠道名。
const CHANNEL: &str = "qq";
/// 发件箱上限（条）；长期没有通道时丢弃最旧的，避免无界增长。
const MAX_DEFERRED: usize = 50;
/// 补发时合并文字的上限（字）。
const MAX_BATCH_CHARS: usize = 1500;

pub(crate) struct Courier {
    api: Arc<Api>,
    store: Store,
    state: Arc<Mutex<State>>,
    /// 补发串行进行：后到的触发等前一次结束后再读一遍发件箱，不会漏掉期间新入队的消息
    flushing: tokio::sync::Mutex<()>,
}

/// 一次发送尝试的结果。
enum Sent {
    Delivered,
    /// 被动窗口已过、主动额度用尽或主动消息被拒
    NoRoute,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn quota_key() -> String {
    let tz = jiff::tz::TimeZone::get("Asia/Shanghai").unwrap_or(jiff::tz::TimeZone::fixed(jiff::tz::offset(8)));
    format!("qq_proactive:{}", jiff::Timestamp::now().to_zoned(tz).strftime("%Y-%m"))
}

fn text_body(text: &str) -> Value {
    json!({"msg_type": MSG_TYPE_TEXT, "content": text})
}

impl Courier {
    pub(crate) fn new(api: Arc<Api>, store: Store, state: Arc<Mutex<State>>) -> Self {
        Self { api, store, state, flushing: tokio::sync::Mutex::new(()) }
    }

    fn openid(&self) -> &str {
        &self.api.cfg().allowed_users[0]
    }

    /// 发送一条消息：先文字，再逐个媒体；没有通道的部分进发件箱。媒体失败时补一句文字说明，不静默吞掉。
    pub(crate) async fn send(&self, out: Outbound) -> SendResult {
        let text = out.text.trim();
        if !text.is_empty() && let Sent::NoRoute = self.route(text_body(text)).await? {
            self.defer_text(text).await?;
        }
        for m in &out.media {
            let res = match self.upload(m).await {
                Ok(body) => self.route(body).await,
                Err(e) => Err(e),
            };
            match res {
                Ok(Sent::Delivered) => {}
                Ok(Sent::NoRoute) => self.defer(json!({"media": m.display().to_string()})).await?,
                Err(e) => {
                    tracing::warn!(error = %e.message, path = %m.display(), "QQ 媒体发送失败");
                    let note = format!("（有个文件没发出去：{}）", e.message);
                    if let Ok(Sent::NoRoute) = self.route(text_body(&note)).await {
                        self.defer_text(&note).await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// 补发发件箱。新的被动窗口打开后调用；与其他补发串行执行。
    pub(crate) async fn flush(&self) {
        let _guard = self.flushing.lock().await;
        let queued = match self.store.outbox_list(CHANNEL).await {
            Ok(q) => q,
            Err(e) => {
                tracing::warn!(error = %e, "读取 QQ 发件箱失败");
                return;
            }
        };
        if queued.is_empty() {
            return;
        }
        tracing::info!(count = queued.len(), "补发此前延迟的 QQ 消息");
        let mut batch: (Vec<i64>, Vec<String>) = (Vec::new(), Vec::new());
        for q in queued {
            if let Some(text) = q.payload["text"].as_str() {
                let used: usize = batch.1.iter().map(|t| t.chars().count()).sum();
                if !batch.0.is_empty() && used + text.chars().count() > MAX_BATCH_CHARS && !self.flush_text(&mut batch).await {
                    return;
                }
                batch.0.push(q.id);
                batch.1.push(text.to_owned());
                continue;
            }
            // 媒体保持原有顺序：先把前面的文字发出去
            if !self.flush_text(&mut batch).await {
                return;
            }
            let path = PathBuf::from(q.payload["media"].as_str().unwrap_or_default());
            if !path.is_file() {
                tracing::warn!(path = %path.display(), "待补发的文件已不存在，跳过");
                self.forget(vec![q.id]).await;
                continue;
            }
            let res = match self.upload(&path).await {
                Ok(body) => self.route(body).await,
                Err(e) => Err(e),
            };
            match res {
                Ok(Sent::Delivered) => self.forget(vec![q.id]).await,
                Ok(Sent::NoRoute) => return,
                Err(e) => {
                    tracing::warn!(error = %e.message, path = %path.display(), "补发文件失败，保留到下次");
                    return;
                }
            }
        }
        self.flush_text(&mut batch).await;
    }

    /// 发出累积的文字批次，成功的记录随即删除并清空批次，返回能否继续补发。
    /// 合并后超长时改为逐条发送；单条仍被拒则原样保留，不丢消息。
    async fn flush_text(&self, batch: &mut (Vec<i64>, Vec<String>)) -> bool {
        if batch.0.is_empty() {
            return true;
        }
        let (ids, texts) = std::mem::take(batch);
        match self.route(text_body(&texts.join("\n\n"))).await {
            Ok(Sent::Delivered) => {
                self.forget(ids).await;
                true
            }
            Err(e) if e.kind == SendErrorKind::TooLong && ids.len() > 1 => {
                for (id, text) in ids.into_iter().zip(texts) {
                    if !Box::pin(self.flush_text(&mut (vec![id], vec![text]))).await {
                        return false;
                    }
                }
                true
            }
            Ok(Sent::NoRoute) => false,
            Err(e) => {
                tracing::warn!(error = %e.message, "补发失败，保留到下次");
                false
            }
        }
    }

    async fn forget(&self, ids: Vec<i64>) {
        if let Err(e) = self.store.outbox_remove(ids).await {
            tracing::warn!(error = %e, "清理 QQ 发件箱失败，下次可能重复补发");
        }
    }

    /// 文字按补发批次上限切块后入发件箱，保证任何一条记录都不会单独超出批次上限。
    async fn defer_text(&self, text: &str) -> Result<(), SendError> {
        let chars: Vec<char> = text.chars().collect();
        for chunk in chars.chunks(MAX_BATCH_CHARS) {
            self.defer(json!({"text": chunk.iter().collect::<String>()})).await?;
        }
        Ok(())
    }

    async fn defer(&self, part: Value) -> Result<(), SendError> {
        tracing::info!("没有可用的 QQ 回复通道（被动窗口已过且主动额度用尽），延迟到用户下次说话");
        match self.store.outbox_push(CHANNEL, part, MAX_DEFERRED).await {
            Ok(0) => Ok(()),
            Ok(n) => {
                tracing::warn!(dropped = n, "QQ 发件箱已满，丢弃最旧的消息");
                Ok(())
            }
            Err(e) => Err(SendError { kind: SendErrorKind::Unknown, message: format!("暂存待发消息失败：{e}") }),
        }
    }

    /// 选择通道发送一段载荷；没有通道时返回 `NoRoute`，由调用方决定是否入发件箱。
    async fn route(&self, body: Value) -> Result<Sent, SendError> {
        let quota = self.api.cfg().proactive_monthly_quota;
        let used: u32 = self.store.kv_get(&quota_key()).await.ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
        let route = self.state.lock().expect("qq 状态锁中毒").reply.route(now_ms(), used < quota);
        let openid = self.openid().to_owned();
        let res = match &route {
            Route::Passive { msg_id, seq } => self.api.send_c2c(&openid, body, Some(("msg_id", msg_id, *seq))).await,
            Route::Event { event_id, seq } => self.api.send_c2c(&openid, body, Some(("event_id", event_id, *seq))).await,
            Route::Proactive => self.api.send_c2c(&openid, body, None).await,
            Route::Defer => return Ok(Sent::NoRoute),
        };
        match res {
            Ok(_) => {
                if route == Route::Proactive {
                    let _ = self.store.kv_set(&quota_key(), &(used + 1).to_string()).await;
                }
                Ok(Sent::Delivered)
            }
            Err(e) if route == Route::Proactive && e.kind == SendErrorKind::Forbidden => {
                tracing::warn!(error = %e.message, "主动消息被拒绝");
                Ok(Sent::NoRoute)
            }
            Err(e) => Err(e),
        }
    }

    /// 上传本地文件，返回可直接发送的媒体消息体。
    async fn upload(&self, path: &Path) -> Result<Value, SendError> {
        let openid = self.openid().to_owned();
        let ft = file_type_for(path);
        let file_info = match self.api.upload(&openid, path, ft).await {
            Ok(f) => f,
            // QQ 对语音格式的要求未经实测确认：语音被拒时退回按文件发送，保证送达
            Err(e) if ft == 3 && matches!(e.kind, SendErrorKind::BadFormat | SendErrorKind::Unknown) => {
                tracing::warn!(error = %e.message, "语音上传失败，改按文件发送");
                self.api.upload(&openid, path, 4).await?
            }
            Err(e) => return Err(e),
        };
        Ok(json!({"msg_type": MSG_TYPE_MEDIA, "media": {"file_info": file_info}}))
    }
}
