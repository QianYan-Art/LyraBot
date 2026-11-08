//! memory：编辑长期记忆文件（条目以单独一行 § 分隔，与 Hermes 格式一致）。
//! 每个实例只开放若干目标文件：唐语歌是 user/memory，专员只有自己的那一份。

use crate::util::arg_str;
use async_trait::async_trait;
use qya_agent::{Tool, ToolCtx, ToolOutput};
use qya_core::ToolSpec;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::sync::Mutex;

pub const FILE_LIMIT: usize = 2000;
const SEP: &str = "\n§\n";

#[derive(Debug, Clone)]
pub struct MemoryTarget {
    pub name: String,
    pub file: PathBuf,
    pub about: String,
}

pub struct Memory {
    spec: ToolSpec,
    targets: Vec<MemoryTarget>,
    /// 串行化同一实例内的读改写
    lock: Mutex<()>,
}

impl Memory {
    pub fn new(targets: Vec<MemoryTarget>, note: &str) -> Self {
        let names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();
        let menu: Vec<String> = targets.iter().map(|t| format!("{}：{}", t.name, t.about)).collect();
        let spec = ToolSpec {
            name: "memory".into(),
            description: format!(
                "编辑长期记忆（{}）。只记稳定、以后还用得上的事实与偏好，不记一次性的内容、任务过程和隐私凭据。\
                 add 新增条目；replace 用 old 定位唯一条目后整条替换为 text；remove 删除含 old 的条目；list 查看。每个文件上限 {FILE_LIMIT} 字。{note}",
                menu.join("；")
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "enum": names},
                    "action": {"type": "string", "enum": ["add", "replace", "remove", "list"]},
                    "text": {"type": "string"},
                    "old": {"type": "string", "description": "replace/remove 时用来定位条目的原文片段"}
                },
                "required": ["target", "action"],
                "additionalProperties": false
            }),
        };
        Self { spec, targets, lock: Mutex::new(()) }
    }
}

fn entries(text: &str) -> Vec<String> {
    text.split(SEP).map(str::trim).filter(|e| !e.is_empty()).map(str::to_owned).collect()
}

/// 纯函数：对条目列表执行一次编辑，返回新列表与说明。
pub fn apply(mut list: Vec<String>, action: &str, text: Option<&str>, old: Option<&str>) -> Result<(Vec<String>, String), String> {
    let find = |list: &[String], old: &str| -> Result<usize, String> {
        let hits: Vec<usize> = list.iter().enumerate().filter(|(_, e)| e.contains(old)).map(|(i, _)| i).collect();
        match hits.as_slice() {
            [i] => Ok(*i),
            [] => Err(format!("没有包含“{old}”的条目")),
            _ => Err(format!("有 {} 条都包含“{old}”，请给更长的片段", hits.len())),
        }
    };
    let msg = match action {
        "add" => {
            let t = text.ok_or("add 需要 text")?.trim().replace(SEP, "\n");
            if list.contains(&t) {
                return Err("已有完全相同的条目".into());
            }
            list.push(t);
            "已新增".to_owned()
        }
        "replace" => {
            let i = find(&list, old.ok_or("replace 需要 old")?)?;
            list[i] = text.ok_or("replace 需要 text")?.trim().to_owned();
            "已替换".to_owned()
        }
        "remove" => {
            let i = find(&list, old.ok_or("remove 需要 old")?)?;
            list.remove(i);
            "已删除".to_owned()
        }
        o => return Err(format!("未知动作 {o}")),
    };
    let total: usize = list.iter().map(|e| e.chars().count()).sum();
    if total > FILE_LIMIT {
        return Err(format!("超过上限 {FILE_LIMIT} 字（编辑后 {total} 字），请先精简或删除旧条目"));
    }
    Ok((list, msg))
}

#[async_trait]
impl Tool for Memory {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        let Some(target) = arg_str(&args, "target").and_then(|n| self.targets.iter().find(|t| t.name == n)) else {
            return ToolOutput::err("target 无效");
        };
        let action = arg_str(&args, "action").unwrap_or("list");
        let _g = self.lock.lock().await;
        let current = entries(&tokio::fs::read_to_string(&target.file).await.unwrap_or_default());
        if action == "list" {
            if current.is_empty() {
                return ToolOutput::ok("（暂无条目）");
            }
            return ToolOutput::ok(current.iter().enumerate().map(|(i, e)| format!("{}. {e}", i + 1)).collect::<Vec<_>>().join("\n"));
        }
        match apply(current, action, arg_str(&args, "text"), arg_str(&args, "old")) {
            Ok((list, msg)) => {
                if let Some(dir) = target.file.parent() {
                    let _ = tokio::fs::create_dir_all(dir).await;
                }
                let body = if list.is_empty() { String::new() } else { format!("{}\n", list.join(SEP)) };
                match tokio::fs::write(&target.file, body).await {
                    Ok(()) => ToolOutput::ok(format!("{msg}（新会话或压缩后生效）")),
                    Err(e) => ToolOutput::err(format!("写入失败：{e}")),
                }
            }
            Err(e) => ToolOutput::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits() {
        let l = vec!["喜欢少糖奶茶".to_owned(), "用北京时间".to_owned()];
        let (l, _) = apply(l, "add", Some("周五晚上有组会"), None).unwrap();
        assert_eq!(l.len(), 3);
        let (l, _) = apply(l, "replace", Some("喜欢无糖奶茶"), Some("少糖")).unwrap();
        assert_eq!(l[0], "喜欢无糖奶茶");
        assert!(apply(l.clone(), "remove", None, Some("不存在")).is_err());
        assert!(apply(l.clone(), "add", Some("用北京时间"), None).is_err());
        let (l, _) = apply(l, "remove", None, Some("组会")).unwrap();
        assert_eq!(l.len(), 2);
        assert!(apply(l, "add", Some(&"长".repeat(FILE_LIMIT)), None).unwrap_err().contains("上限"));
    }
}
