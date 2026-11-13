//! 长期记忆文件格式（条目以单独一行 `§` 分隔，与 Hermes 格式一致）。

/// 渲染时使用的条目分隔符（单独一行 `§`）；解析时只要求 `§` 独占一行，兼容 LF 与 CRLF。
pub const MEMORY_SEP: &str = "\n§\n";

/// 单个记忆文件的字符数上限。
pub const MEMORY_FILE_LIMIT: usize = 2000;

/// 按分隔行惰性迭代记忆文件中的非空条目（去首尾空白，零拷贝借用，条目内部换行保留）。
pub fn iter_memory_entries(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = Some(text);
    std::iter::from_fn(move || {
        loop {
            let (entry, next) = split_at_sep(rest?);
            rest = next;
            let e = entry.trim();
            if !e.is_empty() {
                return Some(e);
            }
        }
    })
}

/// 在第一处“单独一行 `§`”处切开，返回分隔行之前的内容与之后的剩余部分。
fn split_at_sep(s: &str) -> (&str, Option<&str>) {
    let mut start = 0;
    for line in s.split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']) == "§" {
            return (&s[..start], Some(&s[start + line.len()..]));
        }
        start += line.len();
    }
    (s, None)
}

/// 规范化一条待写入的条目：去掉会被解析成分隔符的独立 `§` 行（LF 与 CRLF 同样处理）并去首尾空白，
/// 保证写入后再读回仍是同一条。
pub fn sanitize_entry(text: &str) -> String {
    text.split_inclusive('\n').filter(|line| line.trim_end_matches(['\n', '\r']) != "§").collect::<String>().trim().to_owned()
}

/// 读取记忆文件：文件不存在视为空；权限、I/O、编码等错误如实返回，避免被当成空文件后覆盖原内容。
pub fn read_memory_file(path: &std::path::Path) -> std::io::Result<String> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        r => r,
    }
}

/// 解析记忆文件文本为去空白后的非空条目列表。
pub fn parse_memory_entries(text: &str) -> Vec<String> {
    iter_memory_entries(text).map(str::to_owned).collect()
}

/// 将条目列表渲染为记忆文件文本（空列表返回空串，非空列表以换行结尾）。
pub fn render_memory_entries(list: &[String]) -> String {
    if list.is_empty() { String::new() } else { format!("{}\n", list.join(MEMORY_SEP)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_edge_cases() {
        let list = parse_memory_entries("甲\n§\n乙\n§\n\n");
        assert_eq!(list, vec!["甲", "乙"]);
        assert_eq!(render_memory_entries(&list), "甲\n§\n乙\n");
        assert_eq!(render_memory_entries(&[]), "");

        // 边界：空串、仅含空白与分隔符、首尾多余空白、条目内部换行保留
        assert!(parse_memory_entries("").is_empty());
        assert!(parse_memory_entries("  \n§\n  \n§\n ").is_empty());
        let multiline = parse_memory_entries("  第一行\n第二行  \n§\n   \n§\n单行");
        assert_eq!(multiline, vec!["第一行\n第二行", "单行"]);

        // CRLF 文件、行首/行尾的分隔行
        assert_eq!(parse_memory_entries("甲\r\n第二行\r\n§\r\n乙\r\n"), vec!["甲\r\n第二行", "乙"]);
        assert_eq!(parse_memory_entries("§\n甲\n§"), vec!["甲"]);
        // 条目正文里出现的 § 不是分隔符
        assert_eq!(parse_memory_entries("符号 § 在句中\n§\n乙"), vec!["符号 § 在句中", "乙"]);

        // 写入前规范化：独立的 § 行会被去掉，写入再读回仍是同一条
        let clean = sanitize_entry("甲\r\n§\r\n乙");
        assert_eq!(parse_memory_entries(&render_memory_entries(std::slice::from_ref(&clean))), vec![clean]);

        // 边界：iter_memory_entries 惰性借用，take(1) 只取首条
        let mut it = iter_memory_entries("首条\n§\n次条\n§\n末条");
        assert_eq!(it.next(), Some("首条"));
    }

    #[test]
    fn missing_file_reads_as_empty_but_other_errors_surface() {
        let dir = std::env::temp_dir().join(format!("lyrabot-memfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(read_memory_file(&dir.join("none.md")).unwrap(), "");
        assert!(read_memory_file(&dir).is_err(), "读目录等错误不能当成空文件");
        std::fs::write(dir.join("bad.md"), [0xff, 0xfe]).unwrap();
        assert!(read_memory_file(&dir.join("bad.md")).is_err(), "非法 UTF-8 不能当成空文件");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
