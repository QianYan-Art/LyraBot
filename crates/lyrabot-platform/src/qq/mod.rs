//! QQ 官方机器人单聊适配器（docs/07-platform-commands.md 第 2 节）。
//!
//! 网关：Hello(op10) → Identify(op2)/Resume(op6) → READY；按服务端间隔 80% 发送心跳(op1)；
//! op7 重连并 Resume，op9 按是否可恢复决定 Resume 或重新 Identify；致命关闭码停止重连。
//! 只接受配置中的 user_openid；出站见 [`courier`]：按 [`reply::ReplyState`] 选择通道，没有通道时写入发件箱。

pub mod api;
mod courier;
pub mod inbound;
mod media;
pub mod reply;

use crate::{Inbound, Outbound, Platform, PlatformCaps, PlatformId, SendErrorKind, SendResult, anyhow_like};
use api::Api;
use async_trait::async_trait;
use courier::Courier;
use futures_util::{SinkExt, StreamExt};
use lyrabot_config::QqConfig;
use lyrabot_store::Store;
use reply::ReplyState;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// C2C 与群消息、单聊事件、互动事件等（与 Hermes 现网一致，避免 4014 未授权）。
const INTENTS: u64 = (1 << 25) | (1 << 30) | (1 << 12) | (1 << 26);
const BACKOFF_S: [u64; 6] = [1, 2, 5, 10, 30, 60];
const FATAL_CODES: [u16; 9] = [4001, 4002, 4010, 4011, 4012, 4013, 4014, 4914, 4915];
/// 接收循环超过该时长没有推进视为卡死。循环各步都有上界：连接后心跳约 30 秒一次、等待 Hello 30 秒；
/// 建连时取 token 与网关地址各 ≤30 秒（`api::CONTROL_TIMEOUT`）、WebSocket 握手 20 秒；重连退避最长 60 秒。
/// 相邻两次推进最长约 80 秒，阈值留出两倍余量。
const STALL: Duration = Duration::from_secs(180);
/// 已收到、尚在下载附件或等待交给运行时的入站消息上限（单用户手动发送，正常远低于此值）。
const INBOUND_BACKLOG: usize = 32;

/// 按到达顺序排队的入站消息：附件在后台下载，交付顺序仍与收到的顺序一致。
type Ordered = mpsc::Sender<Pending>;

/// 一条排队中的入站消息（附件下载任务）。被丢弃时取消下载，未完成的半截文件由下载守卫清理，
/// 因此接收循环退出、队列被丢弃或积压满时都不会留下脱离管理的下载任务。
struct Pending(tokio::task::JoinHandle<Option<Inbound>>);

impl Drop for Pending {
    fn drop(&mut self) {
        self.0.abort();
    }
}

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

/// 入站与出站共享的回复状态。
pub(crate) struct State {
    reply: ReplyState,
    seen: VecDeque<String>,
}

pub struct QqPlatform {
    api: Arc<Api>,
    state: Arc<Mutex<State>>,
    courier: Arc<Courier>,
    session: tokio::sync::Mutex<Session>,
    /// 接收循环最近一次推进的时刻（相对 `born` 的毫秒数，单调时钟，不受系统时间调整影响）
    progress: AtomicU64,
    born: Instant,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

impl QqPlatform {
    pub fn new(cfg: QqConfig, store: Store) -> Self {
        let reply = ReplyState::new(cfg.reply_window_min, cfg.max_replies);
        let api = Arc::new(Api::new(cfg));
        let state = Arc::new(Mutex::new(State { reply, seen: VecDeque::new() }));
        Self {
            courier: Arc::new(Courier::new(api.clone(), store, state.clone())),
            api,
            state,
            session: tokio::sync::Mutex::new(Session::default()),
            progress: AtomicU64::new(0),
            born: Instant::now(),
        }
    }

    fn touch(&self) {
        self.progress.store(self.born.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("qq 状态锁中毒")
    }

    fn openid(&self) -> &str {
        &self.api.cfg().allowed_users[0]
    }

    /// 一次网关会话：连接、鉴权、收发，直到断开。正常需要重连时返回 Ok。
    async fn session_once(&self, inbox: &Ordered) -> Result<(), GwError> {
        let url = self.api.gateway_url().await.map_err(|e| match e.kind {
            SendErrorKind::Forbidden => GwError::Fatal(e.message),
            _ => GwError::Retry(e.message),
        })?;
        self.touch();
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
            self.touch();
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
    async fn handle_payload(&self, p: &Value, inbox: &Ordered, heartbeat: &mut Option<tokio::time::Interval>) -> Result<Option<Value>, GwError> {
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
                        "properties": {"$os": "linux", "$browser": "lyrabot", "$device": "lyrabot"}}}),
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
                    "C2C_MESSAGE_CREATE" => self.on_c2c(d.clone(), inbox),
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
    fn on_c2c(&self, d: Value, inbox: &Ordered) {
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
        let download = tokio::spawn(async move {
            let dir = api.cfg().inbox_dir.clone();
            let mut saved: Vec<inbound::Download> = Vec::new();
            for a in &p.attachments {
                saved.push(match a {
                    inbound::Attachment::Image { url, ext } => api.download(url, &dir, &format!("image.{ext}")).await,
                    inbound::Attachment::Video { url, name } | inbound::Attachment::File { url, name } => api.download(url, &dir, name).await,
                    inbound::Attachment::Voice { .. } => inbound::Download::Failed,
                });
            }
            let parts = inbound::to_parts(&p, &saved);
            (!parts.is_empty()).then(|| Inbound { platform: PlatformId::Qq, user: p.openid.clone(), parts, msg_id: Some(p.msg_id.clone()) })
        });
        // 不在网关循环里等待：积压已满（运行时长时间没有取走消息）时如实记录并丢弃这一条
        // 发送失败时 Pending 随错误一起丢弃，下载随之取消
        match inbox.try_send(Pending(download)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => tracing::error!("入站消息积压超过 {INBOUND_BACKLOG} 条，丢弃一条"),
            // 正在退出：运行时已不再接收消息
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
        // 有了新的被动回复窗口：唤醒补发任务（多次唤醒合并为一次），不阻塞网关的心跳与收包
        self.courier.request_flush();
    }
}

/// 接收循环结束（返回或被取消）时一并停止其后台任务。
struct AbortOnDrop<const N: usize>([tokio::task::JoinHandle<()>; N]);

impl<const N: usize> Drop for AbortOnDrop<N> {
    fn drop(&mut self) {
        for t in &self.0 {
            t.abort();
        }
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
        // 按到达顺序交付：前一条的附件没下载完，后面的消息（包括 /stop 等命令）也不会越过它
        let (ordered, mut queue) = mpsc::channel::<Pending>(INBOUND_BACKLOG);
        let forward = inbox.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(mut p) = queue.recv().await {
                if let Ok(Some(m)) = (&mut p.0).await
                    && forward.send(m).await.is_err()
                {
                    break;
                }
            }
        });
        let flusher = tokio::spawn(self.courier.clone().run_flusher());
        let _tasks = AbortOnDrop([forwarder, flusher]);
        let mut attempt = 0usize;
        loop {
            self.touch();
            let started = Instant::now();
            let res = self.session_once(&ordered).await;
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
            self.touch();
            tokio::time::sleep(wait).await;
        }
    }

    async fn send(&self, out: Outbound) -> SendResult {
        self.courier.send(out).await
    }

    fn alive(&self) -> bool {
        let idle_ms = (self.born.elapsed().as_millis() as u64).saturating_sub(self.progress.load(Ordering::Relaxed));
        idle_ms <= STALL.as_millis() as u64
    }
}
