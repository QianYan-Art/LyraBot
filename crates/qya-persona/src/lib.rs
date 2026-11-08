//! 唐语歌人格层：身份渲染、记忆快照、按需资料工具与每轮尾注入。
//!
//! 提示词布局（docs/01-architecture.md 6.1）：
//! - S1 身份：由 card.json + 常驻资料 + 运行时段落渲染，进程内不变 → 前缀缓存；
//! - S2 记忆快照：会话创建时冻结（由调用方持久化），新会话或压缩后才刷新；
//! - 尾注入：时间等每轮变化的内容，附在最后一条用户消息后，不落库。

mod assets;
pub mod consolidate;
pub mod learner;
mod notes_tool;
pub mod pacer;
mod render;
mod sticker;

pub use assets::{Assets, Note};
pub use notes_tool::PersonaNotes;
pub use render::MEMORY_FILE_LIMIT;
pub use sticker::Sticker;

use jiff::{Timestamp, tz::TimeZone};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum PersonaError {
    #[error("读取 {path} 失败：{source}")]
    Io { path: String, source: std::io::Error },
    #[error("人格资产无效：{0}")]
    Invalid(String),
}

#[derive(Debug, Clone)]
pub struct Persona {
    identity: Arc<str>,
    user_name: String,
    notes: Vec<Note>,
    tz: TimeZone,
}

impl Persona {
    pub fn load(assets_dir: &Path) -> Result<Self, PersonaError> {
        let a = Assets::load(assets_dir)?;
        let identity: Arc<str> = render::identity(&a).into();
        // 阿颜在国内，时间统一按东八区呈现
        let tz = TimeZone::get("Asia/Shanghai").unwrap_or(TimeZone::fixed(jiff::tz::offset(8)));
        Ok(Self { identity, user_name: a.user_name, notes: a.notes, tz })
    }

    /// S1 身份段。
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// 生成 S2 记忆快照（调用方负责按会话冻结保存）。
    pub fn snapshot(&self, memory_dir: &Path) -> String {
        render::snapshot(memory_dir, &self.user_name)
    }

    /// 系统段：S1 + 冻结的 S2。
    pub fn system(&self, s2: &str) -> Vec<String> {
        vec![self.identity.to_string(), s2.to_owned()]
    }

    /// 每轮尾注入：只放会变化的内容。
    pub fn tail(&self, now: Timestamp) -> String {
        let t = now.to_zoned(self.tz.clone());
        format!("〔当前时间 {}〕", t.strftime("%Y-%m-%d %H:%M %a"))
    }

    pub fn user_name(&self) -> &str {
        &self.user_name
    }

    pub fn notes_tool(&self) -> PersonaNotes {
        PersonaNotes::new(self.notes.clone())
    }
}
