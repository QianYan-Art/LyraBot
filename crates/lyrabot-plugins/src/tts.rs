//! speak：语音合成（kind = "gptsovits_http"）：工作站 GPT-SoVITS，经服务器本机端口转发。
//! POST JSON {text, media_type:"wav", streaming_mode:false, scene}，校验 WAV 完整后落盘投递。
//! 语气总由主对话角色从固定选项里指定，不交给工作站按关键词自动选择：
//! 那套规则面向游戏台词，日常文本常被选成命令或严肃语气，听感尖锐失真。

use crate::Host;
use crate::media::{save, wav_ok};
use async_trait::async_trait;
use lyrabot_agent::{Tool, ToolCtx, ToolOutput};
use lyrabot_core::ToolSpec;
use serde_json::{Value, json};
use std::time::Duration;

pub struct Speak {
    spec: ToolSpec,
    host: Host,
}

/// 语气选项与用途，名字是工作站 bridge 认识的 scene 别名。
/// 不开放 narration：其参考音频在长文本上仍偶发拖长数秒（2026-11-27 实测 64 段中 1 段），讲故事用 soft。
const SCENES: [(&str, &str); 11] = [
    ("soft", "日常、温柔，也用于讲故事"),
    ("lively", "活泼轻快地聊天"),
    ("happy", "高兴、兴奋"),
    ("exclaim", "惊讶、感叹"),
    ("question", "提问"),
    ("apology", "道歉"),
    ("intimate", "安慰、哄人"),
    ("romantic", "亲昵、撒娇"),
    ("morning", "早安、打招呼"),
    ("neutral", "平稳地说明事情"),
    ("low_energy", "疲惫、低落"),
];
const DEFAULT_SCENE: &str = "soft";
/// 单次字数上限默认值：32 kHz 单声道 WAV 约 64 KB/秒，QQ 上传上限 9 MB 约 147 秒，
/// 按实测约 7 字/秒，600 字约 90 秒、6 MB，给语速较慢的语气留出余量。
const DEFAULT_MAX_CHARS: i64 = 600;

/// 去掉念不出来的字符：表情（会让工作站合成失败）与 Markdown 标记（会被当成停顿或读出声）。
fn speakable(text: &str) -> String {
    let unspoken = |c: char| {
        matches!(c as u32,
            0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0xFE00..=0xFE0F | 0x200D | 0x20E3 | 0xE0020..=0xE007F)
            || matches!(c, '*' | '#' | '`' | '_' | '|' | '>')
    };
    text.chars().filter(|&c| !unspoken(c)).collect::<String>().trim().to_owned()
}

impl Speak {
    pub fn new(host: Host) -> Self {
        let options = SCENES.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join("；");
        let spec = ToolSpec {
            name: "speak".into(),
            description: format!(
                "用你的声音把一段话合成语音发出（对方说“说……”“发语音”时用）。text 写要说的话本身，口语化，不带动作描写、表情或 Markdown 符号。\
                scene 选最贴合这段话情绪的语气（不填为 {DEFAULT_SCENE}）：{options}。\
                单次不超过 {DEFAULT_MAX_CHARS} 字，更长的文案按段落拆开多次调用，每段各发一条语音。\
                语音在独立工作站生成，离线或失败时如实告诉对方，不要说已经发了。"
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string"},
                    "scene": {"type": "string", "enum": SCENES.iter().map(|(k, _)| *k).collect::<Vec<_>>()}
                },
                "required": ["text"],
                "additionalProperties": false
            }),
        };
        Self { spec, host }
    }

    async fn run(&self, args: &Value) -> Result<ToolOutput, String> {
        let (c, p) = self.host.active("tts", "gptsovits_http")?;
        let text = speakable(args.get("text").and_then(Value::as_str).unwrap_or_default());
        if text.is_empty() {
            return Err("text 里没有可以念出来的文字".into());
        }
        let max = p.int("max_text_length", DEFAULT_MAX_CHARS) as usize;
        if text.chars().count() > max {
            return Err(format!("文本超过 {max} 字（语音过长 QQ 发不出去），请按段落拆开多次调用"));
        }
        let scene = args.get("scene").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_SCENE);
        if !SCENES.iter().any(|(k, _)| *k == scene) {
            let names = SCENES.iter().map(|(k, _)| *k).collect::<Vec<_>>().join("、");
            return Err(format!("未知语气 `{scene}`，可选：{names}"));
        }
        let body = json!({"text": text, "media_type": "wav", "streaming_mode": false, "scene": scene});
        let url = format!("{}{}", p.req("base_url")?.trim_end_matches('/'), p.str("endpoint").unwrap_or("/tts"));
        let resp = self
            .host
            .http
            .post(&url)
            .timeout(Duration::from_secs(p.int("timeout_s", 300) as u64))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    "连不上语音工作站（可能离线或转发未建立）".to_owned()
                } else if e.is_timeout() {
                    "语音生成超时".to_owned()
                } else {
                    format!("语音请求失败：{e}")
                }
            })?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("语音工作站返回 HTTP {status}：{}", String::from_utf8_lossy(&bytes).chars().take(200).collect::<String>()));
        }
        wav_ok(&bytes).map_err(|e| format!("工作站{e}"))?;
        let path = save(&c.tools.media_dir, "voice", "wav", &bytes).await.map_err(|e| format!("保存语音失败：{e}"))?;
        Ok(ToolOutput { text: "语音已生成，会随回复发出。".into(), is_error: false, media: vec![path] })
    }
}

#[async_trait]
impl Tool for Speak {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        self.run(&args).await.unwrap_or_else(ToolOutput::err)
    }
}
