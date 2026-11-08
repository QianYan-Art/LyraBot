//! 聊天命令输出的统一排版（QQ 单聊纯文本）：标题带一个图标，分节用「▍」，条目用「·」，操作提示用「💡」。
//! 不用表格和 Markdown；手机一行约 20 字，单行内容尽量短。

/// 逐行拼装一张卡片。
#[derive(Debug, Default)]
pub struct Card {
    out: String,
    hinted: bool,
}

impl Card {
    /// 标题行，例如 `Card::new("📊", "运行状态")`。
    pub fn new(icon: &str, title: &str) -> Self {
        Self { out: format!("{icon} {title}"), hinted: false }
    }

    /// 普通一行。
    pub fn line(&mut self, text: impl AsRef<str>) -> &mut Self {
        self.out.push('\n');
        self.out.push_str(text.as_ref());
        self
    }

    /// 空一行后的小节标题。
    pub fn section(&mut self, name: impl AsRef<str>) -> &mut Self {
        self.out.push_str("\n\n▍");
        self.out.push_str(name.as_ref());
        self
    }

    /// 条目行，前缀「· 」。
    pub fn item(&mut self, text: impl AsRef<str>) -> &mut Self {
        self.line(format!("· {}", text.as_ref()))
    }

    /// 操作提示：第一条前空一行并加「💡」，后续提示与之对齐。
    pub fn hint(&mut self, text: impl AsRef<str>) -> &mut Self {
        if self.hinted {
            self.line(format!("　 {}", text.as_ref()))
        } else {
            self.hinted = true;
            self.out.push_str("\n\n💡 ");
            self.out.push_str(text.as_ref());
            self
        }
    }

    pub fn build(&self) -> String {
        self.out.clone()
    }
}

/// 成功的一句话回复。
pub fn ok(text: impl AsRef<str>) -> String {
    format!("✅ {}", text.as_ref())
}

/// 失败或无法执行的一句话回复。
pub fn warn(text: impl AsRef<str>) -> String {
    format!("⚠️ {}", text.as_ref())
}

/// 十格进度条加百分比：`▰▰▰▱▱▱▱▱▱▱ 30%`。比例超出 0–1 时截断。
pub fn bar(ratio: f64) -> String {
    let r = if ratio.is_finite() { ratio.clamp(0.0, 1.0) } else { 0.0 };
    let filled = (r * 10.0).round() as usize;
    format!("{}{} {:.0}%", "▰".repeat(filled), "▱".repeat(10 - filled), r * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let mut c = Card::new("📊", "状态");
        c.line("副标题").section("上下文").item("甲").hint("第一条").hint("第二条");
        assert_eq!(c.build(), "📊 状态\n副标题\n\n▍上下文\n· 甲\n\n💡 第一条\n　 第二条");
        assert_eq!(ok("好"), "✅ 好");
        assert_eq!(warn("坏"), "⚠️ 坏");
    }

    #[test]
    fn bar_bounds() {
        assert_eq!(bar(0.0), "▱▱▱▱▱▱▱▱▱▱ 0%");
        assert_eq!(bar(0.34), "▰▰▰▱▱▱▱▱▱▱ 34%");
        assert_eq!(bar(1.7), "▰▰▰▰▰▰▰▰▰▰ 100%");
        assert_eq!(bar(f64::NAN), "▱▱▱▱▱▱▱▱▱▱ 0%");
    }
}
