//! QQ 官方机器人单聊适配器（docs/01-architecture.md 9.1）。
//!
//! 网关：Hello(op10) → Identify(op2)/Resume(op6) → READY；按服务端间隔 80% 发送心跳(op1)；
//! op7 重连并 Resume，op9 按是否可恢复决定 Resume 或重新 Identify；致命关闭码停止重连。
//! 只接受配置中的 user_openid；出站按 [`reply::ReplyState`] 选择被动/主动/延迟。

pub mod api;
pub mod inbound;
pub mod reply;

use crate::{Inbound, Outbound, Platform, PlatformCaps, PlatformId, SendError, SendErrorKind, SendResult, anyhow_like};
use api::{Api, MSG_TYPE_MEDIA, MSG_TYPE_TEXT, file_type_for};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use qya_config::QqConfig;
use qya_store::Store;
use reply::{ReplyState, Route};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// C2C 与群消息、单聊事件、互动事件等（与 Hermes 现网一致，避免 4014 未授权）。
const INTENTS: u64 = (1 << 25) | (1 << 30) | (1 << 12) | (1 << 26);
const BACKOFF_S: [u64; 6] = [1, 2, 5, 10, 30, 60];
const FATAL_CODES: [u16; 9] = [4001, 4002, 4010, 4011, 4012, 4013, 4014, 4914, 4915];

#[derive(Debug)]
enum GwError {
    /// 不可恢复（凭据、权限、封禁等），停止重连
    Fatal(String),
    RateLimited,
    Retry(String),
}

#[derive(Default)]
struct Session {
    id: Option<String>,
    seq: Option<u64>,
}

struct State {
    reply: ReplyState,
    deferred: Vec<Outbound>,
    seen: VecDeque<String>,
}

/// 延迟投递队列上限。
const MAX_DEFERRED: usize = 50;

pub struct QqPlatform {
    api: Arc<Api>,
    store: Store,
    state: Mutex<State>,
    session: tokio::sync::Mutex<Session>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn quota_key() -> String {
    let tz = jiff::tz::TimeZone::get("Asia/Shanghai").unwrap_or(jiff::tz::TimeZone::fixed(jiff::tz::offset(8)));
    format!("qq_proactive:{}", jiff::Timestamp::now().to_zoned(tz).strftime("%Y-%m"))
}

impl QqPlatform {
    pub fn new(cfg: QqConfig, store: Store) -> Self {
        let reply = ReplyState::new(cfg.reply_window_min, cfg.max_replies);
        Self {
            api: Arc::new(Api::new(cfg)),
            store,
            state: Mutex::new(State { reply, deferred: Vec::new(), seen: VecDeque::new() }),
            session: tokio::sync::Mutex::new(Session::default()),
        }
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("qq 状态锁中毒")
    }

    /// 加入延迟队列；超过上限时丢弃最旧的一条，避免长期无窗口时无界增长。
    fn defer(&self, out: Outbound) {
        let mut st = self.st();
        st.deferred.push(out);
        if st.deferred.len() > MAX_DEFERRED {
            st.deferred.remove(0);
            tracing::warn!("延迟队列已满，丢弃最旧的一条");
        }
    }

    fn openid(&self) -> &str {
        &self.api.cfg().allowed_users[0]
    }

    async fn quota_used(&self) -> u32 {
        self.store.kv_get(&quota_key()).await.ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0)
    }

    /// 一次网关会话：连接、鉴权、收发，直到断开。正常需要重连时返回 Ok。
    async fn session_once(&self, inbox: &mpsc::Sender<Inbound>) -> Result<(), GwError> {
        let url = self.api.gateway_url().await.map_err(|e| match e.kind {
            SendErrorKind::Forbidden => GwError::Fatal(e.message),
            _ => GwError::Retry(e.message),
        })?;
        let (ws, _) = tokio::time::timeout(Duration::from_secs(20), tokio_tungstenite::connect_async(url.as_str()))
            .await
            .map_err(|_| GwError::Retry("连接网关超时".into()))?
            .map_err(|e| GwError::Retry(format!("连接网关失败：{e}")))?;
        let (mut tx, mut rx) = ws.split();
        let mut heartbeat: Option<tokio::time::Interval> = None;
        // 已发出但未收到 ACK（op 11）的心跳数；连续两次未确认视为连接假死
        let mut unacked = 0u32;
        let hello_deadline = tokio::time::sleep(Duration::from_secs(30));
        tokio::pin!(hello_deadline);
        tracing::info!("QQ 网关已连接");
        loop {
            let waiting_hello = heartbeat.is_none();
            let tick = async {
                match heartbeat.as_mut() {
                    Some(i) => {
                        i.tick().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = &mut hello_deadline, if waiting_hello => {
                    return Err(GwError::Retry("连接后 30 秒未收到 Hello".into()));
                }
                _ = tick => {
                    if unacked >= 2 {
                        return Err(GwError::Retry("心跳连续未确认，连接可能已失效".into()));
                    }
                    unacked += 1;
                    let seq = self.session.lock().await.seq;
                    tx.send(Message::text(json!({"op": 1, "d": seq}).to_string())).await.map_err(|e| GwError::Retry(format!("发送心跳失败：{e}")))?;
                }
                msg = rx.next() => {
                    let msg = match msg {
                        None => return Err(GwError::Retry("网关连接已关闭".into())),
                        Some(Err(e)) => return Err(GwError::Retry(format!("网关读取失败：{e}"))),
                        Some(Ok(m)) => m,
                    };
                    match msg {
                        Message::Text(t) => {
                            let Ok(p) = serde_json::from_str::<Value>(&t) else { continue };
                            if p["op"].as_u64() == Some(11) {
                                unacked = 0;
                            }
                            if let Some(reply) = self.handle_payload(&p, inbox, &mut heartbeat).await? {
                                tx.send(Message::text(reply.to_string())).await.map_err(|e| GwError::Retry(format!("发送失败：{e}")))?;
                            }
                            if matches!(p["op"].as_u64(), Some(7 | 9)) {
                                return Ok(());
                            }
                        }
                        Message::Ping(d) => { let _ = tx.send(Message::Pong(d)).await; }
                        Message::Close(frame) => return Err(self.on_close(frame.map(|f| u16::from(f.code))).await),
                        _ => {}
                    }
                }
            }
        }
    }

    async fn on_close(&self, code: Option<u16>) -> GwError {
        let code = code.unwrap_or(0);
        tracing::warn!(code, "QQ 网关关闭连接");
        if FATAL_CODES.contains(&code) {
            return GwError::Fatal(format!("QQ 网关致命关闭码 {code}（检查机器人权限、intent 或账号状态）"));
        }
        match code {
            4004 => {
                self.api.invalidate_token().await;
                GwError::Retry("token 失效，刷新后重连".into())
            }
            4006 | 4007 => {
                *self.session.lock().await = Session::default();
                GwError::Retry("会话失效，重新鉴权".into())
            }
            4008 => GwError::RateLimited,
            _ => GwError::Retry(format!("关闭码 {code}")),
        }
    }

    /// 处理一条网关消息；需要回送的载荷（Identify/Resume）作为返回值。
    async fn handle_payload(&self, p: &Value, inbox: &mpsc::Sender<Inbound>, heartbeat: &mut Option<tokio::time::Interval>) -> Result<Option<Value>, GwError> {
        if let Some(s) = p["s"].as_u64() {
            let mut sess = self.session.lock().await;
            if sess.seq.is_none_or(|old| s > old) {
                sess.seq = Some(s);
            }
        }
        match p["op"].as_u64() {
            Some(10) => {
                let ms = p.pointer("/d/heartbeat_interval").and_then(Value::as_u64).unwrap_or(30_000);
                let mut i = tokio::time::interval(Duration::from_millis(ms * 8 / 10));
                i.tick().await; // 第一跳立即完成，跳过
                *heartbeat = Some(i);
                let token = self.api.token().await.map_err(|e| GwError::Retry(e.message))?;
                let sess = self.session.lock().await;
                return Ok(Some(match (&sess.id, sess.seq) {
                    (Some(id), Some(seq)) => json!({"op": 6, "d": {"token": format!("QQBot {token}"), "session_id": id, "seq": seq}}),
                    _ => json!({"op": 2, "d": {"token": format!("QQBot {token}"), "intents": INTENTS, "shard": [0, 1],
                        "properties": {"$os": "linux", "$browser": "qy-assistant", "$device": "qy-assistant"}}}),
                }));
            }
            Some(0) => {
                let t = p["t"].as_str().unwrap_or_default();
                let d = &p["d"];
                match t {
                    "READY" => {
                        self.session.lock().await.id = d["session_id"].as_str().map(str::to_owned);
                        tracing::info!("QQ 网关 READY");
                    }
                    "RESUMED" => tracing::info!("QQ 会话已恢复"),
                    "C2C_MESSAGE_CREATE" => self.on_c2c(d.clone(), inbox.clone()).await,
                    "FRIEND_ADD" | "C2C_MSG_RECEIVE" | "FRIEND_DEL" | "C2C_MSG_REJECT" => {
                        let allowed = matches!(t, "FRIEND_ADD" | "C2C_MSG_RECEIVE");
                        if d["openid"].as_str() == Some(self.openid()) {
                            let event_id = p["id"].as_str().unwrap_or_default();
                            self.st().reply.on_event(event_id, now_ms(), allowed);
                            tracing::info!(event = t, "QQ 主动消息许可变化");
                        }
                    }
                    other => tracing::debug!(event = other, "忽略 QQ 事件"),
                }
            }
            Some(9) => {
                if !p["d"].as_bool().unwrap_or(false) {
                    *self.session.lock().await = Session::default();
                }
                tracing::info!(resumable = p["d"].as_bool().unwrap_or(false), "QQ 会话无效（op9），重连");
            }
            Some(7) => tracing::info!("QQ 网关要求重连（op7）"),
            _ => {}
        }
        Ok(None)
    }

    /// 入站单聊：过滤发送者、去重、下载附件（后台进行，不阻塞心跳），再交给运行时。
    async fn on_c2c(&self, d: Value, inbox: mpsc::Sender<Inbound>) {
        let Some(p) = inbound::parse_c2c(&d) else { return };
        if !self.api.cfg().allowed_users.contains(&p.openid) {
            tracing::info!("忽略非授权用户的 QQ 消息");
            return;
        }
        {
            let mut st = self.st();
            if st.seen.contains(&p.msg_id) {
                return;
            }
            st.seen.push_back(p.msg_id.clone());
            if st.seen.len() > 200 {
                st.seen.pop_front();
            }
            st.reply.on_inbound(&p.msg_id, now_ms());
        }
        let api = self.api.clone();
        tokio::spawn(async move {
            let dir = api.cfg().inbox_dir.clone();
            let mut saved: Vec<Option<PathBuf>> = Vec::new();
            for a in &p.attachments {
                saved.push(match a {
                    inbound::Attachment::Image { url, ext } => api.download(url, &dir, &format!("image.{ext}")).await,
                    inbound::Attachment::Video { url, name } | inbound::Attachment::File { url, name } => api.download(url, &dir, name).await,
                    inbound::Attachment::Voice { .. } => None,
                });
            }
            let parts = inbound::to_parts(&p, &saved);
            if parts.is_empty() {
                return;
            }
            let _ = inbox.send(Inbound { platform: PlatformId::Qq, user: p.openid.clone(), parts, msg_id: Some(p.msg_id.clone()) }).await;
        });
        // 有了新的被动回复窗口，把之前没发出的消息补发
        let deferred = std::mem::take(&mut self.st().deferred);
        if !deferred.is_empty() {
            let text = deferred.iter().map(|o| o.text.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n");
            let media = deferred.into_iter().flat_map(|o| o.media).collect();
            tracing::info!("补发此前延迟的 QQ 消息");
            let out = Outbound { text, media, reply_to: None };
            if let Err(e) = self.send(out.clone()).await {
                // 文字没发出去：放回延迟队列最前，下次阿颜说话时再补
                tracing::warn!(error = %e.message, "补发失败，保留到下次");
                self.st().deferred.insert(0, out);
            }
        }
    }

    /// 选择通道并发送一段载荷。`Ok(false)` 表示已延迟。
    async fn deliver(&self, body: Value, fallback: &Outbound) -> Result<bool, SendError> {
        let quota = self.api.cfg().proactive_monthly_quota;
        let used = self.quota_used().await;
        let route = self.st().reply.route(now_ms(), used < quota);
        let openid = self.openid().to_owned();
        let res = match &route {
            Route::Passive { msg_id, seq } => self.api.send_c2c(&openid, body, Some(("msg_id", msg_id, *seq))).await,
            Route::Event { event_id, seq } => self.api.send_c2c(&openid, body, Some(("event_id", event_id, *seq))).await,
            Route::Proactive => self.api.send_c2c(&openid, body, None).await,
            Route::Defer => {
                tracing::info!("没有可用的 QQ 回复通道（被动窗口已过且主动额度用尽），延迟到阿颜下次说话");
                self.defer(fallback.clone());
                return Ok(false);
            }
        };
        match res {
            Ok(_) => {
                if route == Route::Proactive {
                    let _ = self.store.kv_set(&quota_key(), &(used + 1).to_string()).await;
                }
                Ok(true)
            }
            Err(e) if route == Route::Proactive && e.kind == SendErrorKind::Forbidden => {
                tracing::warn!(error = %e.message, "主动消息被拒绝，延迟投递");
                self.defer(fallback.clone());
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    async fn send_media(&self, path: &std::path::Path) -> Result<bool, SendError> {
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
        let body = json!({"msg_type": MSG_TYPE_MEDIA, "media": {"file_info": file_info}});
        self.deliver(body, &Outbound { text: String::new(), media: vec![path.to_owned()], reply_to: None }).await
    }
}

#[async_trait]
impl Platform for QqPlatform {
    fn id(&self) -> PlatformId {
        PlatformId::Qq
    }

    fn caps(&self) -> PlatformCaps {
        PlatformCaps { streaming: false }
    }

    async fn run(&self, inbox: mpsc::Sender<Inbound>) -> anyhow_like::Result<()> {
        let mut attempt = 0usize;
        loop {
            let started = Instant::now();
            let res = self.session_once(&inbox).await;
            if inbox.is_closed() {
                return Ok(());
            }
            if started.elapsed() > Duration::from_secs(60) {
                attempt = 0;
            }
            let wait = match res {
                Ok(()) => Duration::from_secs(1),
                Err(GwError::Fatal(m)) => {
                    tracing::error!(%m, "QQ 网关不可恢复，停止重连");
                    return Err(m.into());
                }
                Err(GwError::RateLimited) => Duration::from_secs(60),
                Err(GwError::Retry(m)) => {
                    let w = Duration::from_secs(BACKOFF_S[attempt.min(BACKOFF_S.len() - 1)]);
                    tracing::warn!(%m, wait_s = w.as_secs(), "QQ 网关断开，稍后重连");
                    attempt += 1;
                    w
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    async fn send(&self, out: Outbound) -> SendResult {
        let text = out.text.trim();
        if !text.is_empty() {
            let body = json!({"msg_type": MSG_TYPE_TEXT, "content": text});
            self.deliver(body, &Outbound { text: text.to_owned(), media: Vec::new(), reply_to: None }).await?;
        }
        for m in &out.media {
            if let Err(e) = self.send_media(m).await {
                tracing::warn!(error = %e.message, path = %m.display(), "QQ 媒体发送失败");
                // 媒体失败时给阿颜一个文字说明，不静默吞掉
                let note = json!({"msg_type": MSG_TYPE_TEXT, "content": format!("（有个文件没发出去：{}）", e.message)});
                let _ = self.deliver(note, &Outbound::default()).await;
            }
        }
        Ok(())
    }
}
