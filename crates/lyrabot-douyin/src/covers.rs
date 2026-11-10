//! 封面相关：服务端预览校验与落盘、二维码落盘、固定制作目录的本地素材枚举。

use crate::sanitize::{is_release_id, is_sha256};
use base64::Engine;
use lyrabot_core::media::image_info;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const MAX_PREVIEW_BYTES: usize = 32 * 1024;
const MAX_PIXELS: u64 = 16_000_000;
pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_QR_BYTES: usize = 2 * 1024 * 1024;
const COVER_SOURCES: &[&str] = &["qq_image", "video_frame", "library_image", "saved_image", "paired_images"];
/// 固定制作目录的文件变体：（文件名后缀, 期望比例）
pub const SAVED_VARIANTS: &[(&str, &str)] = &[("9x16", "9:16"), ("16x9", "16:9"), ("4x3", "4:3"), ("3x4", "3:4")];
const SAVED_EXTS: &[(&str, &str)] = &[("png", "png"), ("jpg", "jpg"), ("jpeg", "jpg"), ("webp", "webp")];
const ASPECTS: &[(&str, u32, u32)] = &[("9:16", 9, 16), ("16:9", 16, 9), ("4:3", 4, 3), ("3:4", 3, 4)];

fn preview_size(format: &str) -> (u32, u32) {
    if format == "landscape" { (320, 240) } else { (240, 320) }
}

/// 写入缓存目录（原子替换），按内容哈希命名。
fn save(dir: &Path, name: &str, data: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    let tmp = dir.join(format!(".{name}.part"));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// 校验并取出一组封面的双槽预览；返回去掉 base64 后的封面描述与预览文件路径。
pub fn decode_covers(covers: &Value, expected_source: Option<&str>, cache: &Path) -> Result<(Value, Vec<PathBuf>), String> {
    let list = covers.as_array().filter(|a| a.len() <= 5).ok_or("covers 响应无效")?;
    let mut clean = Vec::new();
    let mut files = Vec::new();
    for c in list {
        let o = c.as_object().ok_or("cover 响应无效")?;
        let allowed = ["release_id", "cover_id", "cover_sha256", "video_sha256", "source", "time", "previews"];
        if o.keys().any(|k| !allowed.contains(&k.as_str()))
            || !o.get("release_id").and_then(Value::as_str).is_some_and(is_release_id)
            || ["cover_id", "cover_sha256", "video_sha256"].iter().any(|f| !o.get(*f).and_then(Value::as_str).is_some_and(is_sha256))
            || !o.get("source").and_then(Value::as_str).is_some_and(|s| COVER_SOURCES.contains(&s) && expected_source.is_none_or(|e| e == s))
            || o.get("time").is_some_and(|t| !t.as_f64().is_some_and(|t| t.is_finite() && t >= 0.0))
        {
            return Err("cover 响应无效".into());
        }
        let previews = o.get("previews").and_then(Value::as_array).filter(|p| p.len() == 2).ok_or("预览响应无效")?;
        let mut seen = Vec::new();
        for p in previews {
            let po = p.as_object().filter(|m| m.len() == 2).ok_or("JPEG 预览响应无效")?;
            let format = po.get("format").and_then(Value::as_str).filter(|f| matches!(*f, "landscape" | "portrait") && !seen.contains(f)).ok_or("JPEG 预览响应无效")?;
            let b64 = po.get("jpeg_base64").and_then(Value::as_str).filter(|s| s.len() <= MAX_PREVIEW_BYTES * 4 / 3 + 4).ok_or("JPEG 预览响应无效")?;
            let data = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|_| "JPEG 预览编码无效")?;
            if data.is_empty() || data.len() > MAX_PREVIEW_BYTES || !data.starts_with(&[0xFF, 0xD8, 0xFF]) || !data.ends_with(&[0xFF, 0xD9]) {
                return Err("JPEG 预览格式或大小无效".into());
            }
            match image_info(&data) {
                Some(("jpg", w, h)) if (w, h) == preview_size(format) => {}
                _ => return Err("JPEG 预览尺寸无效".into()),
            }
            seen.push(format);
            let name = format!("{}.jpg", hex::encode(Sha256::digest(&data)));
            files.push(save(cache, &name, &data).map_err(|e| format!("保存预览失败：{e}"))?);
        }
        let mut cc = o.clone();
        cc.insert("previews".into(), json!(seen.iter().map(|f| json!({"format": f})).collect::<Vec<_>>()));
        clean.push(Value::Object(cc));
    }
    Ok((Value::Array(clean), files))
}

/// 登录二维码：校验 PNG 后落盘。
pub fn save_qr(b64: &str, cache: &Path) -> Result<PathBuf, String> {
    if b64.len() > MAX_QR_BYTES * 4 / 3 + 4 {
        return Err("二维码过大".into());
    }
    let data = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|_| "二维码编码无效")?;
    if data.len() > MAX_QR_BYTES || !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("二维码格式无效".into());
    }
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    save(cache, &format!("qr-{ts}.png"), &data).map_err(|e| format!("保存二维码失败：{e}"))
}

/// 拒绝符号链接与非普通文件；读取前后比对身份，防止读取过程中被替换。
fn read_regular(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    let before = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !before.file_type().is_file() || before.len() > MAX_IMAGE_BYTES {
        return Err(std::io::Error::other("制作素材必须是不超过 8 MB 的普通文件"));
    }
    let data = std::fs::read(path)?;
    let after = std::fs::symlink_metadata(path)?;
    if !after.file_type().is_file() || after.len() != before.len() || after.modified().ok() != before.modified().ok() || data.len() as u64 != after.len() {
        return Err(std::io::Error::other("制作素材读取期间发生变化"));
    }
    Ok(Some(data))
}

fn check_dir(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_dir() => Ok(true),
        Ok(_) => Err(std::io::Error::other("制作素材目录不安全（符号链接或不是目录）")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// 枚举某作品固定制作目录中的规范封面素材。返回（目录是否已交付, [(描述, 原始字节)]）。
/// image_id = sha256(release_id \0 文件名 \0 内容哈希) 前 32 位，内容变化即失效。
/// 一份本地素材：描述 + 原始字节。
pub type SavedSource = (Value, Vec<u8>);

pub fn saved_sources(root: &Path, release_id: &str) -> Result<(bool, Vec<SavedSource>), String> {
    if !is_release_id(release_id) {
        return Err("作品编号无效".into());
    }
    if !check_dir(root).map_err(|e| e.to_string())? {
        return Ok((false, Vec::new()));
    }
    let dir = root.join(release_id);
    if !check_dir(&dir).map_err(|e| e.to_string())? {
        return Ok((true, Vec::new()));
    }
    let mut out = Vec::new();
    for (variant, expected) in SAVED_VARIANTS {
        for (ext, fmt) in SAVED_EXTS {
            let filename = format!("{release_id}_{variant}.{ext}");
            let Some(data) = read_regular(&dir.join(&filename)).map_err(|e| e.to_string())? else { continue };
            let (got, w, h) = image_info(&data).ok_or("制作素材无法识别")?;
            let aspect = ASPECTS.iter().find(|(_, aw, ah)| w as u64 * *ah as u64 == h as u64 * *aw as u64).map(|a| a.0);
            if got != *fmt || w == 0 || h == 0 || w as u64 * h as u64 > MAX_PIXELS || aspect.is_none() {
                return Err(format!("制作素材 {filename} 的格式、尺寸或比例无效"));
            }
            let content = hex::encode(Sha256::digest(&data));
            let id = hex::encode(Sha256::digest(format!("owner\0{release_id}\0{filename}\0{content}").as_bytes()))[..32].to_owned();
            out.push((
                json!({"image_id": id, "filename": filename, "release_id": release_id, "width": w, "height": h, "aspect": aspect, "expected_aspect": expected}),
                data,
            ));
        }
    }
    Ok((true, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        v.extend(w.to_be_bytes());
        v.extend(h.to_be_bytes());
        v
    }

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
        v.extend(h.to_be_bytes());
        v.extend(w.to_be_bytes());
        v.extend([0u8; 12]);
        v.extend([0xFF, 0xD9]);
        v
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lyrabot-dy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn saved_cover_enumeration() {
        let root = tmp("saved");
        assert!(!saved_sources(&root.join("missing"), "ep02").unwrap().0);
        assert!(saved_sources(&root, "ep02").unwrap().1.is_empty());
        std::fs::create_dir_all(root.join("ep02")).unwrap();
        std::fs::write(root.join("ep02/ep02_4x3.png"), png(1440, 1080)).unwrap();
        std::fs::write(root.join("ep02/ep02_3x4.png"), png(1080, 1440)).unwrap();
        std::fs::write(root.join("ep02/other.png"), png(10, 10)).unwrap();
        let (delivered, recs) = saved_sources(&root, "ep02").unwrap();
        assert!(delivered);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].0["aspect"], "4:3");
        assert_eq!(recs[0].0["image_id"].as_str().unwrap().len(), 32);
        std::fs::write(root.join("ep02/ep02_9x16.png"), png(100, 100)).unwrap();
        assert!(saved_sources(&root, "ep02").is_err(), "比例不在允许范围内应报错");
        assert!(saved_sources(&root, "../x").is_err());
    }

    #[test]
    fn cover_previews_validated_and_saved() {
        let cache = tmp("cache");
        let b = |d: Vec<u8>| base64::engine::general_purpose::STANDARD.encode(d);
        let cover = json!([{
            "release_id": "ep02", "cover_id": "a".repeat(64), "cover_sha256": "b".repeat(64), "video_sha256": "c".repeat(64),
            "source": "paired_images",
            "previews": [{"format": "landscape", "jpeg_base64": b(jpeg(320, 240))}, {"format": "portrait", "jpeg_base64": b(jpeg(240, 320))}]
        }]);
        let (clean, files) = decode_covers(&cover, Some("paired_images"), &cache).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(clean[0]["previews"], json!([{"format": "landscape"}, {"format": "portrait"}]));
        assert!(decode_covers(&cover, Some("qq_image"), &cache).is_err(), "来源不符");
        let mut bad = cover.clone();
        bad[0]["previews"][0]["jpeg_base64"] = json!(b(jpeg(100, 100)));
        assert!(decode_covers(&bad, None, &cache).is_err(), "预览尺寸不符");
    }
}
