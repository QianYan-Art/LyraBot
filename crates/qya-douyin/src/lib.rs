//! 抖音外部服务接入（docs/06-tools-plugins.md 第 3 节）：连接 hermes-douyin 的 Unix socket。
//!
//! 与 Hermes 插件的差异（阿颜 2026-11-07 决定）：
//! - 公开发布不再由模型调用：阿颜在 QQ 发送“确认发布 <plan_id>”后，运行时经 [`Douyin::intercept`] 直接提交发布，
//!   并跟进任务状态；需要短信验证时自动请求发码，阿颜回复的六位数字同样由程序接收，不进入模型、工具参数或会话记录。
//! - 只在存在发码窗口时拦截六位数字，其余时候数字消息照常交给唐语歌。
//!
//! 其余校验、净化与封面规则与插件一致。

mod client;
mod covers;
pub mod sanitize;
mod tools;

pub use client::Client;
pub use tools::{DouyinContent, DouyinLogin, DouyinVerification};

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const IMAGE_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_IMAGES: usize = 20;
const VERIFY_TTL: Duration = Duration::from_secs(5 * 60);
const VERIFY_COOLDOWN: Duration = Duration::from_secs(60);
/// 发布后跟进任务状态的最长时间与间隔
const PUBLISH_FOLLOW: Duration = Duration::from_secs(180);
const POLL_EVERY: Duration = Duration::from_secs(2);

pub struct InboundImage {
    pub id: String,
    pub path: PathBuf,
    pub at: Instant,
    pub received: String,
    pub ordinal: usize,
}

#[derive(Debug, Clone)]
struct Binding {
    job_id: String,
    requested_at: Instant,
    expires_at: Instant,
    requesting: bool,
    sms_ready: bool,
    processing: bool,
}

/// 程序接管消息后的后台结果。
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// 直接发给阿颜的固定回执
    pub user_notice: Option<String>,
    /// 交给唐语歌的系统消息（〔〕包裹），让她知道发生了什么
    pub persona_note: Option<String>,
}

pub type Work = Pin<Box<dyn Future<Output = Outcome> + Send>>;

pub enum Intercept {
    /// 不相关，照常交给唐语歌
    Pass,
    /// 程序已接管：`notice` 立即发给阿颜，`work` 在后台执行
    Handled { notice: String, work: Option<Work> },
}

pub struct Douyin {
    pub(crate) client: Client,
    pub(crate) cache_dir: PathBuf,
    pub(crate) saved_root: PathBuf,
    pub(crate) images: Mutex<VecDeque<InboundImage>>,
    binding: Mutex<Option<Binding>>,
}

fn hex_id() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

impl Douyin {
    pub fn new(cfg: &qya_config::DouyinConfig) -> Arc<Self> {
        Arc::new(Self {
            client: Client::new(cfg.socket.clone()),
            cache_dir: cfg.cache_dir.clone(),
            saved_root: cfg.saved_covers_dir.clone(),
            images: Mutex::new(VecDeque::new()),
            binding: Mutex::new(None),
        })
    }

    /// 入站消息前置检查（在命令解析与模型之前调用）。`images` 为本条消息已落盘的图片。
    pub fn intercept(self: &Arc<Self>, text: &str, images: &[PathBuf]) -> Intercept {
        self.observe_images(images);
        let t = text.trim();
        if let Some(token) = sanitize::confirm_token(t) {
            let me = self.clone();
            let token = token.to_owned();
            return Intercept::Handled {
                notice: "收到发布确认，程序正在直接提交，结果出来后告诉你。".into(),
                work: Some(Box::pin(async move { me.publish(token).await })),
            };
        }
        if let Some(code) = sanitize::sms_code(t) {
            return self.on_sms(code.to_owned());
        }
        Intercept::Pass
    }

    fn observe_images(&self, images: &[PathBuf]) {
        if images.is_empty() {
            return;
        }
        let mut list = self.images.lock().expect("images 锁中毒");
        let now = Instant::now();
        list.retain(|i| now.duration_since(i.at) < IMAGE_TTL);
        let received = jiff::Timestamp::now().to_string();
        for (n, p) in images.iter().enumerate() {
            let ok = p.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png" | "webp"));
            if ok {
                list.push_back(InboundImage { id: hex_id(), path: p.clone(), at: now, received: received.clone(), ordinal: n + 1 });
            }
        }
        while list.len() > MAX_IMAGES {
            list.pop_front();
        }
    }

    /// 最近收到的图片候选（新的在前）。
    pub(crate) fn cover_candidates(&self) -> Vec<Value> {
        let mut list = self.images.lock().expect("images 锁中毒");
        let now = Instant::now();
        list.retain(|i| now.duration_since(i.at) < IMAGE_TTL && i.path.is_file());
        list.iter().rev().map(|i| json!({"image_id": i.id, "received_at": i.received, "ordinal": i.ordinal})).collect()
    }

    pub(crate) fn registered_image(&self, id: &str) -> Option<PathBuf> {
        let list = self.images.lock().expect("images 锁中毒");
        list.iter().find(|i| i.id == id && Instant::now().duration_since(i.at) < IMAGE_TTL).map(|i| i.path.clone())
    }

    /// 请求发送短信验证码，建立收码窗口（douyin_verification 工具与发布流程共用）。
    pub async fn request_sms(&self, job_id: &str) -> Result<Value, String> {
        {
            let mut b = self.binding.lock().expect("binding 锁中毒");
            let now = Instant::now();
            if let Some(p) = b.as_ref() {
                if p.requesting || p.processing {
                    return Err("当前短信验证请求仍在处理中，不要重复发送或提交。".into());
                }
                if p.job_id == job_id && now.duration_since(p.requested_at) < VERIFY_COOLDOWN {
                    return if p.sms_ready && p.expires_at > now {
                        Ok(json!({"ok": true, "state": "ready", "message": "刚刚已请求短信验证码；请阿颜直接回复最新六位数字，不要反复请求发送。"}))
                    } else {
                        Err("上次发码未确认或收码窗口已失效；请先查询原任务，稍后再请求。".into())
                    };
                }
            }
            *b = Some(Binding { job_id: job_id.to_owned(), requested_at: now, expires_at: now + VERIFY_TTL, requesting: true, sms_ready: false, processing: false });
        }
        // 请求期间外层可能被取消（future 被丢弃），守卫保证 requesting 状态不会永久残留
        let mut guard = RequestingGuard { binding: &self.binding, job_id, armed: true };
        let res = self.client.content(json!({"action": "verification_request_sms", "job_id": job_id})).await;
        guard.armed = false;
        let mut b = self.binding.lock().expect("binding 锁中毒");
        match res {
            Ok(v) if v["state"] == "ready" && v.pointer("/job/job_id").and_then(Value::as_str) == Some(job_id) => {
                if let Some(p) = b.as_mut().filter(|p| p.job_id == job_id) {
                    p.requesting = false;
                    p.sms_ready = true;
                }
                Ok(json!({"ok": true, "state": "ready", "message": "已请求发送短信。请阿颜在 QQ 私聊直接键入纯文本六位数字（不加前缀、不用语音或图片），程序会代填并点击验证；验证码不会进入模型或会话记录。尚未确认发布成功。"}))
            }
            Ok(v) => {
                *b = None;
                let state = v["state"].as_str().unwrap_or("unknown");
                let msg = v["message"].as_str().unwrap_or_default();
                Err(format!("发码未就绪（状态 {state}）：{msg}；未自动重试，请先查询原任务。"))
            }
            Err(e) => {
                *b = None;
                Err(format!("短信验证服务暂不可用：{e}；未自动重试，也未重新提交作品。"))
            }
        }
    }

    fn on_sms(self: &Arc<Self>, code: String) -> Intercept {
        let job_id = {
            let mut b = self.binding.lock().expect("binding 锁中毒");
            let Some(p) = b.as_mut() else { return Intercept::Pass };
            if p.processing {
                return Intercept::Handled { notice: "验证码正在处理中，请先不要重复发送。".into(), work: None };
            }
            if !p.sms_ready || p.expires_at <= Instant::now() {
                return Intercept::Handled { notice: "现在没有可用的发布收码窗口，这串数字没有交给语歌；需要的话让她查一下原发布任务。".into(), work: None };
            }
            p.processing = true;
            p.job_id.clone()
        };
        let me = self.clone();
        Intercept::Handled {
            notice: "已安全接收本次发布验证码，正在处理原任务，不会重新上传或重复发布。".into(),
            work: Some(Box::pin(async move { me.submit_code(job_id, code).await })),
        }
    }

    async fn submit_code(&self, job_id: String, code: String) -> Outcome {
        let res = self.client.content(json!({"action": "verification_submit", "job_id": job_id, "code": code})).await;
        drop(code);
        let mut finished = false;
        let text = match res {
            Ok(v) if v["verification_submitted"] == true => {
                let last = self.follow_job(&job_id, Duration::from_secs(20)).await.unwrap_or(v);
                match last["state"].as_str() {
                    Some("published") => {
                        finished = true;
                        "验证码验证后，平台已确认提交发布。后续审核状态以抖音为准。"
                    }
                    Some("needs_attention" | "unavailable" | "page_error") => "短信验证未通过或页面已失效；未重新上传、未再次点击发布。请让语歌查询原任务后再处理。",
                    _ => "验证码已提交验证，尚未取得发布成功回执；请让语歌查询原发布任务，不要重新上传或重复发布。",
                }
            }
            Ok(v) if matches!(v["state"].as_str(), Some("needs_attention" | "unavailable" | "page_error")) => "短信验证未通过或页面已失效；未重新上传、未再次点击发布。",
            _ => "验证码处理未确认完成；请让语歌查询原发布任务，不要重复发布或重新上传。",
        };
        {
            let mut b = self.binding.lock().expect("binding 锁中毒");
            if b.as_ref().is_some_and(|p| p.job_id == job_id) {
                if finished {
                    *b = None;
                } else if let Some(p) = b.as_mut() {
                    p.processing = false;
                }
            }
        }
        Outcome { user_notice: Some(text.into()), persona_note: Some(format!("〔抖音短信验证（任务 {job_id}）：{text}〕")) }
    }

    /// 跟进任务直到离开 in_progress 或超时；返回最后一次查询结果。
    async fn follow_job(&self, job_id: &str, max: Duration) -> Option<Value> {
        let deadline = Instant::now() + max;
        let mut last = None;
        while Instant::now() < deadline {
            match self.client.content(json!({"action": "job", "job_id": job_id})).await {
                Ok(v) => {
                    let done = v["state"] != "in_progress";
                    last = Some(v);
                    if done {
                        break;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "查询抖音任务失败"),
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
        last
    }

    /// 阿颜确认后直接提交发布，并跟进到结果或需要短信验证。
    async fn publish(&self, token: String) -> Outcome {
        let res = match self.client.content(json!({"action": "publish", "token": token})).await {
            Ok(v) => v,
            Err(e) => {
                return Outcome {
                    user_notice: None,
                    persona_note: Some(format!("〔抖音发布：提交计划 {token} 时服务不可用或响应无效（{e}）。操作结果未知，请先查询任务状态，不要重复提交。〕")),
                };
            }
        };
        let job_id = res.pointer("/job/job_id").and_then(Value::as_str).map(str::to_owned);
        let mut last = res.clone();
        if let (Some(j), "in_progress") = (&job_id, res["state"].as_str().unwrap_or_default())
            && let Some(v) = self.follow_job(j, PUBLISH_FOLLOW).await
        {
            last = v;
        }
        let state = last["state"].as_str().unwrap_or("unknown");
        let msg = last["message"].as_str().unwrap_or_default();
        let job = job_id.clone().unwrap_or_else(|| "?".into());
        let note = match state {
            "verification_required" if job_id.is_some() => match self.request_sms(&job).await {
                Ok(_) => format!("〔抖音发布：计划 {token} 已提交（任务 {job}），平台要求短信验证，程序已请求发码。请提醒阿颜直接回复六位验证码。平台说明：{msg}〕"),
                Err(e) => format!("〔抖音发布：任务 {job} 需要短信验证，但请求发码失败：{e}〕"),
            },
            "published" => format!("〔抖音发布：计划 {token} 已确认提交发布（任务 {job}）。后续审核以抖音为准。平台说明：{msg}〕"),
            "in_progress" => format!("〔抖音发布：任务 {job} 仍在进行中（已跟进 3 分钟）。阿颜问起时让创作专员查询该任务。平台说明：{msg}〕"),
            _ => format!("〔抖音发布：计划 {token} 当前状态 {state}（任务 {job}）。平台说明：{msg}。不要自动重试，需要时让创作专员按恢复指引处理。〕"),
        };
        Outcome { user_notice: None, persona_note: Some(note) }
    }
}

/// `request_sms` 的取消守卫：未解除就被丢弃时清掉仍处于 requesting 的同一任务绑定。
struct RequestingGuard<'a> {
    binding: &'a std::sync::Mutex<Option<Binding>>,
    job_id: &'a str,
    armed: bool,
}

impl Drop for RequestingGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut b) = self.binding.lock()
            && b.as_ref().is_some_and(|p| p.requesting && p.job_id == self.job_id)
        {
            *b = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dy() -> Arc<Douyin> {
        let dir = std::env::temp_dir().join(format!("qya-dy-lib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Douyin::new(&qya_config::DouyinConfig { socket: dir.join("no.sock"), saved_covers_dir: dir.join("saved"), cache_dir: dir.join("cache") })
    }

    #[test]
    fn intercept_rules() {
        let d = dy();
        assert!(matches!(d.intercept("晚上吃什么", &[]), Intercept::Pass));
        // 没有发码窗口时，六位数字照常交给唐语歌
        assert!(matches!(d.intercept("123456", &[]), Intercept::Pass));
        match d.intercept("确认发布 abcdefghijklmnopqrstuvwxyz012-_9", &[]) {
            Intercept::Handled { notice, work } => assert!(notice.contains("发布确认") && work.is_some()),
            Intercept::Pass => panic!("确认发布应被程序接管"),
        }
        assert!(matches!(d.intercept("确认发布 短", &[]), Intercept::Pass));
        // 有窗口但未就绪：拦截且不执行
        *d.binding.lock().unwrap() = Some(Binding {
            job_id: "j".into(), requested_at: Instant::now(), expires_at: Instant::now() + VERIFY_TTL,
            requesting: false, sms_ready: false, processing: false,
        });
        assert!(matches!(d.intercept("654321", &[]), Intercept::Handled { work: None, .. }));
        d.binding.lock().unwrap().as_mut().unwrap().sms_ready = true;
        assert!(matches!(d.intercept("654321", &[]), Intercept::Handled { work: Some(_), .. }));
        assert!(d.binding.lock().unwrap().as_ref().unwrap().processing, "接管后进入处理中，防止重复提交");
        assert!(matches!(d.intercept("111111", &[]), Intercept::Handled { work: None, .. }));
    }

    #[test]
    fn inbound_image_registry() {
        let d = dy();
        let dir = std::env::temp_dir().join(format!("qya-dy-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.jpg");
        std::fs::write(&a, b"x").unwrap();
        d.intercept("看这张", &[a.clone(), dir.join("b.txt")]);
        let c = d.cover_candidates();
        assert_eq!(c.len(), 1);
        let id = c[0]["image_id"].as_str().unwrap();
        assert_eq!(d.registered_image(id), Some(a));
        assert_eq!(d.registered_image("0".repeat(32).as_str()), None);
    }
}
