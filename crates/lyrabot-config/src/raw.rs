//! TOML 原始反序列化结构与路径展开转换。

use crate::types::{
    AgentConfig, DouyinConfig, KeepaliveSettings, LearnerSettings, LlmConfig, MailSettings, ModelInfo, OrchestraConfig,
    PacerSettings, PersonaConfig, PluginsConfig, ProviderConfig, QqConfig, ShellSettings, SpecialistConfig, Timeouts, ToolsConfig,
    Transport,
};
use crate::{Config, ConfigError, Result, read_secret_line};
use lyrabot_core::{Effort, Secret};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    video_model: Option<String>,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDouyin {
    socket: PathBuf,
    saved_covers_dir: PathBuf,
    cache_dir: Option<PathBuf>,
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
    #[serde(default = "d_orchestra_window")]
    context_window: u64,
    compaction_ratio: Option<f64>,
    compaction_keep_tokens: Option<u64>,
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
fn d_orchestra_window() -> u64 {
    131_072
}

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
    keepalive: KeepaliveSettings,
    #[serde(default)]
    learner: LearnerSettings,
    #[serde(default = "d_persona_tools")]
    tools: Vec<String>,
}

fn d_persona_tools() -> Vec<String> {
    ["image_generate", "speak", "web_search", "cron", "skill_view", "recall"].map(str::to_owned).to_vec()
}

pub(crate) fn read_file(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|source| ConfigError::Io { path: path.to_owned(), source })
}

pub(crate) fn load_config(path: &Path) -> Result<Config> {
    let text = read_file(path)?;
    let raw: RawConfig = toml::from_str(&text).map_err(|source| ConfigError::Parse { path: path.to_owned(), source })?;
    let base_dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let resolve = |p: &Path| if p.is_absolute() { p.to_owned() } else { base_dir.join(p) };

    let mut providers = BTreeMap::new();
    for (id, rp) in raw.llm.providers {
        let key = read_file(&resolve(&rp.key_file))?.trim().to_owned();
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
        keepalive: raw.persona.keepalive,
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
        context_window: ro.context_window,
        compaction_ratio: ro.compaction_ratio.unwrap_or(raw.agent.compaction_ratio),
        compaction_keep_tokens: ro.compaction_keep_tokens.unwrap_or(raw.agent.compaction_keep_tokens),
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
        video_model: rt.video_model.unwrap_or_else(|| "kimi-for-coding".into()),
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
