//! 各领域公开配置结构体与默认值。

use lyrabot_core::{Effort, Secret};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub default_provider: String,
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    pub id: String,
    pub base_url: String,
    pub api_key: Secret,
    pub transport: Transport,
    pub user_agent: Option<String>,
    /// 规范化思考等级 → 提供方参数值
    pub effort_map: BTreeMap<Effort, String>,
    pub models: BTreeMap<String, ModelInfo>,
    pub timeouts: Timeouts,
}

impl ProviderConfig {
    /// 将规范化等级映射为提供方参数；未配置映射时原样使用等级名。
    pub fn effort_param(&self, e: Effort) -> String {
        self.effort_map.get(&e).cloned().unwrap_or_else(|| e.as_str().to_owned())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    ChatCompletions,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub context_length: u64,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub video: bool,
    #[serde(default)]
    pub audio: bool,
    #[serde(default = "default_max_output")]
    pub max_output_tokens: u32,
}

fn default_max_output() -> u32 {
    32_000
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct Timeouts {
    #[serde(default = "d10")]
    pub connect_s: u64,
    #[serde(default = "d60")]
    pub first_byte_s: u64,
    #[serde(default = "d90")]
    pub idle_s: u64,
}
fn d10() -> u64 {
    10
}
fn d60() -> u64 {
    60
}
fn d90() -> u64 {
    90
}
impl Default for Timeouts {
    fn default() -> Self {
        Self { connect_s: 10, first_byte_s: 60, idle_s: 90 }
    }
}

#[derive(Debug, Clone)]
pub struct PersonaConfig {
    pub provider: String,
    pub model: String,
    pub effort: Effort,
    /// 人格资产目录（persona.toml、card.json、core/、notes/、runtime/）
    pub assets_dir: PathBuf,
    /// 长期记忆目录（user.md、memory.md 等）；默认 `<data_dir>/memory`
    pub memory_dir: PathBuf,
    /// 表情包目录；默认 `<data_dir>/stickers`
    pub stickers_dir: PathBuf,
    pub pacer: PacerSettings,
    pub keepalive: KeepaliveSettings,
    pub learner: LearnerSettings,
    /// 主对话角色可直接使用的轻工具（注册表中的名字；人格专属工具另行加入）
    pub tools: Vec<String>,
}

/// 主会话前缀缓存保活参数（docs/04-persona-memory.md 第 1 节）。
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeepaliveSettings {
    /// 距上次模型请求多少分钟发一次保活（设为 0 关闭保活）
    #[serde(default = "d_ka_interval")]
    pub interval_minutes: u64,
    /// 每次用户说话后最多连续保活多少次（设为 0 关闭保活）
    #[serde(default = "d_ka_max")]
    pub max_count: u32,
}
fn d_ka_interval() -> u64 {
    18
}
fn d_ka_max() -> u32 {
    9
}
impl Default for KeepaliveSettings {
    fn default() -> Self {
        Self { interval_minutes: d_ka_interval(), max_count: d_ka_max() }
    }
}

/// 表达/黑话学习参数（docs/04-persona-memory.md 第 3 节）。
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct LearnerSettings {
    pub enabled: bool,
    /// 学习用的低价模型（需在主对话角色所用提供方的模型目录中）
    pub model: String,
    pub effort: Effort,
    /// 累计多少条用户的新消息触发一次学习
    pub batch_messages: u32,
    /// 空闲多少分钟后，若新消息不少于 min_messages 也触发
    pub idle_minutes: u64,
    pub min_messages: u32,
    /// 表达库上限
    pub max_items: usize,
    /// 尾注入的最大字数
    pub inject_max_chars: usize,
}

impl Default for LearnerSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "kimi-for-coding".into(),
            effort: Effort::Low,
            batch_messages: 30,
            idle_minutes: 10,
            min_messages: 8,
            max_items: 300,
            inject_max_chars: 220,
        }
    }
}

/// 回复节奏参数（docs/04-persona-memory.md 第 2 节）。
#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PacerSettings {
    #[serde(default = "d_debounce")]
    pub debounce_ms: u64,
    #[serde(default = "d_max_wait")]
    pub max_wait_ms: u64,
    #[serde(default = "d_segments")]
    pub max_segments: usize,
}
fn d_debounce() -> u64 {
    1800
}
fn d_max_wait() -> u64 {
    6000
}
fn d_segments() -> usize {
    4
}
impl Default for PacerSettings {
    fn default() -> Self {
        Self { debounce_ms: d_debounce(), max_wait_ms: d_max_wait(), max_segments: d_segments() }
    }
}

/// 内置工具（docs/06-tools-plugins.md 第 1 节）。路径均已解析为绝对路径。
#[derive(Debug, Clone)]
pub struct ToolsConfig {
    pub shell: ShellSettings,
    /// 工具产出的媒体（生图、语音、附件）落盘目录
    pub media_dir: PathBuf,
    /// SKILL.md 技能目录
    pub skills_dir: PathBuf,
    /// 工具调用审计日志（JSON Lines）
    pub audit_log: PathBuf,
    /// Komari 配置文件（含 base_url）；缺省时不注册流量日报工具
    pub komari_config: Option<PathBuf>,
    /// 博客同步脚本；缺省时不注册
    pub blog_script: Option<PathBuf>,
    pub blog_timeout_s: u64,
    pub mail: Option<MailSettings>,
    /// watch_video 使用的模型（须在主对话角色提供方的模型目录中且支持视频）
    pub video_model: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ShellSettings {
    /// 解释器（以 `-lc` 执行命令）
    pub shell: String,
    pub workdir: Option<PathBuf>,
    pub timeout_s: u64,
    pub max_timeout_s: u64,
    /// rtk 可执行文件；缺省或找不到时不改写命令
    pub rtk: Option<String>,
    /// 返回给模型的输出上限（字符），超出保留首尾
    pub output_limit: usize,
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self { shell: "bash".into(), workdir: None, timeout_s: 120, max_timeout_s: 1300, rtk: Some("rtk".into()), output_limit: 8000 }
    }
}

#[derive(Debug, Clone)]
pub struct MailSettings {
    pub python: String,
    pub helper: PathBuf,
    pub config: PathBuf,
}

/// QQ 官方机器人（docs/07-platform-commands.md 第 2 节）。
#[derive(Debug, Clone)]
pub struct QqConfig {
    pub app_id: String,
    pub secret: Secret,
    /// 允许的 user_openid（只服务用户）
    pub allowed_users: Vec<String>,
    pub api_base: String,
    pub token_url: String,
    /// 被动回复有效期（分钟）与单条消息最多回复次数
    pub reply_window_min: u64,
    pub max_replies: u32,
    /// 每月主动消息额度（超出后延迟到下次用户说话时一并发出）
    pub proactive_monthly_quota: u32,
    /// 入站附件落盘目录
    pub inbox_dir: PathBuf,
}

/// 抖音外部服务（docs/06-tools-plugins.md 第 3 节）。
#[derive(Debug, Clone)]
pub struct DouyinConfig {
    /// 抖音服务的 Unix socket
    pub socket: PathBuf,
    /// 固定制作目录根（每个作品一个子目录，内含 <release_id>_<4x3|3x4|9x16|16x9>.<png|jpg|webp>）
    pub saved_covers_dir: PathBuf,
    /// 二维码与封面预览缓存目录
    pub cache_dir: PathBuf,
}

/// 可插拔提供方（docs/06-tools-plugins.md 第 2 节）：类别 → { active, providers{名 → 原始表} }。
/// 各类别的字段由 lyrabot-plugins 按 `kind` 解析，配置层只保证结构。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PluginsConfig {
    #[serde(flatten)]
    pub categories: BTreeMap<String, PluginCategory>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCategory {
    pub active: String,
    pub providers: BTreeMap<String, toml::Table>,
}

impl PluginsConfig {
    /// 当前激活的提供方：(名字, 原始表)。
    pub fn active(&self, category: &str) -> Option<(&str, &toml::Table)> {
        let c = self.categories.get(category)?;
        c.providers.get(&c.active).map(|t| (c.active.as_str(), t))
    }
}

/// 多代理编排（docs/05-orchestra.md）。
#[derive(Debug, Clone)]
pub struct OrchestraConfig {
    pub provider: String,
    pub default_model: String,
    pub default_effort: Effort,
    /// 同时运行的后台执行者上限（worker + 专员）
    pub max_concurrent: usize,
    /// 后台执行者（worker 与专员）上下文窗口上限（默认 131072，即 128K）
    pub context_window: u64,
    /// 后台执行者的压缩触发比例与保留尾部；未配置时沿用 `[agent]`
    pub compaction_ratio: f64,
    pub compaction_keep_tokens: u64,
    /// 执行者系统提示文件
    pub worker_prompt: PathBuf,
    /// 任务型 worker 可用的工具名
    pub worker_tools: Vec<String>,
    pub specialists: BTreeMap<String, SpecialistConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecialistConfig {
    /// 专长说明：写进主对话角色的 delegate 工具说明与专员系统提示
    pub description: String,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    #[serde(default)]
    pub tools: Vec<String>,
    /// 专员长期记忆文件名（位于 memory_dir 下）
    pub memory: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    /// 上下文占模型窗口的比例达到该值时压缩
    #[serde(default = "d_ratio")]
    pub compaction_ratio: f64,
    /// 压缩时保留的最近消息估算 token 数
    #[serde(default = "d_keep")]
    pub compaction_keep_tokens: u64,
    /// 单轮最多工具调用次数
    #[serde(default = "d_calls")]
    pub max_tool_calls: u32,
    /// /gc 与每周自动清理的默认保留天数
    #[serde(default = "d_gc_days")]
    pub gc_days: u32,
}
fn d_gc_days() -> u32 {
    30
}
fn d_ratio() -> f64 {
    0.9
}
fn d_keep() -> u64 {
    20_000
}
fn d_calls() -> u32 {
    40
}
impl Default for AgentConfig {
    fn default() -> Self {
        Self { compaction_ratio: d_ratio(), compaction_keep_tokens: d_keep(), max_tool_calls: d_calls(), gc_days: d_gc_days() }
    }
}
