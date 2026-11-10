//! 本地媒体内联：会话里以 file:// 引用本地图片与视频（避免 base64 进数据库），请求时才读文件转为 data URI。

/// 内联到请求里的本地媒体上限：图片 10 MB，视频 20 MB（更大的视频由 watch_video 先压缩）。
pub const IMAGE_INLINE_MAX: u64 = 10 * 1024 * 1024;
pub const VIDEO_INLINE_MAX: u64 = 20 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Inline {
    Missing,
    TooLarge,
}

/// 本地媒体（会话里以 file:// 引用，避免 base64 进数据库）在请求时转为 data URI。
pub fn local_data_uri(path: &str, max: u64) -> std::result::Result<String, Inline> {
    use base64::Engine;
    let meta = std::fs::metadata(path).map_err(|_| Inline::Missing)?;
    if meta.len() > max {
        return Err(Inline::TooLarge);
    }
    let bytes = std::fs::read(path).map_err(|_| Inline::Missing)?;
    Ok(format!("data:{};base64,{}", sniff_mime(path, &bytes), base64::engine::general_purpose::STANDARD.encode(&bytes)))
}

fn sniff_mime(path: &str, bytes: &[u8]) -> &'static str {
    match bytes.get(..12) {
        Some([0x89, b'P', b'N', b'G', ..]) => return "image/png",
        Some([0xFF, 0xD8, ..]) => return "image/jpeg",
        Some([b'G', b'I', b'F', ..]) => return "image/gif",
        Some([b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P']) => return "image/webp",
        Some([_, _, _, _, b'f', b't', b'y', b'p', b'q', b't', ..]) => return "video/quicktime",
        Some([_, _, _, _, b'f', b't', b'y', b'p', ..]) => return "video/mp4",
        Some([0x1A, 0x45, 0xDF, 0xA3, ..]) => return "video/webm",
        _ => {}
    }
    match path.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm" | "mkv") => "video/webm",
        Some("png") => "image/png",
        _ => "image/jpeg",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_limits_and_mime() {
        let dir = std::env::temp_dir().join(format!("lyrabot-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mp4 = dir.join("a.mp4");
        std::fs::write(&mp4, b"\0\0\0\x18ftypisom0000").unwrap();
        let p = mp4.to_str().unwrap();
        assert!(local_data_uri(p, 1024).unwrap().starts_with("data:video/mp4;base64,"));
        assert_eq!(local_data_uri(p, 4), Err(Inline::TooLarge));
        assert_eq!(local_data_uri(dir.join("gone.mp4").to_str().unwrap(), 1024), Err(Inline::Missing));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
