//! 文件工具：读、写、精确替换、搜索。

use crate::util::{arg_bool, arg_str, arg_u64, exec, truncate_middle};
use async_trait::async_trait;
use lyrabot_agent::{Concurrency, Tool, ToolCtx, ToolOutput};
use lyrabot_core::ToolSpec;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

const READ_MAX_LINES: u64 = 2000;
const READ_MAX_CHARS: usize = 100_000;

fn spec(name: &str, description: &str, parameters: Value) -> ToolSpec {
    ToolSpec { name: name.into(), description: description.into(), parameters }
}

pub struct FsRead(ToolSpec);
pub struct FsWrite(ToolSpec);
pub struct FsPatch(ToolSpec);
pub struct FsSearch(ToolSpec);

impl Default for FsRead {
    fn default() -> Self {
        Self(spec(
            "fs_read",
            "读取文本文件，返回带行号的内容。大文件用 offset（起始行，从 1 开始）和 limit（行数）分段读。",
            json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false}),
        ))
    }
}

impl Default for FsWrite {
    fn default() -> Self {
        Self(spec(
            "fs_write",
            "把内容整体写入文件（覆盖），父目录不存在时自动创建。修改已有文件的局部内容用 fs_patch。",
            json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false}),
        ))
    }
}

impl Default for FsPatch {
    fn default() -> Self {
        Self(spec(
            "fs_patch",
            "精确替换文件中的一段文本：old 必须与文件内容逐字一致且只出现一次（replace_all=true 时替换全部）。",
            json!({"type":"object","properties":{"path":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["path","old","new"],"additionalProperties":false}),
        ))
    }
}

impl Default for FsSearch {
    fn default() -> Self {
        Self(spec(
            "fs_search",
            "在目录中按正则搜索文本内容（优先用 ripgrep），返回 文件:行号:内容，最多 200 行。glob 可限定文件，如 *.rs。",
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"glob":{"type":"string"}},"required":["pattern"],"additionalProperties":false}),
        ))
    }
}

#[async_trait]
impl Tool for FsRead {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        let Some(path) = arg_str(&args, "path") else { return ToolOutput::err("缺少 path") };
        let text = match tokio::fs::read(path).await {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => return ToolOutput::err(format!("读取 {path} 失败：{e}")),
        };
        let offset = arg_u64(&args, "offset").unwrap_or(1).max(1);
        let limit = arg_u64(&args, "limit").unwrap_or(READ_MAX_LINES).min(READ_MAX_LINES);
        let total = text.lines().count();
        let mut out = String::new();
        for (i, line) in text.lines().enumerate().skip(offset as usize - 1).take(limit as usize) {
            out.push_str(&format!("{:>6}\t{line}\n", i + 1));
            if out.len() > READ_MAX_CHARS {
                out.push_str("…（单次读取上限，缩小范围继续）\n");
                break;
            }
        }
        if out.is_empty() {
            return ToolOutput::ok(format!("（{path} 共 {total} 行，offset 之后没有内容）"));
        }
        ToolOutput::ok(format!("{path}（共 {total} 行）\n{out}"))
    }
}

#[async_trait]
impl Tool for FsWrite {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        let (Some(path), Some(content)) = (arg_str(&args, "path"), args.get("content").and_then(Value::as_str)) else {
            return ToolOutput::err("需要 path 和 content");
        };
        if let Some(dir) = Path::new(path).parent().filter(|d| !d.as_os_str().is_empty())
            && let Err(e) = tokio::fs::create_dir_all(dir).await
        {
            return ToolOutput::err(format!("创建目录失败：{e}"));
        }
        match tokio::fs::write(path, content).await {
            Ok(()) => ToolOutput::ok(format!("已写入 {path}（{} 字节）", content.len())),
            Err(e) => ToolOutput::err(format!("写入 {path} 失败：{e}")),
        }
    }
}

#[async_trait]
impl Tool for FsPatch {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        let (Some(path), Some(old), Some(new)) =
            (arg_str(&args, "path"), args.get("old").and_then(Value::as_str), args.get("new").and_then(Value::as_str))
        else {
            return ToolOutput::err("需要 path、old、new");
        };
        if old.is_empty() {
            return ToolOutput::err("old 不能为空");
        }
        let text = match tokio::fs::read_to_string(path).await {
            Ok(t) => t,
            Err(e) => return ToolOutput::err(format!("读取 {path} 失败：{e}")),
        };
        let n = text.matches(old).count();
        let all = arg_bool(&args, "replace_all");
        match (n, all) {
            (0, _) => ToolOutput::err("old 在文件中没有找到，请先 fs_read 确认原文"),
            (n, false) if n > 1 => ToolOutput::err(format!("old 出现了 {n} 次，请带上更多上下文使其唯一，或设 replace_all")),
            _ => {
                let updated = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
                match tokio::fs::write(path, updated).await {
                    Ok(()) => ToolOutput::ok(format!("已替换 {path} 中 {} 处", if all { n } else { 1 })),
                    Err(e) => ToolOutput::err(format!("写入 {path} 失败：{e}")),
                }
            }
        }
    }
}

#[async_trait]
impl Tool for FsSearch {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn call(&self, ctx: ToolCtx, args: Value) -> ToolOutput {
        let Some(pattern) = arg_str(&args, "pattern") else { return ToolOutput::err("缺少 pattern") };
        let path = arg_str(&args, "path").unwrap_or(".");
        let mut rg = vec!["-n".to_owned(), "--max-columns".into(), "300".into(), "--max-count".into(), "50".into()];
        if let Some(g) = arg_str(&args, "glob") {
            rg.push("--glob".into());
            rg.push(g.into());
        }
        rg.extend(["--".into(), pattern.into(), path.into()]);
        let res = match exec("rg", &rg, None, &[], Duration::from_secs(30), &ctx.cancel).await {
            Ok(o) => o,
            Err(_) => {
                // 没有 ripgrep 时退回 grep
                let g = vec!["-rnE".to_owned(), "--".into(), pattern.into(), path.into()];
                match exec("grep", &g, None, &[], Duration::from_secs(30), &ctx.cancel).await {
                    Ok(o) => o,
                    Err(e) => return ToolOutput::err(format!("rg 与 grep 都不可用：{e}")),
                }
            }
        };
        match res.code {
            Some(0) => {
                let lines: Vec<&str> = res.output.lines().take(200).collect();
                let more = res.output.lines().count().saturating_sub(200);
                let mut t = truncate_middle(&lines.join("\n"), 20_000);
                if more > 0 {
                    t.push_str(&format!("\n…（另有 {more} 行未显示，请缩小范围）"));
                }
                ToolOutput::ok(t)
            }
            Some(1) => ToolOutput::ok("没有匹配"),
            _ => ToolOutput::err(format!("搜索失败：{}", truncate_middle(&res.output, 2000))),
        }
    }
}
