//! /provider 命令：查看与修改可插拔提供方，写回配置文件（toml_edit 保留注释与格式）并热加载。
//! 写回失败或新配置校验失败时恢复原文件，运行中的配置不变。

use qya_config::ConfigHandle;
use std::fmt::Write as _;
use toml_edit::{DocumentMut, Item, value};

/// 列出各类别激活的提供方与其字段（`*_file` 只显示路径，不读取内容）。
pub fn list(cfg: &ConfigHandle) -> String {
    let c = cfg.get();
    if c.plugins.categories.is_empty() {
        return "还没有配置任何提供方".into();
    }
    let mut s = String::new();
    for (cat, pc) in &c.plugins.categories {
        let _ = writeln!(s, "【{cat}】当前 {}\n可选：{}", pc.active, pc.providers.keys().cloned().collect::<Vec<_>>().join("、"));
        if let Some(t) = pc.providers.get(&pc.active) {
            for (k, v) in t {
                let shown = match v {
                    toml::Value::String(x) => x.clone(),
                    other => other.to_string(),
                };
                let _ = writeln!(s, "  {k} = {shown}");
            }
        }
        s.push('\n');
    }
    s.push_str("修改：/provider 类别 字段 值\n切换：/provider 类别 use 名字");
    s
}

/// 解析值：整数、布尔按类型写入，其余按字符串。
fn parse_value(raw: &str) -> toml_edit::Value {
    if let Ok(i) = raw.parse::<i64>() {
        return i.into();
    }
    match raw {
        "true" => true.into(),
        "false" => false.into(),
        _ => raw.into(),
    }
}

/// 在文档上执行修改（纯函数，便于测试）。
pub fn edit(doc: &mut DocumentMut, category: &str, field: &str, raw: &str) -> Result<String, String> {
    if field.ends_with("_file") || field == "kind" {
        return Err(format!("{field} 不允许在聊天里修改，请直接改配置文件"));
    }
    let cat = doc
        .get_mut("plugins")
        .and_then(|p| p.get_mut(category))
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| format!("没有 {category} 这个类别"))?;
    if field == "use" {
        let exists = cat.get("providers").and_then(|p| p.get(raw)).is_some();
        if !exists {
            return Err(format!("{category} 下没有名为 {raw} 的提供方"));
        }
        cat.insert("active", value(raw));
        return Ok(format!("{category} 已切换到 {raw}"));
    }
    let active = cat.get("active").and_then(Item::as_str).ok_or("缺少 active")?.to_owned();
    let t = cat
        .get_mut("providers")
        .and_then(|p| p.get_mut(&active))
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| format!("找不到提供方 {active}"))?;
    let old = t.get(field).map(|v| v.to_string().trim().to_owned());
    t.insert(field, Item::Value(parse_value(raw)));
    Ok(match old {
        Some(o) => format!("{category}/{active} 的 {field}：{o} → {raw}"),
        None => format!("{category}/{active} 新增 {field} = {raw}"),
    })
}

/// 修改配置文件并热加载；任何一步失败都恢复原文件。
pub fn apply(cfg: &ConfigHandle, category: &str, field: &str, raw: &str) -> Result<String, String> {
    let path = cfg.path().to_owned();
    let original = std::fs::read_to_string(&path).map_err(|e| format!("读取配置失败：{e}"))?;
    let mut doc: DocumentMut = original.parse().map_err(|e| format!("配置文件解析失败：{e}"))?;
    let msg = edit(&mut doc, category, field, raw)?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string()).map_err(|e| format!("写入失败：{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("替换配置失败：{e}"))?;
    if let Err(e) = cfg.reload() {
        let _ = std::fs::write(&path, &original);
        return Err(format!("新配置无效，已恢复原配置：{e}"));
    }
    Ok(format!("{msg}（已写回配置并生效）"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
# 生图
[plugins.image]
active = "a"   # 当前
[plugins.image.providers.a]
kind = "openai_images"
model = "gpt-image-2"
key_file = "k"
[plugins.image.providers.b]
kind = "openai_images"
model = "m2"
"#;

    #[test]
    fn edits_keep_comments() {
        let mut doc: DocumentMut = SRC.parse().unwrap();
        let m = edit(&mut doc, "image", "model", "gpt-image-2.5-flare").unwrap();
        assert!(m.contains("\"gpt-image-2\" → gpt-image-2.5-flare"), "{m}");
        edit(&mut doc, "image", "timeout_s", "240").unwrap();
        let out = doc.to_string();
        assert!(out.contains("# 生图") && out.contains("# 当前") && out.contains("timeout_s = 240"));
        assert!(edit(&mut doc, "image", "key_file", "x").is_err());
        assert!(edit(&mut doc, "image", "use", "zzz").is_err());
        edit(&mut doc, "image", "use", "b").unwrap();
        assert!(doc.to_string().contains("active = \"b\""));
        assert!(edit(&mut doc, "nope", "model", "x").is_err());
    }
}
