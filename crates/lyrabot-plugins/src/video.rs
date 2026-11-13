//! watch_video：让支持视频的模型代看一段本地视频并回答问题。
//!
//! 主对话角色的模型（如 k3-256k）不支持视频输入时，入站视频不直接发给主对话角色，改由其按需调用本工具：
//! 用 `[tools] video_model`（默认 kimi-for-coding）看视频，把文字结果交回，主对话角色再用自己的口吻回复。
//! 视频必须位于媒体目录内；超过内联上限或时长超过 10 分钟时先用 ffmpeg 压到 480p、每秒 2 帧（结果缓存在媒体目录，随媒体清理回收）。

use crate::Host;
use async_trait::async_trait;
use lyrabot_agent::{Concurrency, Tool, ToolCtx, ToolOutput};
use lyrabot_core::{ContentPart, Effort, Message, Role, ToolSpec};
use lyrabot_llm::ChatRequest;
use lyrabot_llm::media::{Inline, VIDEO_INLINE_MAX, local_data_uri};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const PROMPT: &str = "你在帮另一个助手看视频。按时间顺序客观描述画面中的人物、动作、场景、文字与声音线索，再回答提问；\
不确定的地方直接说看不清，不要编造。用中文，不超过 600 字。";
/// 压缩时最多保留的时长（秒）。
const MAX_SECONDS: u32 = 600;
const ANSWER_LIMIT: usize = 1500;

pub struct WatchVideo {
    spec: ToolSpec,
    host: Host,
}

impl WatchVideo {
    pub fn new(host: Host) -> Self {
        let spec = ToolSpec {
            name: "watch_video".into(),
            description: "看一段对方发来的视频并回答问题（你自己看不了视频时用）。path 用消息里给出的保存路径；question 写你想知道什么，不写则概括视频内容。".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "question": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }),
        };
        Self { spec, host }
    }

    async fn run(&self, args: &Value, cancel: &CancellationToken) -> Result<String, String> {
        let c = self.host.cfg.get();
        let raw = args.get("path").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).ok_or("缺少 path")?;
        let path = within(Path::new(raw.trim_start_matches("file://")), &c.tools.media_dir)?;
        let question = args.get("question").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or("概括这段视频的内容。");
        let pcfg = c.provider(&c.persona.provider).ok_or("主对话模型提供方未配置")?;
        let model = c.tools.video_model.clone();
        if !pcfg.models.get(&model).is_some_and(|m| m.video) {
            return Err(format!("看视频的模型 {model} 不在模型目录中或不支持视频"));
        }
        // 超长视频只看前 MAX_SECONDS 秒（与文件大小无关），结果里如实说明；读不出时长时同样走截取，保证成本上界。
        // 确认未超长且体积在内联上限内的才原样发送
        let mut scope = String::new();
        let secs = probe_seconds(&path).await;
        let inline = if secs.is_none_or(|s| s > f64::from(MAX_SECONDS)) {
            None
        } else {
            match local_data_uri(path_str(&path)?, VIDEO_INLINE_MAX) {
                Ok(d) => Some(d),
                Err(Inline::Missing) => return Err("视频文件已不存在（媒体缓存可能已清理）".into()),
                Err(Inline::TooLarge) => None,
            }
        };
        let data = match inline {
            Some(d) => d,
            None => {
                let small = compress(&path, &c.tools.media_dir, cancel).await?;
                let minutes = MAX_SECONDS / 60;
                scope = match secs {
                    Some(secs) if secs > f64::from(MAX_SECONDS) => format!("（视频约 {} 分钟，只看了前 {minutes} 分钟）\n", (secs / 60.0).round()),
                    Some(_) => String::new(),
                    // 读不出时长时不确定是否被截取，如实说明
                    None => format!("（读不出视频时长；如果超过 {minutes} 分钟，只看了前 {minutes} 分钟）\n"),
                };
                local_data_uri(path_str(&small)?, VIDEO_INLINE_MAX).map_err(|_| format!("视频压缩后仍超过 {} MB，太长了看不了", VIDEO_INLINE_MAX >> 20))?
            }
        };
        let provider = lyrabot_llm::build(pcfg).map_err(|e| e.to_string())?;
        let req = ChatRequest {
            model,
            system: vec![PROMPT.to_owned()],
            messages: vec![Message::with_parts(Role::User, vec![ContentPart::Video { url: data }, ContentPart::text(question)])],
            tools: Vec::new(),
            effort: Some(Effort::Low),
            max_tokens: Some(4000),
            cache_key: None,
        };
        let out = provider.complete(req, cancel.clone()).await.map_err(|e| format!("看视频失败：{e}"))?;
        let text = out.message.text();
        if text.trim().is_empty() {
            return Err("看视频的模型没有给出内容".into());
        }
        Ok(format!("{scope}{}", crate::grok::clip(text.trim(), ANSWER_LIMIT)))
    }
}

fn path_str(p: &Path) -> Result<&str, String> {
    p.to_str().ok_or_else(|| "路径不是合法 UTF-8".to_owned())
}

/// 只允许读取媒体目录内的文件（防止借工具读取任意文件）。
fn within(path: &Path, root: &Path) -> Result<PathBuf, String> {
    let p = path.canonicalize().map_err(|_| "找不到这个视频文件".to_owned())?;
    let r = root.canonicalize().map_err(|_| "媒体目录不存在".to_owned())?;
    if !p.starts_with(&r) || !p.is_file() {
        return Err("只能查看保存在媒体目录里的视频".into());
    }
    Ok(p)
}

/// 视频时长（秒）；没有 ffprobe 或读不出时为 None。
async fn probe_seconds(src: &Path) -> Option<f64> {
    let out = tokio::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(src)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(20), out).await.ok()?.ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// 临时文件守卫：无论哪条出口离开都删除（发布成功后文件已改名，删除自然无效）。
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 压缩结果的缓存文件名：文件名 + 源路径哈希 + 大小与修改时间，同名的不同视频或被替换过的视频不会命中旧缓存。
fn cache_name(src: &Path) -> Result<String, String> {
    use std::hash::{Hash, Hasher};
    let meta = std::fs::metadata(src).map_err(|e| format!("读取视频信息失败：{e}"))?;
    // 修改时间取纳秒精度：同一秒内原地替换成同样大小的视频也能区分
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    src.hash(&mut h);
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
    Ok(format!("{stem}-{:016x}-{}-{mtime}", h.finish(), meta.len()))
}

/// 用 ffmpeg 压到 480p、每秒 2 帧、去掉音轨；同一视频的压缩结果复用。
/// 每次压缩写独立的临时文件、完成后原子改名发布，同一视频并发调用也不会互相覆盖。
async fn compress(src: &Path, media_dir: &Path, cancel: &CancellationToken) -> Result<PathBuf, String> {
    let stem = cache_name(src)?;
    let out = media_dir.join("video-cache").join(format!("{stem}-480p.mp4"));
    if out.is_file() {
        return Ok(out);
    }
    tokio::fs::create_dir_all(out.parent().expect("有父目录")).await.map_err(|e| format!("创建缓存目录失败：{e}"))?;
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp = TempFile(out.with_file_name(format!("{stem}-480p.{}-{nonce}.part.mp4", std::process::id())));
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.args(["-y", "-loglevel", "error", "-i"])
        .arg(src)
        .args(["-t", &MAX_SECONDS.to_string(), "-vf", "scale=-2:480,fps=2", "-c:v", "libx264", "-preset", "veryfast", "-crf", "32", "-an"])
        .arg(&tmp.0)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let res = tokio::select! {
        r = tokio::time::timeout(Duration::from_secs(180), cmd.output()) => r,
        _ = cancel.cancelled() => return Err("已取消".into()),
    };
    match res {
        Ok(Ok(o)) if o.status.success() => {}
        Ok(Ok(o)) => return Err(format!("压缩视频失败：{}", String::from_utf8_lossy(&o.stderr).chars().take(200).collect::<String>())),
        Ok(Err(e)) => return Err(format!("无法运行 ffmpeg：{e}")),
        Err(_) => return Err("压缩视频超时".into()),
    }
    tokio::fs::rename(&tmp.0, &out).await.map_err(|e| format!("保存压缩结果失败：{e}"))?;
    Ok(out)
}

#[async_trait]
impl Tool for WatchVideo {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn call(&self, ctx: ToolCtx, args: Value) -> ToolOutput {
        match self.run(&args, &ctx.cancel).await {
            Ok(t) => ToolOutput::ok(t),
            Err(e) => ToolOutput::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::within;

    #[test]
    fn cache_name_distinguishes_same_stem_and_replaced_content() {
        let root = std::env::temp_dir().join(format!("lyrabot-vcache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/clip.mp4"), b"one").unwrap();
        std::fs::write(root.join("b/clip.mp4"), b"one").unwrap();
        let a = super::cache_name(&root.join("a/clip.mp4")).unwrap();
        assert_ne!(a, super::cache_name(&root.join("b/clip.mp4")).unwrap(), "不同目录的同名视频");
        std::fs::write(root.join("a/clip.mp4"), b"longer").unwrap();
        assert_ne!(a, super::cache_name(&root.join("a/clip.mp4")).unwrap(), "内容被替换");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn only_files_inside_media_dir() {
        let root = std::env::temp_dir().join(format!("lyrabot-video-{}", std::process::id()));
        std::fs::create_dir_all(root.join("inbox")).unwrap();
        std::fs::write(root.join("inbox/a.mp4"), b"x").unwrap();
        let outside = std::env::temp_dir().join(format!("lyrabot-video-outside-{}.mp4", std::process::id()));
        std::fs::write(&outside, b"x").unwrap();
        assert!(within(&root.join("inbox/a.mp4"), &root).is_ok());
        assert!(within(&root.join("inbox/../inbox/a.mp4"), &root).is_ok());
        assert!(within(&outside, &root).is_err(), "媒体目录外的文件不能看");
        assert!(within(&root.join("inbox"), &root).is_err(), "目录不是视频");
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_file(&outside).unwrap();
    }
}
