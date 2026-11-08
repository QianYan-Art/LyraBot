//! sticker：按情绪从表情包目录选一张图随回复发送。
//! 文件名约定沿用 Hermes：`情绪-场景-角色.扩展名`（如 shy-flower-bear.jpg）。

use async_trait::async_trait;
use qya_agent::{Concurrency, Tool, ToolCtx, ToolOutput};
use qya_core::ToolSpec;
use serde_json::json;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

pub struct Sticker {
    spec: ToolSpec,
    files: Vec<(String, PathBuf)>,
    seed: AtomicU64,
}

impl Sticker {
    /// 扫描目录；目录不存在或为空时返回 `None`（不向模型暴露该工具）。
    pub fn scan(dir: &Path) -> Option<Self> {
        let mut files: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
            .ok()?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()).is_some_and(|x| EXTS.contains(&x.to_ascii_lowercase().as_str())))
            .filter_map(|p| Some((p.file_stem()?.to_str()?.to_ascii_lowercase(), p)))
            .collect();
        if files.is_empty() {
            return None;
        }
        files.sort();
        // 情绪清单按名字排序，保证工具说明字节稳定
        let emotions: BTreeSet<&str> = files.iter().filter_map(|(n, _)| n.split('-').next()).collect();
        let list: Vec<&str> = emotions.into_iter().collect();
        let spec = ToolSpec {
            name: "sticker".into(),
            description: format!(
                "发一张表情包，跟在你这条回复后面。适合撒娇、调皮、害羞、无语等情绪节点，偶尔用，不要每条都发。可用情绪：{}",
                list.join("、")
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "emotion": {"type": "string", "enum": list},
                    "hint": {"type": "string", "description": "可选：画面关键词（英文），用于在同一情绪里挑图"}
                },
                "required": ["emotion"],
                "additionalProperties": false
            }),
        };
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(7) | 1;
        Some(Self { spec, files, seed: AtomicU64::new(seed) })
    }

    /// xorshift：只用于在候选里随机挑一张，不需要密码学强度。
    fn next(&self) -> u64 {
        let mut x = self.seed.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.seed.store(x, Ordering::Relaxed);
        x
    }

    pub fn pick(&self, emotion: &str, hint: Option<&str>) -> Option<&Path> {
        let emotion = emotion.trim().to_ascii_lowercase();
        let mut cands: Vec<&(String, PathBuf)> = self.files.iter().filter(|(n, _)| n.split('-').next() == Some(emotion.as_str())).collect();
        if cands.is_empty() {
            cands = self.files.iter().filter(|(n, _)| n.contains(&emotion)).collect();
        }
        if let Some(h) = hint.map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty()) {
            let narrowed: Vec<_> = cands.iter().copied().filter(|(n, _)| n.contains(&h)).collect();
            if !narrowed.is_empty() {
                cands = narrowed;
            }
        }
        if cands.is_empty() {
            return None;
        }
        let i = (self.next() % cands.len() as u64) as usize;
        Some(cands[i].1.as_path())
    }
}

#[async_trait]
impl Tool for Sticker {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn call(&self, _ctx: ToolCtx, args: serde_json::Value) -> ToolOutput {
        let emotion = args.get("emotion").and_then(|v| v.as_str()).unwrap_or_default();
        let hint = args.get("hint").and_then(|v| v.as_str());
        match self.pick(emotion, hint) {
            Some(p) => ToolOutput { text: "表情包会跟在你的回复后发出，不用在文字里提它".into(), is_error: false, media: vec![p.to_path_buf()] },
            None => ToolOutput::err(format!("没有情绪为 `{emotion}` 的表情包")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("qya-sticker-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for f in ["shy-flower-bear.jpg", "shy-paws-pinkcat.gif", "cry-blob.gif", "notes.txt"] {
            std::fs::write(d.join(f), b"x").unwrap();
        }
        d
    }

    #[test]
    fn scan_and_pick() {
        let s = Sticker::scan(&dir()).unwrap();
        assert!(s.spec.description.contains("cry、shy"));
        for _ in 0..10 {
            let p = s.pick("shy", None).unwrap();
            assert!(p.file_name().unwrap().to_str().unwrap().starts_with("shy-"));
        }
        assert!(s.pick("shy", Some("pinkcat")).unwrap().to_str().unwrap().contains("pinkcat"));
        assert!(s.pick("angry", None).is_none());
        assert!(Sticker::scan(Path::new("Z:/definitely/missing")).is_none());
    }
}
