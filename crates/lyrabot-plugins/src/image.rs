//! image_generate：OpenAI Images 兼容接口（kind = "openai_images"）。
//! 无输入图走 /images/generations（JSON），有输入图走 /images/edits（multipart）。
//! 沿用 Hermes 已验证的规则：HTTP 200 但正文带 error 视为失败；实际尺寸与请求比例不符时提示。

use crate::Host;
use crate::media::{image_info, save};
use async_trait::async_trait;
use base64::Engine;
use lyrabot_agent::{Tool, ToolCtx, ToolOutput};
use lyrabot_core::ToolSpec;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

pub struct ImageGenerate {
    spec: ToolSpec,
    host: Host,
}

fn size_of(aspect: &str) -> (&'static str, f64) {
    match aspect {
        "landscape" => ("1536x1024", 1.5),
        "portrait" => ("1024x1536", 2.0 / 3.0),
        _ => ("1024x1024", 1.0),
    }
}

impl ImageGenerate {
    pub fn new(host: Host) -> Self {
        let spec = ToolSpec {
            name: "image_generate".into(),
            description: "生成或修改图片，成功后图片会自动随回复发出，不用在文字里贴路径。prompt 写清画面内容、风格与构图。\
                aspect_ratio：square（默认）/ landscape 横图 / portrait 竖图。要求低/中/高质量时传 quality=low/medium/high，没说就不传。\
                改图或参考图时把图片的本地路径或 URL 放进 input_images，局部重绘再给 mask。生成需要几十秒到数分钟。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string"},
                    "aspect_ratio": {"type": "string", "enum": ["square", "landscape", "portrait"]},
                    "quality": {"type": "string", "enum": ["low", "medium", "high"]},
                    "n": {"type": "integer", "minimum": 1, "maximum": 4},
                    "input_images": {"type": "array", "items": {"type": "string"}},
                    "mask": {"type": "string"}
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
        };
        Self { spec, host }
    }

    async fn load_image(&self, r: &str) -> Result<(Vec<u8>, String), String> {
        if r.starts_with("http://") || r.starts_with("https://") {
            let resp = self.host.http.get(r).timeout(Duration::from_secs(60)).send().await.map_err(|e| format!("下载 {r} 失败：{e}"))?;
            let b = resp.bytes().await.map_err(|e| e.to_string())?.to_vec();
            let ext = image_info(&b).map(|i| i.0).unwrap_or("png");
            return Ok((b, format!("input.{ext}")));
        }
        let p = PathBuf::from(r.strip_prefix("file://").unwrap_or(r));
        let b = tokio::fs::read(&p).await.map_err(|e| format!("读取 {} 失败：{e}", p.display()))?;
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("input.png").to_owned();
        Ok((b, name))
    }

    async fn run(&self, args: &Value) -> Result<ToolOutput, String> {
        let (c, p) = self.host.active("image", "openai_images")?;
        let prompt = args.get("prompt").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).ok_or("缺少 prompt")?;
        let base = p.req("base_url")?.trim_end_matches('/').to_owned();
        let key = p.secret(&c, "key")?;
        let model = p.req("model")?.to_owned();
        let quality = args.get("quality").and_then(Value::as_str).or(p.str("quality")).unwrap_or("medium").to_owned();
        let aspect = args.get("aspect_ratio").and_then(Value::as_str).unwrap_or("square");
        let (size, want_ratio) = size_of(aspect);
        let n = args.get("n").and_then(Value::as_u64).unwrap_or(1).clamp(1, 4);
        let timeout = Duration::from_secs(p.int("timeout_s", 180) as u64);
        let inputs: Vec<String> = args
            .get("input_images")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        let mask = args.get("mask").and_then(Value::as_str);
        if mask.is_some() && inputs.is_empty() {
            return Err("mask 需要同时提供 input_images".into());
        }

        let resp = if inputs.is_empty() {
            let body = json!({"model": model, "prompt": prompt, "size": size, "n": n, "quality": quality});
            self.host.http.post(format!("{base}/images/generations")).bearer_auth(key.expose()).timeout(timeout).json(&body).send().await
        } else {
            let mut form = reqwest::multipart::Form::new()
                .text("model", model.clone())
                .text("prompt", prompt.to_owned())
                .text("size", size)
                .text("n", n.to_string())
                .text("quality", quality.clone());
            let field = if inputs.len() == 1 { "image" } else { "image[]" };
            for r in &inputs {
                let (b, name) = self.load_image(r).await?;
                form = form.part(field, reqwest::multipart::Part::bytes(b).file_name(name));
            }
            if let Some(m) = mask {
                let (b, name) = self.load_image(m).await?;
                form = form.part("mask", reqwest::multipart::Part::bytes(b).file_name(name));
            }
            self.host.http.post(format!("{base}/images/edits")).bearer_auth(key.expose()).timeout(timeout).multipart(form).send().await
        };
        let resp = resp.map_err(|e| if e.is_timeout() { format!("生图超时（{} 秒）", timeout.as_secs()) } else { format!("生图请求失败：{e}") })?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&text).map_err(|_| format!("生图接口返回无法解析（HTTP {status}）：{}", text.chars().take(300).collect::<String>()))?;
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            let msg = err.get("message").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| err.to_string());
            return Err(format!("生图失败（HTTP {status}）：{msg}"));
        }
        if !status.is_success() {
            return Err(format!("生图失败（HTTP {status}）：{}", text.chars().take(300).collect::<String>()));
        }
        let data = v.get("data").and_then(Value::as_array).filter(|d| !d.is_empty()).ok_or("生图接口没有返回图片")?;
        let media_dir = c.tools.media_dir.clone();
        let mut paths = Vec::new();
        let mut notes = Vec::new();
        for item in data {
            let bytes = if let Some(b64) = item.get("b64_json").and_then(Value::as_str) {
                base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| format!("图片解码失败：{e}"))?
            } else if let Some(url) = item.get("url").and_then(Value::as_str) {
                self.host.http.get(url).timeout(Duration::from_secs(60)).send().await.map_err(|e| e.to_string())?.bytes().await.map_err(|e| e.to_string())?.to_vec()
            } else {
                continue;
            };
            let (ext, w, h) = image_info(&bytes).unwrap_or(("png", 0, 0));
            if w > 0 && h > 0 && ((w as f64 / h as f64) - want_ratio).abs() > 0.05 {
                notes.push(format!("实际尺寸 {w}x{h} 与请求的 {size} 比例不符"));
            }
            paths.push(save(&media_dir, "image", ext, &bytes).await.map_err(|e| format!("保存图片失败：{e}"))?);
        }
        if paths.is_empty() {
            return Err("生图接口返回的数据里没有图片".into());
        }
        let mut msg = format!(
            "已生成 {} 张图（提供方 {}，模型 {model}，质量 {quality}，{size}），会随回复发出。",
            paths.len(),
            p.name
        );
        if !notes.is_empty() {
            msg.push_str(&format!("注意：{}。", notes.join("；")));
        }
        Ok(ToolOutput { text: msg, is_error: false, media: paths })
    }
}

#[async_trait]
impl Tool for ImageGenerate {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        self.run(&args).await.unwrap_or_else(ToolOutput::err)
    }
}
