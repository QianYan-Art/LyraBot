//! 搜索结果整形：不同来源统一成“标题（来源 · 日期）/ 链接 / 摘要”的紧凑列表，
//! 去重、在句子边界截断摘要，减少模型阅读负担与输入 token。

/// 一条搜索结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    /// 来源与日期等附加信息（可空）
    pub meta: String,
    pub summary: String,
}

/// 单条摘要上限（字）。
pub const SUMMARY_CHARS: usize = 200;

/// 渲染结果列表：按链接与标题去重，跳过标题和摘要都为空的条目。
pub fn render(query: &str, hits: Vec<Hit>) -> String {
    let mut seen_urls = Vec::new();
    let mut seen_titles = Vec::new();
    let mut out = Vec::new();
    for h in hits {
        let (url_key, title_key) = (url_key(&h.url), h.title.trim().to_lowercase());
        if (h.title.trim().is_empty() && h.summary.trim().is_empty())
            || (!url_key.is_empty() && seen_urls.contains(&url_key))
            || (!title_key.is_empty() && seen_titles.contains(&title_key))
        {
            continue;
        }
        seen_urls.push(url_key);
        seen_titles.push(title_key);
        let head = if h.meta.is_empty() { h.title.trim().to_owned() } else { format!("{}（{}）", h.title.trim(), h.meta) };
        let mut item = format!("{}. {head}", out.len() + 1);
        if !h.url.is_empty() {
            item.push('\n');
            item.push_str(h.url.trim());
        }
        let summary = trim_sentence(&h.summary, SUMMARY_CHARS);
        if !summary.is_empty() {
            item.push('\n');
            item.push_str(&summary);
        }
        out.push(item);
    }
    if out.is_empty() {
        return format!("“{query}”没有搜到结果，可以换个说法或去掉站点限制再试");
    }
    out.join("\n\n")
}

/// 链接去重键：忽略协议、`www.`、末尾斜杠与锚点；主机名不分大小写，路径与参数保留大小写。
fn url_key(url: &str) -> String {
    let u = url.trim().split('#').next().unwrap_or("");
    let u = u.split_once("://").map_or(u, |(_, rest)| rest);
    // 主机名止于第一个 / 或 ?，其后的路径与参数保留大小写
    let cut = u.find(['/', '?']).unwrap_or(u.len());
    let (host, rest) = u.split_at(cut);
    let host = host.to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    format!("{host}{}", rest.trim_end_matches('/'))
}

/// 压平空白后截断：优先停在上限内最后一个句末标点，没有合适的句点才硬截并加省略号。
pub fn trim_sentence(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let head: String = flat.chars().take(max).collect();
    let cut = head.char_indices().filter(|(_, c)| matches!(c, '。' | '！' | '？' | '；' | '.' | '!' | '?' | ';')).map(|(i, c)| i + c.len_utf8()).next_back();
    match cut {
        Some(i) if head[..i].chars().count() >= max / 2 => head[..i].to_owned(),
        _ => format!("{head}…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(title: &str, url: &str, summary: &str) -> Hit {
        Hit { title: title.into(), url: url.into(), meta: String::new(), summary: summary.into() }
    }

    #[test]
    fn dedupes_and_numbers() {
        let hits = vec![
            hit("北京天气", "https://www.w.cn/1/", "有雨"),
            hit("北京天气预报", "http://w.cn/1#top", "同一页面"),
            hit("北京天气", "https://other.cn/2", "同标题"),
            hit("", "", ""),
            hit("空气质量", "https://a.cn/3", "良"),
        ];
        assert_eq!(render("北京", hits), "1. 北京天气\nhttps://www.w.cn/1/\n有雨\n\n2. 空气质量\nhttps://a.cn/3\n良");
        // 路径大小写不同是不同页面；主机名大小写不同是同一页面
        let pages = vec![hit("手册", "https://Docs.cn/Manual", "甲"), hit("另一本", "https://docs.cn/manual", "乙"), hit("同站", "https://DOCS.cn/Manual/", "丙")];
        assert_eq!(render("q", pages).matches("\n\n").count(), 1, "保留两条");
        let queries = vec![hit("甲", "https://x.cn?q=A", "1"), hit("乙", "https://X.cn?q=a", "2")];
        assert_eq!(render("q", queries).matches("\n\n").count(), 1, "查询参数区分大小写");
        assert!(render("x", vec![]).contains("没有搜到"));
    }

    #[test]
    fn trims_at_sentence_boundary() {
        assert_eq!(trim_sentence("  第一句。\n 第二句  ", 50), "第一句。 第二句");
        let long = format!("{}。{}", "甲".repeat(120), "乙".repeat(200));
        assert_eq!(trim_sentence(&long, 200), format!("{}。", "甲".repeat(120)));
        assert!(trim_sentence(&"丙".repeat(300), 200).ends_with('…'));
    }
}
