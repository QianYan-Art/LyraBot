//! 可插拔提供方（docs/06-tools-plugins.md 第 2 节）：生图、语音合成、网页搜索、grok 搜索；另有代看视频的 watch_video。
//!
//! 配置结构为 `[plugins.<类别>] active = "名"` + `[plugins.<类别>.providers.<名>]`（含 `kind` 与各自字段）。
//! 工具每次调用都从当前配置读取激活的提供方，因此 `/provider` 改动写回配置并 reload 后立即生效。

mod grok;
mod image;
mod media;
pub mod provider_cmd;
mod search;
mod shape;
mod tts;
mod video;

pub use grok::{GrokFetch, GrokSearch};
pub use image::ImageGenerate;
pub use search::WebSearch;
pub use tts::Speak;
pub use video::WatchVideo;

use qya_agent::{Tool, ToolRegistry};
use qya_config::{Config, ConfigHandle, read_secret_line};
use qya_core::Secret;
use std::sync::Arc;
use std::time::Duration;

/// 插件共用的运行环境。
#[derive(Clone)]
pub struct Host {
    pub cfg: Arc<ConfigHandle>,
    pub http: reqwest::Client,
}

impl Host {
    pub fn new(cfg: Arc<ConfigHandle>) -> Self {
        // 进程级 TLS 后端；已安装时忽略
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(2)
            .user_agent(concat!("qy-assistant/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("构建 HTTP 客户端");
        Self { cfg, http }
    }

    /// 取某类别当前激活的提供方，并校验 kind。
    pub fn active(&self, category: &str, kind: &str) -> Result<(Arc<Config>, ProviderTable), String> {
        let c = self.cfg.get();
        let (name, table) = c.plugins.active(category).ok_or_else(|| format!("未配置 {category} 提供方"))?;
        let got = table.get("kind").and_then(|v| v.as_str()).unwrap_or_default();
        if got != kind {
            return Err(format!("{category} 提供方 {name} 的 kind 是 `{got}`，本工具只支持 `{kind}`"));
        }
        let t = ProviderTable { name: name.to_owned(), table: table.clone() };
        Ok((c, t))
    }
}

/// 一个提供方的原始配置表及取值辅助。
pub struct ProviderTable {
    pub name: String,
    pub table: toml::Table,
}

impl ProviderTable {
    pub fn str(&self, key: &str) -> Option<&str> {
        self.table.get(key).and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty())
    }
    pub fn req(&self, key: &str) -> Result<&str, String> {
        self.str(key).ok_or_else(|| format!("提供方 {} 缺少字段 {key}", self.name))
    }
    pub fn int(&self, key: &str, default: i64) -> i64 {
        self.table.get(key).and_then(|v| v.as_integer()).unwrap_or(default)
    }
    /// 读取 `<key>`（直接值）或 `<key>_file`（文件首行）。
    pub fn value_or_file(&self, c: &Config, key: &str) -> Result<String, String> {
        if let Some(v) = self.str(key) {
            return Ok(v.to_owned());
        }
        let f = self.req(&format!("{key}_file"))?;
        read_secret_line(&c.resolve(std::path::Path::new(f))).map(|s| s.expose().to_owned()).map_err(|e| e.to_string())
    }
    /// 读取 secret：只允许 `<key>_file`，不接受明文写在配置里。
    pub fn secret(&self, c: &Config, key: &str) -> Result<Secret, String> {
        let f = self.req(&format!("{key}_file"))?;
        read_secret_line(&c.resolve(std::path::Path::new(f))).map_err(|e| e.to_string())
    }
}

/// 按配置注册插件工具（配置里存在对应类别才注册）。返回注册的名字。
pub fn register(reg: &mut ToolRegistry, host: &Host) -> Vec<String> {
    let c = host.cfg.get();
    let mut names = Vec::new();
    let mut add = |t: Arc<dyn Tool>| {
        names.push(t.spec().name.clone());
        reg.register(t);
    };
    if c.plugins.categories.contains_key("image") {
        add(Arc::new(ImageGenerate::new(host.clone())));
    }
    if c.plugins.categories.contains_key("tts") {
        add(Arc::new(Speak::new(host.clone())));
    }
    if c.plugins.categories.contains_key("search") {
        add(Arc::new(WebSearch::new(host.clone())));
    }
    if c.plugins.categories.contains_key("grok") {
        add(Arc::new(GrokSearch::new(host.clone())));
        add(Arc::new(GrokFetch::new(host.clone())));
    }
    // 看视频的模型须在唐语歌提供方的模型目录中且支持视频
    if c.provider(&c.persona.provider).and_then(|p| p.models.get(&c.tools.video_model)).is_some_and(|m| m.video) {
        add(Arc::new(WatchVideo::new(host.clone())));
    } else {
        tracing::warn!(model = %c.tools.video_model, "看视频的模型不可用，不注册 watch_video");
    }
    names
}
