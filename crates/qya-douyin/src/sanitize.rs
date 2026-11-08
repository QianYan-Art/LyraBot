//! 参数校验与响应净化（移植自 Hermes douyin-login 插件，规则逐条对应）。
//! 服务端响应里不得出现 cookie、base64 原文、本地路径等内容，命中即整体拒绝。

use base64::Engine;
use serde_json::Value;

pub fn is_token(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn is_hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn is_hex32(s: &str) -> bool {
    is_hex(s, 32)
}

pub fn is_sha256(s: &str) -> bool {
    is_hex(s, 64)
}

/// 作品编号：`[a-z0-9][a-z0-9-]{0,63}`。
pub fn is_release_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// “确认发布 <plan_id>”整条消息匹配。
pub fn confirm_token(text: &str) -> Option<&str> {
    let t = text.strip_prefix("确认发布 ")?;
    is_token(t).then_some(t)
}

/// 纯文本六位数字（短信验证码）。
pub fn sms_code(text: &str) -> Option<&str> {
    let t = text.trim();
    (t.len() == 6 && t.bytes().all(|b| b.is_ascii_digit())).then_some(t)
}

fn has_cookie_assignment(s: &str) -> bool {
    let lower = s.to_lowercase();
    for key in ["set-cookie", "cookie", "sessionid", "passport_csrf_token"] {
        let mut from = 0;
        while let Some(i) = lower[from..].find(key) {
            let rest = lower[from + i + key.len()..].trim_start();
            if rest.starts_with(':') || rest.starts_with('=') {
                return true;
            }
            from += i + key.len();
        }
    }
    false
}

/// 去掉 http(s) 链接后检测绝对路径（Unix /x、Windows C:\、UNC \\a\b）。
fn has_absolute_path(s: &str) -> bool {
    let cleaned: String = s
        .split_whitespace()
        .filter(|w| !(w.to_ascii_lowercase().starts_with("http://") || w.to_ascii_lowercase().starts_with("https://")))
        .collect::<Vec<_>>()
        .join(" ");
    let c: Vec<char> = cleaned.chars().collect();
    for i in 0..c.len() {
        let prev_ok = i == 0 || !(c[i - 1].is_ascii_alphanumeric() || c[i - 1] == ':' || c[i - 1] == '/');
        if !prev_ok {
            continue;
        }
        let next = c.get(i + 1).copied();
        // Unix：/ 后接非空白非斜杠
        if c[i] == '/' && next.is_some_and(|n| !n.is_whitespace() && n != '/') {
            return true;
        }
        // Windows 盘符
        if c[i].is_ascii_alphabetic() && next == Some(':') && matches!(c.get(i + 2), Some('\\' | '/')) {
            return true;
        }
        // UNC
        if c[i] == '\\' && next == Some('\\') && c.get(i + 2).is_some_and(|n| !n.is_whitespace() && *n != '\\') {
            return true;
        }
    }
    false
}

fn is_long_base64(s: &str) -> bool {
    s.len() >= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'/' | b'=' | b'-'))
}

fn looks_like_base64(s: &str) -> bool {
    if s.len() < 8 || !s.len().is_multiple_of(4) {
        return false;
    }
    let body = s.trim_end_matches('=');
    if s.len() - body.len() > 2 || !body.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/') {
        return false;
    }
    base64::engine::general_purpose::STANDARD.decode(s).is_ok_and(|d| d.len() >= 16)
}

const ID_FIELDS: &[&str] = &["release_id", "job_id", "image_id", "cover_id", "cover_sha256", "video_sha256", "token"];

/// 递归扫描：键名含 cookie/base64/path，或值像 cookie 赋值、file://、绝对路径、base64 原文，则拒绝。
pub fn scan(v: &Value) -> Result<(), String> {
    fn walk(v: &Value, field: Option<&str>) -> Result<(), String> {
        match v {
            Value::Object(m) => {
                for (k, child) in m {
                    let lk = k.to_lowercase();
                    if ["cookie", "base64", "path"].iter().any(|f| lk.contains(f)) {
                        return Err(format!("响应包含禁止字段 {k}"));
                    }
                    walk(child, Some(&lk))?;
                }
                Ok(())
            }
            Value::Array(a) => a.iter().try_for_each(|c| walk(c, field)),
            Value::String(s) => {
                let id_field = field.is_some_and(|f| ID_FIELDS.contains(&f));
                if has_cookie_assignment(s)
                    || s.to_lowercase().starts_with("file://")
                    || has_absolute_path(s)
                    || is_long_base64(s)
                    || (!id_field && looks_like_base64(s))
                {
                    return Err("响应包含禁止内容".into());
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    walk(v, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validators() {
        assert!(is_token("abcdefghijklmnopqrstuvwxyz012-_9"));
        assert!(!is_token("short"));
        assert!(is_release_id("ep02-starlib") && !is_release_id("-x") && !is_release_id("A1"));
        assert_eq!(confirm_token("确认发布 abcdefghijklmnopqrstuvwxyz012-_9"), Some("abcdefghijklmnopqrstuvwxyz012-_9"));
        assert_eq!(confirm_token("确认发布  abcdefghijklmnopqrstuvwxyz012-_9"), None);
        assert_eq!(sms_code(" 123456 "), Some("123456"));
        assert_eq!(sms_code("1234567"), None);
    }

    #[test]
    fn scan_rejects_sensitive() {
        assert!(scan(&json!({"ok": true, "message": "已就绪", "releases": [{"release_id": "ep01", "title": "第一栞"}]})).is_ok());
        assert!(scan(&json!({"message": "见 https://www.douyin.com/a/b 页面"})).is_ok(), "链接不算路径");
        assert!(scan(&json!({"message": "文件在 /var/lib/x"})).is_err());
        assert!(scan(&json!({"message": "C:\\Users\\x"})).is_err());
        assert!(scan(&json!({"message": "sessionid=abc"})).is_err());
        assert!(scan(&json!({"cookie_jar": "x"})).is_err());
        assert!(scan(&json!({"message": "and/or 都可以"})).is_ok(), "字母后的斜杠不算路径");
        assert!(scan(&json!({"cover_id": "a".repeat(64)})).is_ok());
        assert!(scan(&json!({"note": "aGVsbG8gd29ybGQgaGVsbG8gd29ybGQ="})).is_err(), "疑似 base64");
    }
}
