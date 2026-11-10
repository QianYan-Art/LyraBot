//! speak：语音合成（kind = "gptsovits_http"）：工作站 GPT-SoVITS，经服务器本机端口转发。
//! 移植自 Hermes tts_remote_gptsovits_bridge.py：POST JSON {text, media_type:"wav", streaming_mode:false, scene/profile/reference}，
//! 校验 WAV 完整后落盘投递。

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

impl Speak {
    pub fn new(host: Host) -> Self {
        let spec = ToolSpec {
            name: "speak".into(),
            description: "用你的声音把一段话合成语音发出（对方说“说……”“发语音”时用）。text 写要说的话本身，口语化，不带动作描写。\
                语气由你判断：可选 scene（场景）、profile（音色配置）、reference（参考音频），优先级 reference > profile > scene，省略时工作站自动选择；\
                只使用已知存在的名字，不要猜。语音在独立工作站生成，离线或失败时如实告诉对方，不要说已经发了。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string"},
                    "scene": {"type": "string"},
                    "profile": {"type": "string"},
                    "reference": {"type": "string"}
                },
                "required": ["text"],
                "additionalProperties": false
            }),
        };
        Self { spec, host }
    }

    async fn run(&self, args: &Value) -> Result<ToolOutput, String> {
        let (c, p) = self.host.active("tts", "gptsovits_http")?;
        let text = args.get("text").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).ok_or("text 不能为空")?;
        let max = p.int("max_text_length", 2000) as usize;
        if text.chars().count() > max {
            return Err(format!("文本超过 {max} 字，请分段"));
        }
        let mut body = json!({"text": text, "media_type": "wav", "streaming_mode": false});
        for k in ["scene", "profile", "reference"] {
            if let Some(v) = args.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
                if v.chars().count() > 128 {
                    return Err(format!("{k} 名称过长"));
                }
                body[k] = json!(v);
            }
        }
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
