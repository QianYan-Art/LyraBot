use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

/// 多模态内容片段。媒体以 URL（含 data: URI）或本地路径表示，由传输层负责编码。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    Image { url: String },
    Video { url: String },
}

impl ContentPart {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// 原始 JSON 字符串；解析失败时原样回传给模型，便于其自我纠正。
    pub arguments: String,
}

/// 对话消息。系统提示不在此列——它属于上下文构建层，不进入会话树。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ContentPart>,
    /// 思考模式下 assistant 的推理文本；部分提供方要求回放时原样带回。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    /// 这条 user 消息发送时附在末尾的动态上下文（当前时间、表达提示）。随消息落库，
    /// 之后每次请求经 [`Message::wire`] 原样拼回，前缀与当初发出的字节一致、跨轮命中缓存；
    /// [`Message::text`] 不含它，学习器、检索与压缩看到的仍是原话。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<String>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self::with_parts(Role::User, vec![ContentPart::text(text)])
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::with_parts(Role::Assistant, vec![ContentPart::text(text)])
    }
    pub fn tool_result(call_id: impl Into<String>, text: impl Into<String>, is_error: bool) -> Self {
        Self {
            tool_call_id: Some(call_id.into()),
            is_error,
            ..Self::with_parts(Role::Tool, vec![ContentPart::text(text)])
        }
    }
    pub fn with_parts(role: Role, content: Vec<ContentPart>) -> Self {
        Self { role, content, reasoning: None, tool_calls: Vec::new(), tool_call_id: None, is_error: false, tail: None }
    }

    /// 发给模型的形态：把 `tail` 合并进末尾文本段（纯文本保持单段 content，含媒体时追加文本段）。
    pub fn wire(mut self) -> Self {
        if let Some(t) = self.tail.take().filter(|t| !t.is_empty()) {
            if let Some(ContentPart::Text { text }) = self.content.last_mut() {
                text.push_str("\n\n");
                text.push_str(&t);
            } else {
                self.content.push(ContentPart::text(t));
            }
        }
        self
    }

    /// 拼接全部文本片段（媒体忽略）。
    pub fn text(&self) -> String {
        let mut out = String::new();
        for p in &self.content {
            if let ContentPart::Text { text } = p {
                out.push_str(text);
            }
        }
        out
    }

    /// 粗略字符数，用于本地 token 估算。
    pub fn char_len(&self) -> usize {
        let body: usize = self
            .content
            .iter()
            .map(|p| match p {
                ContentPart::Text { text } => text.chars().count(),
                // 图片/视频按固定开销估算
                _ => 1500,
            })
            .sum();
        let calls: usize = self.tool_calls.iter().map(|c| c.name.len() + c.arguments.chars().count()).sum();
        let tail = self.tail.as_ref().map_or(0, |t| t.chars().count() + 2);
        body + calls + tail + self.reasoning.as_ref().map_or(0, |r| r.chars().count())
    }
}

/// 暴露给模型的工具定义。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema（object）。
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.prompt_tokens += o.prompt_tokens;
        self.completion_tokens += o.completion_tokens;
        self.cached_tokens += o.cached_tokens;
        self.reasoning_tokens += o.reasoning_tokens;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serde_compact() {
        let m = Message::user("hi");
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"role":"user","content":[{"type":"text","text":"hi"}]}"#);
        let back: Message = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m);
    }
}
