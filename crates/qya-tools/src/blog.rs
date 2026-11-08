//! blog_sync：执行博客同步脚本。脚本自带“代理优先、失败回退直连”；日志出现“代理同步失败”时如实汇报，但不干预脚本。

use crate::util::{Audit, exec, now_ms, tail_chars};
use async_trait::async_trait;
use qya_agent::{Tool, ToolCtx, ToolOutput};
use qya_core::ToolSpec;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

pub struct BlogSync {
    spec: ToolSpec,
    script: PathBuf,
    timeout: Duration,
    audit: Audit,
}

impl BlogSync {
    pub fn new(script: PathBuf, timeout_s: u64, audit: Audit) -> Self {
        let spec = ToolSpec {
            name: "blog_sync".into(),
            description: format!(
                "执行博客文章同步（把知识库文章同步到博客）。阿颜说“博客同步 / 同步博客文章”时调用。脚本优先走代理，失败会自动回退直连，正常约 2 分钟，最长 {} 秒；中途不要重复调用。",
                timeout_s
            ),
            parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        };
        Self { spec, script, timeout: Duration::from_secs(timeout_s), audit }
    }
}

/// 从脚本输出整理汇报：代理失败提示 + 退出状态 + 日志尾部。
pub fn summarize(output: &str, code: Option<i32>, timed_out: bool) -> (String, bool) {
    let mut lines = Vec::new();
    if output.contains("代理同步失败") {
        lines.push("注意：日志出现“代理同步失败”，脚本已回退直连继续同步。".to_owned());
    }
    let ok = !timed_out && code == Some(0);
    lines.push(match (timed_out, code) {
        (true, _) => "同步超时，已终止脚本。".to_owned(),
        (false, Some(0)) => "同步完成。".to_owned(),
        (false, Some(c)) => format!("同步失败，退出码 {c}。"),
        (false, None) => "同步脚本被信号终止。".to_owned(),
    });
    lines.push(format!("日志尾部：\n{}", tail_chars(output.trim_end(), 3000)));
    (lines.join("\n"), ok)
}

#[async_trait]
impl Tool for BlogSync {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, ctx: ToolCtx, _args: Value) -> ToolOutput {
        let script = self.script.display().to_string();
        let res = exec("bash", std::slice::from_ref(&script), None, &[], self.timeout, &ctx.cancel).await;
        let out = match res {
            Ok(o) => o,
            Err(e) => return ToolOutput::err(format!("无法执行 {script}：{e}")),
        };
        self.audit
            .record(json!({"ts": now_ms(), "session": ctx.session, "tool": "blog_sync", "exit": out.code, "timed_out": out.timed_out, "ms": out.elapsed.as_millis() as u64}))
            .await;
        let (text, ok) = summarize(&out.output, out.code, out.timed_out);
        if ok { ToolOutput::ok(text) } else { ToolOutput::err(text) }
    }
}

#[cfg(test)]
mod tests {
    use super::summarize;

    #[test]
    fn proxy_failure_reported_once() {
        let (t, ok) = summarize("开始\n代理同步失败: timeout\n直连同步成功\n", Some(0), false);
        assert!(ok && t.starts_with("注意：日志出现“代理同步失败”") && t.contains("同步完成"));
        let (t, ok) = summarize("x", Some(2), false);
        assert!(!ok && t.contains("退出码 2") && !t.contains("代理"));
        assert!(summarize("x", None, true).0.contains("超时"));
    }
}
