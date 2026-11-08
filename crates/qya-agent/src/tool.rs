use async_trait::async_trait;
use qya_core::{SessionId, ToolSpec};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 工具并发声明：内核据此决定同一批调用并行还是串行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concurrency {
    /// 只读、无副作用，可与其他 Shared 工具并行
    Shared,
    /// 有副作用，必须独占执行
    Exclusive,
}

#[derive(Debug, Clone)]
pub struct ToolCtx {
    pub session: SessionId,
    pub call_id: String,
    pub cancel: CancellationToken,
}

#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    /// 回传给模型的文本
    pub text: String,
    pub is_error: bool,
    /// 需要投递给用户的媒体（走平台投递通道，不靠文本标签）
    pub media: Vec<PathBuf>,
}

impl ToolOutput {
    pub fn ok(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Default::default() }
    }
    pub fn err(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true, ..Default::default() }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }
    async fn call(&self, ctx: ToolCtx, args: serde_json::Value) -> ToolOutput;
}

/// 全局工具注册表；会话按工具名白名单（toolset）取子集。
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        let name = tool.spec().name.clone();
        assert!(!self.tools.contains_key(&name), "重复注册工具 `{name}`");
        self.tools.insert(name, tool);
        self
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// 按白名单选出工具集；未知名字会被报告而不是静默忽略。
    pub fn select(&self, names: &[&str]) -> Result<Toolset, String> {
        let mut tools = BTreeMap::new();
        for n in names {
            let t = self.tools.get(*n).ok_or_else(|| format!("未注册的工具 `{n}`"))?;
            tools.insert((*n).to_owned(), t.clone());
        }
        Ok(Toolset { tools })
    }

    pub fn all(&self) -> Toolset {
        Toolset { tools: self.tools.clone() }
    }
}

/// 某个会话可用的工具子集。顺序稳定（按名字排序），保证工具 schema 字节一致以命中缓存。
#[derive(Default, Clone)]
pub struct Toolset {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl Toolset {
    /// 加入一个会话专属工具（如人格资料工具），同名覆盖。
    pub fn with(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.insert(tool.spec().name.clone(), tool);
        self
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec().clone()).collect()
    }
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
