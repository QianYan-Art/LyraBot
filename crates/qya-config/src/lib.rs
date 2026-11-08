//! 配置装载：TOML 主配置 + 独立 secret 文件。
//!
//! - 相对路径一律以配置文件所在目录为基准，开发机与服务器共用同一套写法。
//! - secret 只从 `*_file` 指向的文件读取，读入后包成 [`Secret`]，不进日志。
//! - [`ConfigHandle`] 用 `ArcSwap` 持有当前配置：读零锁，`reload` 原子替换。

use arc_swap::ArcSwap;
use qya_core::{Effort, Secret};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("读取 {path} 失败：{source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("解析 {path} 失败：{source}")]
    Parse { path: PathBuf, source: toml::de::Error },
    #[error("配置无效：{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Debug, Clone)]
pub struct Config {
    /// 配置文件所在目录
    pub base_dir: PathBuf,
    pub data_dir: PathBuf,
    pub llm: LlmConfig,
    pub persona: PersonaConfig,
    pub orchestra: OrchestraConfig,
    pub tools: ToolsConfig,
    pub plugins: PluginsConfig,
    pub qq: Option<QqConfig>,
    pub douyin: Option<DouyinConfig>,
    pub agent: AgentConfig,
}

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub default_provider: String,
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone, Copy, Deserialize)]
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
    pub learner: LearnerSettings,
    /// 唐语歌可直接使用的轻工具（注册表中的名字；人格专属工具另行加入）
    pub tools: Vec<String>,
}

/// 表达/黑话学习参数（docs/01-architecture.md 6.3）。
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct LearnerSettings {
    pub enabled: bool,
    /// 学习用的低价模型（需在唐语歌所用提供方的模型目录中）
    pub model: String,
    pub effort: Effort,
    /// 累计多少条阿颜的新消息触发一次学习
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

/// 回复节奏参数（docs/01-architecture.md 6.2）。
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

/// 内置工具（docs/01-architecture.md 8.1）。路径均已解析为绝对路径。
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

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawTools {
    #[serde(default)]
    shell: ShellSettings,
    media_dir: Option<PathBuf>,
    skills_dir: Option<PathBuf>,
    audit_log: Option<PathBuf>,
    komari_config: Option<PathBuf>,
    blog_script: Option<PathBuf>,
    blog_timeout_s: Option<u64>,
    mail: Option<RawMail>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMail {
    #[serde(default = "d_python")]
    python: String,
    helper: PathBuf,
    config: PathBuf,
}
fn d_python() -> String {
    "python3".into()
}

/// QQ 官方机器人（docs/01-architecture.md 9.1）。
#[derive(Debug, Clone)]
pub struct QqConfig {
    pub app_id: String,
    pub secret: Secret,
    /// 允许的 user_openid（只服务阿颜）
    pub allowed_users: Vec<String>,
    pub api_base: String,
    pub token_url: String,
    /// 被动回复有效期（分钟）与单条消息最多回复次数
    pub reply_window_min: u64,
    pub max_replies: u32,
    /// 每月主动消息额度（超出后延迟到下次阿颜说话时一并发出）
    pub proactive_monthly_quota: u32,
    /// 入站附件落盘目录
    pub inbox_dir: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQq {
    app_id: String,
    secret_file: PathBuf,
    allowed_users: Vec<String>,
    #[serde(default = "d_qq_api")]
    api_base: String,
    #[serde(default = "d_qq_token")]
    token_url: String,
    #[serde(default = "d_window")]
    reply_window_min: u64,
    #[serde(default = "d_replies")]
    max_replies: u32,
    #[serde(default = "d_quota")]
    proactive_monthly_quota: u32,
    inbox_dir: Option<PathBuf>,
}
fn d_qq_api() -> String {
    "https://api.sgroup.qq.com".into()
}
fn d_qq_token() -> String {
    "https://bots.qq.com/app/getAppAccessToken".into()
}
fn d_window() -> u64 {
    60
}
fn d_replies() -> u32 {
    5
}
fn d_quota() -> u32 {
    4
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawPlatforms {
    qq: Option<RawQq>,
}

/// 抖音外部服务（docs/01-architecture.md 8.3）。
#[derive(Debug, Clone)]
pub struct DouyinConfig {
    /// 抖音服务的 Unix socket
    pub socket: PathBuf,
    /// 固定制作目录根（每个作品一个子目录，内含 <release_id>_<4x3|3x4|9x16|16x9>.<png|jpg|webp>）
    pub saved_covers_dir: PathBuf,
    /// 二维码与封面预览缓存目录
    pub cache_dir: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDouyin {
    socket: PathBuf,
    saved_covers_dir: PathBuf,
    cache_dir: Option<PathBuf>,
}

/// 可插拔提供方（docs/01-architecture.md 8.2）：类别 → { active, providers{名 → 原始表} }。
/// 各类别的字段由 qya-plugins 按 `kind` 解析，配置层只保证结构。
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

/// 多代理编排（docs/01-architecture.md 第 7 节）。
#[derive(Debug, Clone)]
pub struct OrchestraConfig {
    pub provider: String,
    pub default_model: String,
    pub default_effort: Effort,
    /// 同时运行的后台执行者上限（worker + 专员）
    pub max_concurrent: usize,
    /// 执行者系统提示文件
    pub worker_prompt: PathBuf,
    /// 任务型 worker 可用的工具名
    pub worker_tools: Vec<String>,
    pub specialists: BTreeMap<String, SpecialistConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecialistConfig {
    /// 专长说明：写进唐语歌的 delegate 工具说明与专员系统提示
    pub description: String,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    #[serde(default)]
    pub tools: Vec<String>,
    /// 专员长期记忆文件名（位于 memory_dir 下）
    pub memory: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOrchestra {
    provider: Option<String>,
    #[serde(default = "d_worker_model")]
    default_model: String,
    #[serde(default = "d_worker_effort")]
    default_effort: Effort,
    #[serde(default = "d_concurrent")]
    max_concurrent: usize,
    worker_prompt: PathBuf,
    #[serde(default)]
    worker_tools: Vec<String>,
    #[serde(default)]
    specialists: BTreeMap<String, SpecialistConfig>,
}
fn d_worker_model() -> String {
    "kimi-for-coding".into()
}
fn d_worker_effort() -> Effort {
    Effort::High
}
fn d_concurrent() -> usize {
    3
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

// ---------- 原始 TOML 结构 ----------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    paths: RawPaths,
    llm: RawLlm,
    persona: RawPersona,
    orchestra: RawOrchestra,
    #[serde(default)]
    tools: RawTools,
    #[serde(default)]
    plugins: PluginsConfig,
    #[serde(default)]
    platforms: RawPlatforms,
    douyin: Option<RawDouyin>,
    #[serde(default)]
    agent: AgentConfig,
}

#[derive(Deserialize, Default)]
struct RawPaths {
    data_dir: Option<PathBuf>,
}

#[derive(Deserialize)]
struct RawLlm {
    default_provider: String,
    providers: BTreeMap<String, RawProvider>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    base_url: String,
    key_file: PathBuf,
    transport: Transport,
    user_agent: Option<String>,
    #[serde(default)]
    effort_map: BTreeMap<String, String>,
    #[serde(default)]
    models: BTreeMap<String, ModelInfo>,
    #[serde(default)]
    timeouts: Timeouts,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPersona {
    provider: Option<String>,
    model: String,
    effort: String,
    assets_dir: PathBuf,
    memory_dir: Option<PathBuf>,
    stickers_dir: Option<PathBuf>,
    #[serde(default)]
    pacer: PacerSettings,
    #[serde(default)]
    learner: LearnerSettings,
    #[serde(default = "d_persona_tools")]
    tools: Vec<String>,
}

fn d_persona_tools() -> Vec<String> {
    ["image_generate", "speak", "web_search", "cron", "skill_view"].map(str::to_owned).to_vec()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = read(path)?;
        let raw: RawConfig =
            toml::from_str(&text).map_err(|source| ConfigError::Parse { path: path.to_owned(), source })?;
        let base_dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        let resolve = |p: &Path| if p.is_absolute() { p.to_owned() } else { base_dir.join(p) };

        let mut providers = BTreeMap::new();
        for (id, rp) in raw.llm.providers {
            let key = read(&resolve(&rp.key_file))?.trim().to_owned();
            if key.is_empty() {
                return Err(ConfigError::Invalid(format!("provider `{id}` 的 key 文件为空")));
            }
            let mut effort_map = BTreeMap::new();
            for (k, v) in rp.effort_map {
                let e: Effort = k.parse().map_err(ConfigError::Invalid)?;
                effort_map.insert(e, v);
            }
            providers.insert(
                id.clone(),
                ProviderConfig {
                    id,
                    base_url: rp.base_url.trim_end_matches('/').to_owned(),
                    api_key: Secret::new(key),
                    transport: rp.transport,
                    user_agent: rp.user_agent,
                    effort_map,
                    models: rp.models,
                    timeouts: rp.timeouts,
                },
            );
        }
        if !providers.contains_key(&raw.llm.default_provider) {
            return Err(ConfigError::Invalid(format!("default_provider `{}` 未定义", raw.llm.default_provider)));
        }

        let data_dir = resolve(raw.paths.data_dir.as_deref().unwrap_or(Path::new("data")));
        let persona_provider = raw.persona.provider.unwrap_or_else(|| raw.llm.default_provider.clone());
        let persona = PersonaConfig {
            model: raw.persona.model,
            effort: raw.persona.effort.parse().map_err(ConfigError::Invalid)?,
            assets_dir: resolve(&raw.persona.assets_dir),
            memory_dir: raw.persona.memory_dir.as_deref().map(resolve).unwrap_or_else(|| data_dir.join("memory")),
            stickers_dir: raw.persona.stickers_dir.as_deref().map(resolve).unwrap_or_else(|| data_dir.join("stickers")),
            pacer: raw.persona.pacer,
            learner: raw.persona.learner,
            tools: raw.persona.tools,
            provider: persona_provider,
        };
        let ro = raw.orchestra;
        let orchestra = OrchestraConfig {
            provider: ro.provider.unwrap_or_else(|| raw.llm.default_provider.clone()),
            default_model: ro.default_model,
            default_effort: ro.default_effort,
            max_concurrent: ro.max_concurrent,
            worker_prompt: resolve(&ro.worker_prompt),
            worker_tools: ro.worker_tools,
            specialists: ro.specialists,
        };
        let rt = raw.tools;
        let mut shell = rt.shell;
        shell.workdir = shell.workdir.as_deref().map(resolve);
        let tools = ToolsConfig {
            shell,
            media_dir: rt.media_dir.as_deref().map(resolve).unwrap_or_else(|| data_dir.join("media")),
            skills_dir: rt.skills_dir.as_deref().map(resolve).unwrap_or_else(|| base_dir.join("skills")),
            audit_log: rt.audit_log.as_deref().map(resolve).unwrap_or_else(|| data_dir.join("audit.jsonl")),
            komari_config: rt.komari_config.as_deref().map(resolve),
            blog_script: rt.blog_script.as_deref().map(resolve),
            blog_timeout_s: rt.blog_timeout_s.unwrap_or(1300),
            mail: rt.mail.map(|m| MailSettings { python: m.python, helper: resolve(&m.helper), config: resolve(&m.config) }),
        };
        for (name, cat) in &raw.plugins.categories {
            if !cat.providers.contains_key(&cat.active) {
                return Err(ConfigError::Invalid(format!("plugins.{name}.active `{}` 未在 providers 中定义", cat.active)));
            }
        }
        let qq = match raw.platforms.qq {
            None => None,
            Some(q) => {
                // 会话、回复锚点与收件人都是单用户设计，多人会串话，因此必须恰好一个
                if q.allowed_users.len() != 1 {
                    return Err(ConfigError::Invalid("platforms.qq.allowed_users 必须恰好包含一个 user_openid（单用户设计）".into()));
                }
                let secret = read_secret_line(&resolve(&q.secret_file))?;
                Some(QqConfig {
                    app_id: q.app_id,
                    secret,
                    allowed_users: q.allowed_users,
                    api_base: q.api_base.trim_end_matches('/').to_owned(),
                    token_url: q.token_url,
                    reply_window_min: q.reply_window_min,
                    max_replies: q.max_replies.max(1),
                    proactive_monthly_quota: q.proactive_monthly_quota,
                    inbox_dir: q.inbox_dir.as_deref().map(resolve).unwrap_or_else(|| data_dir.join("media").join("inbox")),
                })
            }
        };
        let douyin = raw.douyin.map(|d| DouyinConfig {
            socket: resolve(&d.socket),
            saved_covers_dir: resolve(&d.saved_covers_dir),
            cache_dir: d.cache_dir.as_deref().map(resolve).unwrap_or_else(|| data_dir.join("media").join("douyin")),
        });
        let cfg = Config {
            qq,
            douyin,
            data_dir,
            llm: LlmConfig { default_provider: raw.llm.default_provider, providers },
            persona,
            orchestra,
            tools,
            plugins: raw.plugins,
            agent: raw.agent,
            base_dir,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        let p = self
            .llm
            .providers
            .get(&self.persona.provider)
            .ok_or_else(|| ConfigError::Invalid(format!("persona.provider `{}` 未定义", self.persona.provider)))?;
        if !p.models.contains_key(&self.persona.model) {
            return Err(ConfigError::Invalid(format!(
                "persona.model `{}` 不在 provider `{}` 的模型目录中",
                self.persona.model, p.id
            )));
        }
        if self.persona.learner.enabled && !p.models.contains_key(&self.persona.learner.model) {
            return Err(ConfigError::Invalid(format!(
                "persona.learner.model `{}` 不在模型目录中",
                self.persona.learner.model
            )));
        }
        let o = &self.orchestra;
        let op = self
            .llm
            .providers
            .get(&o.provider)
            .ok_or_else(|| ConfigError::Invalid(format!("orchestra.provider `{}` 未定义", o.provider)))?;
        let check_model = |m: &str, field: &str| {
            if op.models.contains_key(m) {
                Ok(())
            } else {
                Err(ConfigError::Invalid(format!("{field} `{m}` 不在 provider `{}` 的模型目录中", op.id)))
            }
        };
        check_model(&o.default_model, "orchestra.default_model")?;
        for (name, s) in &o.specialists {
            if let Some(m) = &s.model {
                check_model(m, &format!("orchestra.specialists.{name}.model"))?;
            }
            if name == "worker" || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(ConfigError::Invalid(format!("专员名 `{name}` 只能用字母数字下划线，且不能叫 worker")));
            }
        }
        if o.max_concurrent == 0 {
            return Err(ConfigError::Invalid("orchestra.max_concurrent 至少为 1".into()));
        }
        let pc = &self.persona.pacer;
        if pc.max_segments == 0 || pc.debounce_ms > pc.max_wait_ms {
            return Err(ConfigError::Invalid("persona.pacer：max_segments 至少为 1，且 debounce_ms 不大于 max_wait_ms".into()));
        }
        if !(0.3..=0.98).contains(&self.agent.compaction_ratio) {
            return Err(ConfigError::Invalid("agent.compaction_ratio 应在 0.3~0.98".into()));
        }
        Ok(())
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.llm.providers.get(id)
    }

    /// 解析配置中的相对路径（以配置文件所在目录为基准）。
    pub fn resolve(&self, p: &Path) -> PathBuf {
        if p.is_absolute() { p.to_owned() } else { self.base_dir.join(p) }
    }
}

/// 读取 secret 文件的第一行非空内容（兼容“首行 key、次行 url”的写法）。
pub fn read_secret_line(path: &Path) -> Result<Secret> {
    let text = read(path)?;
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    if line.is_empty() {
        return Err(ConfigError::Invalid(format!("secret 文件为空：{}", path.display())));
    }
    Ok(Secret::new(line))
}

impl ProviderConfig {
    /// 将规范化等级映射为提供方参数；未配置映射时原样使用等级名。
    pub fn effort_param(&self, e: Effort) -> String {
        self.effort_map.get(&e).cloned().unwrap_or_else(|| e.as_str().to_owned())
    }
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|source| ConfigError::Io { path: path.to_owned(), source })
}

/// 运行期配置句柄：读零锁，`reload` 校验通过才替换，失败保留旧配置。
pub struct ConfigHandle {
    path: PathBuf,
    current: ArcSwap<Config>,
}

impl ConfigHandle {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let cfg = Config::load(&path)?;
        Ok(Self { path, current: ArcSwap::from_pointee(cfg) })
    }
    pub fn get(&self) -> Arc<Config> {
        self.current.load_full()
    }
    pub fn reload(&self) -> Result<Arc<Config>> {
        let cfg = Arc::new(Config::load(&self.path)?);
        self.current.store(cfg.clone());
        Ok(cfg)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(dir: &Path, key: &str) -> PathBuf {
        std::fs::write(dir.join("k.txt"), key).unwrap();
        let cfg = r#"
[llm]
default_provider = "kimi-code"
[llm.providers.kimi-code]
base_url = "https://example.invalid/v1/"
key_file = "k.txt"
transport = "chat_completions"
[llm.providers.kimi-code.effort_map]
medium = "high"
[llm.providers.kimi-code.models.k3-256k]
context_length = 262144
vision = true
[llm.providers.kimi-code.models.kimi-for-coding]
context_length = 1048576
[persona]
model = "k3-256k"
effort = "low"
assets_dir = "persona"
[orchestra]
worker_prompt = "agents/worker.md"
[orchestra.specialists.ops]
description = "运维"
model = "k3-256k"
"#;
        let p = dir.join("qya.toml");
        std::fs::write(&p, cfg).unwrap();
        p
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qya-config-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn loads_and_hides_secret() {
        let d = tmpdir("load");
        let p = write_fixture(&d, "  sk-secret-value\n");
        let c = Config::load(&p).unwrap();
        let pc = c.provider("kimi-code").unwrap();
        assert_eq!(pc.api_key.expose(), "sk-secret-value");
        assert_eq!(pc.base_url, "https://example.invalid/v1");
        assert_eq!(pc.effort_param(Effort::Medium), "high");
        assert_eq!(pc.effort_param(Effort::Low), "low");
        assert_eq!(c.persona.assets_dir, d.join("persona"));
        assert_eq!(c.data_dir, d.join("data"));
        assert_eq!(c.persona.memory_dir, d.join("data").join("memory"));
        assert!(!format!("{c:?}").contains("sk-secret-value"));
    }

    #[test]
    fn reload_keeps_old_on_error() {
        let d = tmpdir("reload");
        let p = write_fixture(&d, "k1");
        let h = ConfigHandle::open(&p).unwrap();
        std::fs::write(d.join("k.txt"), "k2").unwrap();
        assert_eq!(h.reload().unwrap().provider("kimi-code").unwrap().api_key.expose(), "k2");
        std::fs::write(&p, "not = [valid").unwrap();
        assert!(h.reload().is_err());
        assert_eq!(h.get().provider("kimi-code").unwrap().api_key.expose(), "k2");
    }

    #[test]
    fn rejects_unknown_model() {
        let d = tmpdir("badmodel");
        let p = write_fixture(&d, "k");
        let t = std::fs::read_to_string(&p).unwrap().replace("model = \"k3-256k\"", "model = \"nope\"");
        std::fs::write(&p, t).unwrap();
        assert!(matches!(Config::load(&p), Err(ConfigError::Invalid(_))));
    }
}
