//! 配置装载：TOML 主配置 + 独立 secret 文件。
//!
//! - 相对路径一律以配置文件所在目录为基准，开发机与服务器共用同一套写法。
//! - secret 只从 `*_file` 指向的文件读取，读入后包成 [`Secret`]，不进日志。
//! - [`ConfigHandle`] 用 `ArcSwap` 持有当前配置：读零锁，`reload` 原子替换。

mod raw;
mod types;

pub use types::{
    AgentConfig, DouyinConfig, KeepaliveSettings, LearnerSettings, LlmConfig, MailSettings, ModelInfo, OrchestraConfig,
    PacerSettings, PersonaConfig, PluginCategory, PluginsConfig, ProviderConfig, QqConfig, ShellSettings, SpecialistConfig,
    Timeouts, ToolsConfig, Transport,
};

use arc_swap::ArcSwap;
use lyrabot_core::Secret;
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

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        raw::load_config(path)
    }

    pub(crate) fn validate(&self) -> Result<()> {
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
        if o.context_window == 0 {
            return Err(ConfigError::Invalid("orchestra.context_window 必须大于 0".into()));
        }
        let pc = &self.persona.pacer;
        if pc.max_segments == 0 || pc.debounce_ms > pc.max_wait_ms {
            return Err(ConfigError::Invalid("persona.pacer：max_segments 至少为 1，且 debounce_ms 不大于 max_wait_ms".into()));
        }
        for (field, r) in [("agent.compaction_ratio", self.agent.compaction_ratio), ("orchestra.compaction_ratio", self.orchestra.compaction_ratio)] {
            if !(0.3..=0.98).contains(&r) {
                return Err(ConfigError::Invalid(format!("{field} 应在 0.3~0.98")));
            }
        }
        if self.agent.compaction_keep_tokens == 0 || self.orchestra.compaction_keep_tokens == 0 {
            return Err(ConfigError::Invalid("compaction_keep_tokens 必须大于 0".into()));
        }
        if self.orchestra.compaction_keep_tokens >= self.orchestra.context_window {
            return Err(ConfigError::Invalid("orchestra.compaction_keep_tokens 必须小于 orchestra.context_window".into()));
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
    let text = raw::read_file(path)?;
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    if line.is_empty() {
        return Err(ConfigError::Invalid(format!("secret 文件为空：{}", path.display())));
    }
    Ok(Secret::new(line))
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
        let cfg = self.load()?;
        self.publish(cfg.clone());
        Ok(cfg)
    }
    /// 只读取并校验配置文件，不替换当前配置；与依赖它的资产一起验证通过后再 [`publish`](Self::publish)。
    pub fn load(&self) -> Result<Arc<Config>> {
        Ok(Arc::new(Config::load(&self.path)?))
    }
    pub fn publish(&self, cfg: Arc<Config>) {
        self.current.store(cfg);
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lyrabot_core::Effort;

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
        let p = dir.join("lyrabot.toml");
        std::fs::write(&p, cfg).unwrap();
        p
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lyrabot-config-test-{name}-{}", std::process::id()));
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
        let _ = std::fs::remove_dir_all(&d);
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
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rejects_unknown_model() {
        let d = tmpdir("badmodel");
        let p = write_fixture(&d, "k");
        let t = std::fs::read_to_string(&p).unwrap().replace("model = \"k3-256k\"", "model = \"nope\"");
        std::fs::write(&p, t).unwrap();
        assert!(matches!(Config::load(&p), Err(ConfigError::Invalid(_))));
        let _ = std::fs::remove_dir_all(&d);
    }
}
