//! skill_view：按需读取 SKILL.md 技能（兼容 Hermes / agentskills 的 frontmatter 格式）。
//! 技能清单（名字 + 一句话描述）写在工具说明里，正文只在调用时读取。

use crate::util::arg_str;
use async_trait::async_trait;
use lyrabot_agent::{Concurrency, Tool, ToolCtx, ToolOutput};
use lyrabot_core::ToolSpec;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

/// 解析 frontmatter 中的 name / description（支持 `description: |` 多行块）。
pub fn parse_frontmatter(md: &str) -> Option<(String, String)> {
    let body = md.strip_prefix("---")?;
    let end = body.find("\n---")?;
    let mut name = None;
    let mut desc = String::new();
    let mut in_block = false;
    for line in body[..end].lines() {
        if in_block {
            if line.starts_with(' ') || line.starts_with('\t') || line.trim().is_empty() {
                desc.push_str(line.trim());
                desc.push(' ');
                continue;
            }
            in_block = false;
        }
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().trim_matches('"').to_owned());
        } else if let Some(v) = line.strip_prefix("description:") {
            let v = v.trim();
            if v == "|" || v == ">" || v == "|-" || v == ">-" {
                in_block = true;
            } else {
                desc = v.trim_matches('"').to_owned();
            }
        }
    }
    Some((name?, desc.trim().to_owned()))
}

pub fn scan(dir: &Path) -> Vec<Skill> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<Skill> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let p = e.path();
            let md = std::fs::read_to_string(p.join("SKILL.md")).ok()?;
            let (name, description) = parse_frontmatter(&md)?;
            Some(Skill { name, description, dir: p })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub struct SkillView {
    spec: ToolSpec,
    skills: Vec<Skill>,
}

impl SkillView {
    /// 目录里没有技能时返回 `None`。
    pub fn new(dir: &Path) -> Option<Self> {
        let skills = scan(dir);
        if skills.is_empty() {
            return None;
        }
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        let menu: Vec<String> = skills
            .iter()
            .map(|s| format!("{}：{}", s.name, s.description.chars().take(160).collect::<String>()))
            .collect();
        let spec = ToolSpec {
            name: "skill_view".into(),
            description: format!(
                "读取技能说明（任务流程与规则）。先读 SKILL.md，需要时再用 file 读技能目录里的其他文件。可用技能——{}",
                menu.join("；")
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "enum": names},
                    "file": {"type": "string", "description": "技能目录内的相对路径，默认 SKILL.md"}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        };
        Some(Self { spec, skills })
    }
}

#[async_trait]
impl Tool for SkillView {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        let Some(skill) = arg_str(&args, "name").and_then(|n| self.skills.iter().find(|s| s.name == n)) else {
            return ToolOutput::err("没有这个技能");
        };
        let rel = Path::new(arg_str(&args, "file").unwrap_or("SKILL.md"));
        if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return ToolOutput::err("file 只能是技能目录内的相对路径");
        }
        match tokio::fs::read_to_string(skill.dir.join(rel)).await {
            Ok(t) => ToolOutput::ok(crate::util::truncate_middle(&t, 30_000)),
            Err(e) => ToolOutput::err(format!("读取失败：{e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_variants() {
        let a = "---\nname: grill-me\ndescription: Interview the user.\n---\nbody";
        assert_eq!(parse_frontmatter(a), Some(("grill-me".into(), "Interview the user.".into())));
        let b = "---\nname: grok-search\ndescription: |\n  Enhanced search.\n  Second line.\nallowed-tools: Bash\n---\n";
        assert_eq!(parse_frontmatter(b), Some(("grok-search".into(), "Enhanced search. Second line.".into())));
        assert_eq!(parse_frontmatter("no frontmatter"), None);
    }
}
