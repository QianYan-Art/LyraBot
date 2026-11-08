//! shell：以 `<shell> -lc` 执行命令；RTK 改写长输出命令、硬禁止高危命令、审计每次调用。
//! 输出超过上限时，完整内容存进溢出目录（随媒体清理回收），模型只看到首尾和文件路径，需要时再分段读取。

use crate::util::{Audit, arg_str, arg_u64, exec, now_ms, truncate_middle};
use async_trait::async_trait;
use qya_agent::{Tool, ToolCtx, ToolOutput};
use qya_config::ShellSettings;
use qya_core::ToolSpec;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

pub struct Shell {
    spec: ToolSpec,
    cfg: ShellSettings,
    audit: Audit,
    /// 长输出的完整内容存放目录；未设置时只截断
    spill_dir: Option<PathBuf>,
}

impl Shell {
    pub fn new(cfg: ShellSettings, audit: Audit) -> Self {
        let spec = ToolSpec {
            name: "shell".into(),
            description: format!(
                "在服务器上用 bash 执行命令并返回输出（stdout 与 stderr）。默认超时 {} 秒，可用 timeout_s 延长到 {} 秒。\
                 长输出会自动压缩，超长时只返回首尾并把完整输出存成文件（附路径，可用 fs_search / fs_read 细看）。不要执行交互式命令。",
                cfg.timeout_s, cfg.max_timeout_s
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_s": {"type": "integer", "minimum": 1},
                    "workdir": {"type": "string", "description": "工作目录（可选）"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        };
        Self { spec, cfg, audit, spill_dir: None }
    }

    /// 设置长输出的存放目录。
    pub fn with_spill(mut self, dir: PathBuf) -> Self {
        self.spill_dir = Some(dir);
        self
    }

    /// 把完整输出写入溢出目录，返回文件路径；失败时只截断，不影响本次调用。
    async fn spill(&self, session: i64, text: &str) -> Option<PathBuf> {
        let dir = self.spill_dir.as_ref()?;
        tokio::fs::create_dir_all(dir).await.ok()?;
        let path = dir.join(format!("shell-{}-{session}.txt", now_ms()));
        match tokio::fs::write(&path, text).await {
            Ok(()) => Some(path),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "保存完整输出失败");
                None
            }
        }
    }

    /// 调用 `rtk rewrite`，只在 RTK 明确给出改写（退出码 0/3）时采用；任何异常都原样执行。
    async fn rewrite(&self, command: &str) -> Option<String> {
        let rtk = self.cfg.rtk.as_deref()?;
        let out = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::process::Command::new(rtk).arg("rewrite").arg(command).kill_on_drop(true).output(),
        )
        .await
        .ok()?
        .ok()?;
        let rewritten = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (matches!(out.status.code(), Some(0 | 3)) && !rewritten.is_empty() && rewritten != command).then_some(rewritten)
    }
}

/// 硬禁止：不可逆、破坏系统的命令。返回命中的原因。
pub fn denied(command: &str) -> Option<&'static str> {
    let c: String = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = c.to_lowercase();
    const PROTECTED: &[&str] = &["/", "/*", "/etc", "/usr", "/bin", "/boot", "/lib", "/var", "/var/lib/qya", "/home", "/root", "/opt/qya", "~"];
    // rm 递归删除受保护目录
    for seg in lower.split(['&', '|', ';']) {
        let words: Vec<&str> = seg.split_whitespace().collect();
        if let Some(i) = words.iter().position(|w| *w == "rm" || w.ends_with("/rm")) {
            let args = &words[i + 1..];
            let recursive = args.iter().any(|a| a.starts_with('-') && (a.contains('r') || a.contains('R')) || *a == "--recursive");
            if recursive && args.iter().any(|a| PROTECTED.contains(&a.trim_end_matches('/')) || PROTECTED.contains(a)) {
                return Some("递归删除系统或数据目录");
            }
        }
        if words.first().is_some_and(|w| matches!(*w, "shutdown" | "reboot" | "poweroff" | "halt" | "init")) {
            return Some("关机或重启");
        }
        if words.first().is_some_and(|w| w.starts_with("mkfs")) {
            return Some("格式化文件系统");
        }
    }
    if lower.contains("dd ") && lower.contains("of=/dev/") {
        return Some("直接写块设备");
    }
    if lower.contains(":(){") || lower.contains(":() {") {
        return Some("fork 炸弹");
    }
    None
}

#[async_trait]
impl Tool for Shell {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, ctx: ToolCtx, args: Value) -> ToolOutput {
        let Some(command) = arg_str(&args, "command") else { return ToolOutput::err("缺少 command") };
        if let Some(why) = denied(command) {
            self.audit.record(json!({"ts": now_ms(), "session": ctx.session, "tool": "shell", "command": command, "denied": why})).await;
            return ToolOutput::err(format!("拒绝执行：{why}。这类操作需要阿颜亲自在服务器上处理。"));
        }
        let timeout = arg_u64(&args, "timeout_s").unwrap_or(self.cfg.timeout_s).clamp(1, self.cfg.max_timeout_s);
        let workdir: Option<PathBuf> = arg_str(&args, "workdir").map(PathBuf::from).or_else(|| self.cfg.workdir.clone());
        if let Some(d) = &workdir {
            let _ = tokio::fs::create_dir_all(d).await;
        }
        let effective = self.rewrite(command).await.unwrap_or_else(|| command.to_owned());
        let res = exec(
            &self.cfg.shell,
            &["-lc".to_owned(), effective.clone()],
            workdir.as_deref(),
            &[],
            Duration::from_secs(timeout),
            &ctx.cancel,
        )
        .await;
        let out = match res {
            Ok(o) => o,
            Err(e) => return ToolOutput::err(format!("无法启动 {}：{e}", self.cfg.shell)),
        };
        self.audit
            .record(json!({
                "ts": now_ms(), "session": ctx.session, "tool": "shell", "command": command,
                "rewritten": (effective != command).then_some(&effective),
                "exit": out.code, "timed_out": out.timed_out, "ms": out.elapsed.as_millis() as u64
            }))
            .await;
        let full = out.output.trim_end();
        let mut body = truncate_middle(full, self.cfg.output_limit);
        if full.chars().count() > self.cfg.output_limit
            && let Some(path) = self.spill(ctx.session, full).await
        {
            // 捕获本身有上限（每路 4 MB），超出时如实说明保存的并非全部
            let what = if out.truncated { "已捕获的前 4 MB 输出" } else { "完整输出" };
            body.push_str(&format!(
                "\n[{what}共 {} 行、{} 字，已保存到 {}；需要细看时用 fs_search 搜关键词或 fs_read 分段读取]",
                full.lines().count(),
                full.chars().count(),
                path.display()
            ));
        } else if out.truncated {
            body.push_str("\n[输出超过 4 MB，超出部分已丢弃]");
        }
        let mut text = String::new();
        if effective != command {
            text.push_str(&format!("[rtk] 实际执行：{effective}\n"));
        }
        text.push_str(if body.is_empty() { "（无输出）" } else { &body });
        if out.timed_out {
            return ToolOutput::err(format!("{text}\n[超时 {timeout} 秒，已终止整个进程组]"));
        }
        if out.cancelled {
            return ToolOutput::err(format!("{text}\n[已取消]"));
        }
        match out.code {
            Some(0) => ToolOutput::ok(text),
            Some(c) => ToolOutput::err(format!("{text}\n[退出码 {c}]")),
            None => ToolOutput::err(format!("{text}\n[进程被信号终止]")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::denied;

    #[test]
    fn denylist() {
        for bad in ["rm -rf /", "rm -rf /etc", "sudo rm -fr /var/lib/qya/", "cd /tmp && rm -r ~", "reboot", "mkfs.ext4 /dev/vda1", "dd if=/dev/zero of=/dev/vda", ":(){ :|:& };:"] {
            assert!(denied(bad).is_some(), "{bad}");
        }
        for ok in ["rm -rf /tmp/build", "rm /etc/foo.conf.bak", "ls -la /", "systemctl status nginx", "echo reboot"] {
            assert!(denied(ok).is_none(), "{ok}");
        }
    }
}
