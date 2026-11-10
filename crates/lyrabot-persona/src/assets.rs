//! 人格资产装载：persona.toml 清单 + 角色卡（chara_card_v2 子集）+ 各 Markdown 资料。

use crate::PersonaError;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    user_name: String,
    #[serde(default)]
    card: Option<PathBuf>,
    #[serde(default)]
    core: Vec<CoreRef>,
    #[serde(default)]
    runtime_relationship: Option<PathBuf>,
    #[serde(default)]
    runtime_agency: Option<PathBuf>,
    #[serde(default)]
    notes: Vec<NoteRef>,
}

#[derive(Debug, Deserialize)]
struct CoreRef {
    title: String,
    file: PathBuf,
}

#[derive(Debug, Deserialize)]
struct NoteRef {
    id: String,
    file: PathBuf,
    #[serde(rename = "use")]
    usage: String,
}

/// 角色卡中参与渲染的字段。
#[derive(Debug, Deserialize)]
pub struct CharacterCard {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub personality: String,
    #[serde(default)]
    pub mes_example: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub character_book: Option<CharacterBook>,
}

#[derive(Debug, Deserialize)]
pub struct CharacterBook {
    #[serde(default)]
    pub entries: Vec<BookEntry>,
}

#[derive(Debug, Deserialize)]
pub struct BookEntry {
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub constant: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct Section {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct Note {
    pub id: String,
    pub usage: String,
    pub body: String,
}

/// 装载完成、可直接渲染的人格资产。
#[derive(Debug)]
pub struct Assets {
    pub user_name: String,
    pub card: CharacterCard,
    pub core: Vec<Section>,
    pub relationship: String,
    pub agency: String,
    pub notes: Vec<Note>,
}

impl Assets {
    pub fn load(dir: &Path) -> Result<Self, PersonaError> {
        let manifest: Manifest = toml::from_str(&read(&dir.join("persona.toml"))?)
            .map_err(|e| PersonaError::Invalid(format!("persona.toml：{e}")))?;
        let card_file = manifest.card.as_deref().unwrap_or_else(|| Path::new("card.json"));
        let raw_card = read(&dir.join(card_file))?;
        let v: serde_json::Value = serde_json::from_str(&raw_card)
            .map_err(|e| PersonaError::Invalid(format!("{}：{e}", card_file.display())))?;
        let target = if v.get("name").and_then(|n| n.as_str()).is_some_and(|s| !s.trim().is_empty()) {
            v
        } else if let Some(data) = v.get("data").filter(|d| d.is_object()) {
            data.clone()
        } else {
            v
        };
        let card: CharacterCard = serde_json::from_value(target)
            .map_err(|e| PersonaError::Invalid(format!("{}：{e}", card_file.display())))?;
        if card.name.trim().is_empty() {
            return Err(PersonaError::Invalid(format!("{} 缺少 name", card_file.display())));
        }
        let core = manifest
            .core
            .iter()
            .map(|c| Ok(Section { title: c.title.clone(), body: body_of(&read(&dir.join(&c.file))?) }))
            .collect::<Result<Vec<_>, PersonaError>>()?;
        let mut notes = Vec::new();
        for n in &manifest.notes {
            if notes.iter().any(|x: &Note| x.id == n.id) {
                return Err(PersonaError::Invalid(format!("notes 条目 id 重复：{}", n.id)));
            }
            let usage = n
                .usage
                .replace("{{user}}", &manifest.user_name)
                .replace("{{char}}", &card.name);
            let body = body_of(&read(&dir.join(&n.file))?)
                .replace("{{user}}", &manifest.user_name)
                .replace("{{char}}", &card.name);
            notes.push(Note { id: n.id.clone(), usage, body });
        }
        let relationship = match &manifest.runtime_relationship {
            Some(p) => read(&dir.join(p))?.trim().to_owned(),
            None => String::new(),
        };
        let agency = match &manifest.runtime_agency {
            Some(p) => read(&dir.join(p))?.trim().to_owned(),
            None => String::new(),
        };
        Ok(Self {
            user_name: manifest.user_name,
            relationship,
            agency,
            card,
            core,
            notes,
        })
    }
}

fn read(p: &Path) -> Result<String, PersonaError> {
    std::fs::read_to_string(p).map_err(|source| PersonaError::Io { path: p.display().to_string(), source })
}

/// 去掉资料文件自带的一级标题，并把其余标题降一级，便于嵌入 S1 的二级小节下。
pub(crate) fn body_of(md: &str) -> String {
    let mut out = Vec::new();
    for (i, line) in md.trim().lines().enumerate() {
        if i == 0 && line.starts_with("# ") {
            continue;
        }
        if line.starts_with('#') {
            out.push(format!("#{line}"));
        } else {
            out.push(line.to_owned());
        }
    }
    out.join("\n").trim().to_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn body_strips_title_and_demotes() {
        assert_eq!(super::body_of("# 标题\n\n## 小节\n内容\n"), "### 小节\n内容");
        assert_eq!(super::body_of("正文\n## 小节"), "正文\n### 小节");
    }
}
