//! 长期记忆文件格式（条目以单独一行 `§` 分隔，与 Hermes 格式一致）。

/// 条目分隔符（单独一行 `§`）。
pub const MEMORY_SEP: &str = "\n§\n";

/// 单个记忆文件的字符数上限。
pub const MEMORY_FILE_LIMIT: usize = 2000;

/// 按分隔符惰性迭代记忆文件中的非空条目（去首尾空白，零拷贝借用）。
pub fn iter_memory_entries(text: &str) -> impl Iterator<Item = &str> {
    text.split(MEMORY_SEP).map(str::trim).filter(|e| !e.is_empty())
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
    fn roundtrip() {
        let list = parse_memory_entries("甲\n§\n乙\n§\n\n");
        assert_eq!(list, vec!["甲", "乙"]);
        assert_eq!(render_memory_entries(&list), "甲\n§\n乙\n");
        assert_eq!(render_memory_entries(&[]), "");
    }
}
