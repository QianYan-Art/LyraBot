//! 媒体格式探测（纯函数，供插件与外部工具共用）。

/// 图片格式与尺寸（PNG/JPEG/WebP/GIF 头部解析，不引入解码库）。
pub fn image_info(b: &[u8]) -> Option<(&'static str, u32, u32)> {
    if b.len() >= 24 && b.starts_with(b"\x89PNG\r\n\x1a\n") {
        let w = u32::from_be_bytes(b[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(b[20..24].try_into().ok()?);
        return Some(("png", w, h));
    }
    if b.len() >= 10 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        return Some(("gif", u16::from_le_bytes([b[6], b[7]]) as u32, u16::from_le_bytes([b[8], b[9]]) as u32));
    }
    if b.len() >= 30 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        return match &b[12..16] {
            b"VP8X" => Some(("webp", 1 + u32::from_le_bytes([b[24], b[25], b[26], 0]), 1 + u32::from_le_bytes([b[27], b[28], b[29], 0]))),
            b"VP8 " => Some(("webp", (u16::from_le_bytes([b[26], b[27]]) & 0x3fff) as u32, (u16::from_le_bytes([b[28], b[29]]) & 0x3fff) as u32)),
            _ => Some(("webp", 0, 0)),
        };
    }
    if b.len() >= 4 && b[0] == 0xFF && b[1] == 0xD8 {
        // 逐段寻找 SOF0~SOF15（排除 DHT/JPG/DAC）
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = b[i + 1];
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
                let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
                return Some(("jpg", w, h));
            }
            i += 2 + len;
        }
        return Some(("jpg", 0, 0));
    }
    None
}
