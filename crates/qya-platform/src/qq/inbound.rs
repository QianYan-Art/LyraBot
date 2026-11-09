//! 入站 C2C 消息解析：文本、图片与视频（落盘后以 file:// 引用，按目标模型能力在请求时内联或改为提示）、
//! 语音（QQ 自带 asr_refer_text）、文件（落盘并在文字中说明）。

pub use super::api::Download;
use super::api::DOWNLOAD_MAX_BYTES;
use qya_core::ContentPart;
use serde_json::Value;
use std::path::Path;

/// 一条入站消息中需要下载的附件。
#[derive(Debug, Clone, PartialEq)]
pub enum Attachment {
    Image { url: String, ext: String },
    Voice { asr: Option<String> },
    Video { url: String, name: String },
    File { url: String, name: String },
}

/// 解析结果：发送者、消息 id、文本、附件。
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub openid: String,
    pub msg_id: String,
    pub text: String,
    pub attachments: Vec<Attachment>,
}

fn fix_url(u: &str) -> String {
    if let Some(rest) = u.strip_prefix("//") { format!("https://{rest}") } else { u.to_owned() }
}

fn is_voice(ct: &str, name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ct.starts_with("audio/") || ct == "voice" || [".silk", ".amr", ".slk", ".wav", ".mp3", ".ogg", ".m4a"].iter().any(|e| n.ends_with(e))
}

pub fn parse_c2c(d: &Value) -> Option<Parsed> {
    let openid = d.pointer("/author/user_openid").and_then(Value::as_str)?.to_owned();
    let msg_id = d.get("id").and_then(Value::as_str)?.to_owned();
    let text = d.get("content").and_then(Value::as_str).unwrap_or_default().trim().to_owned();
    let mut attachments = Vec::new();
    for a in d.get("attachments").and_then(Value::as_array).into_iter().flatten() {
        let ct = a.get("content_type").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
        let url = fix_url(a.get("url").and_then(Value::as_str).unwrap_or_default());
        let name = a.get("filename").and_then(Value::as_str).unwrap_or_default().to_owned();
        if is_voice(&ct, &name) {
            let asr = a.get("asr_refer_text").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
            attachments.push(Attachment::Voice { asr });
        } else if url.is_empty() {
            continue;
        } else if ct.starts_with("image/") {
            let ext = ct.trim_start_matches("image/").replace("jpeg", "jpg");
            attachments.push(Attachment::Image { url, ext: if ext.is_empty() { "jpg".into() } else { ext } });
        } else if ct.starts_with("video/") {
            attachments.push(Attachment::Video { url, name: if name.is_empty() { "video.mp4".into() } else { name } });
        } else {
            attachments.push(Attachment::File { url, name: if name.is_empty() { "file".into() } else { name } });
        }
    }
    Some(Parsed { openid, msg_id, text, attachments })
}

/// 把解析结果（附件已下载为本地路径）转成模型可读的内容片段。
/// `saved[i]` 对应第 i 个附件的下载结果。
pub fn to_parts(p: &Parsed, saved: &[Download]) -> Vec<ContentPart> {
    let mut notes = Vec::new();
    let mut media = Vec::new();
    let mb = |b: u64| b as f64 / 1048576.0;
    let cap_mb = DOWNLOAD_MAX_BYTES / 1048576;
    for (a, dl) in p.attachments.iter().zip(saved) {
        match (a, dl) {
            (Attachment::Voice { asr: Some(t) }, _) => notes.push(format!("〔语音转文字〕{t}")),
            (Attachment::Voice { asr: None }, _) => notes.push("〔对方发了一段语音，但没有拿到识别文字〕".to_owned()),
            (Attachment::Image { .. }, Download::Saved(path)) => media.push(ContentPart::Image { url: file_url(path) }),
            (Attachment::Image { .. }, Download::TooLarge(b)) => {
                notes.push(format!("〔一张图片（{:.1} MB）超过 {cap_mb} MB 下载上限，未保存〕", mb(*b)));
            }
            (Attachment::Image { .. }, Download::Failed) => notes.push("〔一张图片下载失败〕".to_owned()),
            (Attachment::Video { name, .. }, Download::Saved(path)) => {
                notes.push(format!("〔对方发来视频 {name}，已保存到 {}〕", path.display()));
                media.push(ContentPart::Video { url: file_url(path) });
            }
            (Attachment::File { name, .. }, Download::Saved(path)) => notes.push(format!("〔对方发来文件 {name}，已保存到 {}〕", path.display())),
            (Attachment::Video { name, .. } | Attachment::File { name, .. }, Download::TooLarge(b)) => {
                notes.push(format!("〔附件 {name}（{:.1} MB）超过 {cap_mb} MB 下载上限，未保存〕", mb(*b)));
            }
            (Attachment::Video { name, .. } | Attachment::File { name, .. }, Download::Failed) => notes.push(format!("〔附件 {name} 下载失败〕")),
        }
    }
    let mut text = p.text.clone();
    for n in notes {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&n);
    }
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(ContentPart::text(text));
    }
    parts.extend(media);
    parts
}

pub fn file_url(p: &Path) -> String {
    format!("file://{}", p.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn sample() -> Value {
        json!({
            "id": "ROBOT1.0_abc",
            "content": " 看看这个 ",
            "author": {"user_openid": "OPENID1"},
            "attachments": [
                {"content_type": "image/jpeg", "url": "//multimedia.nt.qq.com.cn/x.jpg", "filename": "x.jpg"},
                {"content_type": "voice", "url": "https://v/1.amr", "filename": "1.amr", "asr_refer_text": "晚上吃什么"},
                {"content_type": "video/mp4", "url": "https://v/2.mp4", "filename": "猫.mp4"},
                {"content_type": "application/pdf", "url": "https://f/3.pdf", "filename": "报告.pdf"}
            ]
        })
    }

    #[test]
    fn parses_c2c_payload() {
        let p = parse_c2c(&sample()).unwrap();
        assert_eq!((p.openid.as_str(), p.msg_id.as_str(), p.text.as_str()), ("OPENID1", "ROBOT1.0_abc", "看看这个"));
        assert_eq!(p.attachments[0], Attachment::Image { url: "https://multimedia.nt.qq.com.cn/x.jpg".into(), ext: "jpg".into() });
        assert_eq!(p.attachments[1], Attachment::Voice { asr: Some("晚上吃什么".into()) });
        assert!(matches!(&p.attachments[2], Attachment::Video { name, .. } if name == "猫.mp4"));
        assert!(matches!(&p.attachments[3], Attachment::File { name, .. } if name == "报告.pdf"));
        assert!(parse_c2c(&json!({"id": "x"})).is_none(), "缺少发送者时忽略");
    }

    #[test]
    fn parts_for_model() {
        let p = parse_c2c(&sample()).unwrap();
        let saved = vec![
            Download::Saved(PathBuf::from("/m/a.jpg")),
            Download::Failed,
            Download::Saved(PathBuf::from("/m/b.mp4")),
            Download::TooLarge(60 * 1024 * 1024),
        ];
        let parts = to_parts(&p, &saved);
        let ContentPart::Text { text } = &parts[0] else { panic!() };
        assert!(text.starts_with("看看这个\n〔语音转文字〕晚上吃什么\n〔对方发来视频 猫.mp4"));
        assert!(text.ends_with("〔附件 报告.pdf（60.0 MB）超过 50 MB 下载上限，未保存〕"));
        assert_eq!(parts[1], ContentPart::Image { url: "file:///m/a.jpg".into() });
        assert_eq!(parts[2], ContentPart::Video { url: "file:///m/b.mp4".into() });
    }
}
