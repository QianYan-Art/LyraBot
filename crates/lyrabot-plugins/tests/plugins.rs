//! 插件集成测试：wiremock 模拟各提供方。
use base64::Engine;
use lyrabot_agent::{Tool, ToolCtx};
use lyrabot_config::ConfigHandle;
use lyrabot_plugins::{Host, ImageGenerate, Speak, WebSearch};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ctx() -> ToolCtx {
    ToolCtx { session: 1, call_id: "c".into(), cancel: CancellationToken::new() }
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    v.extend(w.to_be_bytes());
    v.extend(h.to_be_bytes());
    v.extend([8, 6, 0, 0, 0]);
    v
}

fn wav() -> Vec<u8> {
    let data = vec![0u8; 64];
    let mut v = b"RIFF".to_vec();
    v.extend(((36 + data.len()) as u32).to_le_bytes());
    v.extend(b"WAVEfmt ");
    v.extend(16u32.to_le_bytes());
    v.extend([1, 0, 1, 0]);
    v.extend(16000u32.to_le_bytes());
    v.extend(32000u32.to_le_bytes());
    v.extend([2, 0, 16, 0]);
    v.extend(b"data");
    v.extend((data.len() as u32).to_le_bytes());
    v.extend(data);
    v
}

struct TempDir(std::path::PathBuf);

impl std::ops::Deref for TempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn host(base: &str) -> (Host, TempDir) {
    let dir = std::env::temp_dir().join(format!("lyrabot-plugins-{}-{}", std::process::id(), base.len() + rand()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("k.txt"), "k").unwrap();
    std::fs::write(dir.join("img.key"), "imgkey\n").unwrap();
    std::fs::write(dir.join("yb.txt"), "ybkey\nhttps://ignored.example/SearchPro\n").unwrap();
    let cfg = format!(
        r#"
[llm]
default_provider = "p"
[llm.providers.p]
base_url = "http://x"
key_file = "k.txt"
transport = "chat_completions"
[llm.providers.p.models.m]
context_length = 1000
[llm.providers.p.models.kimi-for-coding]
context_length = 1000
[persona]
model = "m"
effort = "low"
assets_dir = "persona"
[orchestra]
default_model = "m"
worker_prompt = "w.md"
[tools]
media_dir = "media"
[plugins.image]
active = "mock"
[plugins.image.providers.mock]
kind = "openai_images"
base_url = "{base}/v1"
key_file = "img.key"
model = "gpt-image-2.5-flare"
[plugins.tts]
active = "ws"
[plugins.tts.providers.ws]
kind = "gptsovits_http"
base_url = "{base}"
[plugins.search]
active = "yb"
[plugins.search.providers.yb]
kind = "tencent_wsa"
url = "{base}/SearchPro"
key_file = "yb.txt"
"#
    );
    std::fs::write(dir.join("lyrabot.toml"), cfg).unwrap();
    (Host::new(Arc::new(ConfigHandle::open(dir.join("lyrabot.toml")).unwrap())), TempDir(dir))
}

fn rand() -> usize {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos() as usize
}

#[tokio::test]
async fn image_generate_saves_and_checks_ratio() {
    let s = MockServer::start().await;
    let b64 = base64::engine::general_purpose::STANDARD.encode(png(1024, 1024));
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(header("authorization", "Bearer imgkey"))
        .and(body_partial_json(json!({"model": "gpt-image-2.5-flare", "size": "1536x1024", "quality": "medium"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"b64_json": b64}]})))
        .mount(&s)
        .await;
    let (h, dir) = host(&s.uri());
    let out = ImageGenerate::new(h).call(ctx(), json!({"prompt": "海边", "aspect_ratio": "landscape"})).await;
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.media.len(), 1);
    assert!(out.media[0].starts_with(dir.join("media")) && out.media[0].extension().unwrap() == "png");
    assert!(out.text.contains("比例不符"), "1024x1024 与横图不符应提示：{}", out.text);
}

#[tokio::test]
async fn image_error_in_200_is_failure() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"error": {"message": "image_model_unavailable"}})))
        .mount(&s)
        .await;
    let (h, _d) = host(&s.uri());
    let out = ImageGenerate::new(h).call(ctx(), json!({"prompt": "x"})).await;
    assert!(out.is_error && out.text.contains("image_model_unavailable"), "{}", out.text);
}

#[tokio::test]
async fn speak_validates_wav() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tts"))
        .and(body_partial_json(json!({"text": "早安", "media_type": "wav", "scene": "温柔"})))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wav()))
        .mount(&s)
        .await;
    let (h, _d) = host(&s.uri());
    let sp = Speak::new(h.clone());
    let out = sp.call(ctx(), json!({"text": "早安", "scene": "温柔"})).await;
    assert!(!out.is_error && out.media[0].extension().unwrap() == "wav", "{}", out.text);

    let bad = MockServer::start().await;
    let mut cut = wav();
    cut.truncate(60);
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_bytes(cut)).mount(&bad).await;
    let (h2, _d2) = host(&bad.uri());
    let out = Speak::new(h2).call(ctx(), json!({"text": "早安"})).await;
    assert!(out.is_error && out.text.contains("截断"), "{}", out.text);
}

#[tokio::test]
async fn yuanbao_search_uses_first_line_key() {
    let s = MockServer::start().await;
    let page = json!({"title": "标题", "url": "https://a.b", "site": "站点", "date": "2026/11/01 10:00:00", "passage": "摘要"}).to_string();
    Mock::given(method("POST"))
        .and(path("/SearchPro"))
        .and(header("authorization", "Bearer ybkey"))
        .and(body_partial_json(json!({"Query": "测试", "Cnt": 10, "Site": "a.b"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Response": {"Pages": [page], "RequestId": "r"}})))
        .mount(&s)
        .await;
    let (h, _d) = host(&s.uri());
    let out = WebSearch::new(h.clone()).call(ctx(), json!({"query": "测试", "site": "a.b"})).await;
    assert_eq!(out.text, "1. 标题（站点 · 2026-11-01）\nhttps://a.b\n摘要");

    let e = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Response": {"Error": {"Code": "AuthFailure", "Message": "bad key"}}})))
        .mount(&e)
        .await;
    let (h3, _d3) = host(&e.uri());
    let out = WebSearch::new(h3).call(ctx(), json!({"query": "x"})).await;
    assert!(out.is_error && out.text.contains("AuthFailure"));
}
