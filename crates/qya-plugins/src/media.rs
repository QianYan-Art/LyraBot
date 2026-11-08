//! 媒体落盘与格式探测。

use std::path::{Path, PathBuf};

/// 在媒体目录下生成带时间戳的文件名并写入。
pub async fn save(dir: &Path, prefix: &str, ext: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let path = dir.join(format!("{prefix}-{ts}.{ext}"));
    let tmp = dir.join(format!(".{prefix}-{ts}.{ext}.part"));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(path)
}

pub use qya_core::media::image_info;

/// 校验 WAV：RIFF/WAVE 头 + 找到 data 块且长度与实际一致（防止空音频或截断）。
pub fn wav_ok(b: &[u8]) -> Result<(), &'static str> {
    if b.len() < 44 || !b.starts_with(b"RIFF") || &b[8..12] != b"WAVE" {
        return Err("不是有效的 WAV 音频");
    }
    let mut i = 12;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let size = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap_or_default()) as usize;
        if id == b"data" {
            return match size {
                0 => Err("WAV 音频为空"),
                n if i + 8 + n > b.len() => Err("WAV 音频已截断"),
                _ => Ok(()),
            };
        }
        i += 8 + size + (size & 1);
    }
    Err("WAV 缺少 data 块")
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn wav(samples: usize) -> Vec<u8> {
        let data = vec![0u8; samples * 2];
        let mut v = b"RIFF".to_vec();
        v.extend(((36 + data.len()) as u32).to_le_bytes());
        v.extend(b"WAVEfmt ");
        v.extend(16u32.to_le_bytes());
        v.extend([1, 0, 1, 0]);
        v.extend(16000u32.to_le_bytes());
        v.extend(32000u32.to_le_bytes());
        v.extend([2, 0, 16, 0]);
        v.extend(b"data");
        v.extend((data.len() as u32).to_le_bytes());
        v.extend(data);
        v
    }

    #[test]
    fn wav_validation() {
        assert!(wav_ok(&wav(100)).is_ok());
        assert_eq!(wav_ok(&wav(0)), Err("WAV 音频为空"));
        let mut cut = wav(100);
        cut.truncate(100);
        assert_eq!(wav_ok(&cut), Err("WAV 音频已截断"));
        assert!(wav_ok(b"not audio at all, definitely not riff wave format.").is_err());
    }

    #[test]
    fn png_size() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(1536u32.to_be_bytes());
        png.extend(1024u32.to_be_bytes());
        assert_eq!(image_info(&png), Some(("png", 1536, 1024)));
        assert_eq!(image_info(b"hello"), None);
    }
}
