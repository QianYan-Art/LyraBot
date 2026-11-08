//! 传输层集成测试：wiremock 模拟服务端；`live_*` 用真实 key，默认忽略。
use qya_config::{ProviderConfig, Timeouts, Transport};
use qya_core::{Effort, Message, Secret, ToolSpec};
use qya_llm::{ChatRequest, ErrorKind, Provider, StreamEvent};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn cfg(base: &str, key: &str) -> ProviderConfig {
    let mut effort_map = BTreeMap::new();
    effort_map.insert(Effort::Medium, "high".to_owned());
    ProviderConfig {
        id: "t".into(),
        base_url: base.trim_end_matches('/').into(),
        api_key: Secret::new(key),
        transport: Transport::ChatCompletions,
        user_agent: None,
        effort_map,
        models: BTreeMap::new(),
        timeouts: Timeouts { connect_s: 2, first_byte_s: 5, idle_s: 5 },
    }
}

const SSE: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

#[tokio::test]
async fn retries_5xx_then_streams_and_maps_effort() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503)).up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .and(header("authorization", "Bearer k"))
        .and(body_partial_json(serde_json::json!({"reasoning_effort": "high", "stream": true})))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(SSE))
        .mount(&server).await;
    let p = qya_llm::ChatCompletionsProvider::new(cfg(&server.uri(), "k")).unwrap();
    let req = ChatRequest { model: "m".into(), messages: vec![Message::user("hi")], effort: Some(Effort::Medium), ..Default::default() };
    let c = p.complete(req, CancellationToken::new()).await.unwrap();
    assert_eq!(c.message.text(), "ok");
}

#[tokio::test]
async fn auth_error_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .expect(1).mount(&server).await;
    let p = qya_llm::ChatCompletionsProvider::new(cfg(&server.uri(), "k")).unwrap();
    let err = p.complete(ChatRequest { model: "m".into(), ..Default::default() }, CancellationToken::new()).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth);
}

fn live_cfg() -> Option<ProviderConfig> {
    let f = std::env::var("QYA_KIMI_KEY_FILE").ok()?;
    let key = std::fs::read_to_string(f).ok()?.trim().to_owned();
    let mut c = cfg("https://api.kimi.com/coding/v1", &key);
    c.effort_map.insert(Effort::Low, "low".into());
    Some(c)
}

#[tokio::test]
#[ignore = "需要真实 kimi key：QYA_KIMI_KEY_FILE=kimi.txt cargo test -p qya-llm -- --ignored"]
async fn live_tool_call_roundtrip() {
    let p = qya_llm::ChatCompletionsProvider::new(live_cfg().expect("QYA_KIMI_KEY_FILE")).unwrap();
    let models = p.list_models().await.unwrap();
    println!("models: {:?}", models.iter().map(|m| &m.id).collect::<Vec<_>>());
    let tool = ToolSpec {
        name: "get_time".into(),
        description: "获取当前时间".into(),
        parameters: serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
    };
    let mut req = ChatRequest {
        model: "kimi-for-coding".into(),
        system: vec!["你是测试助手。需要时间时必须调用 get_time。".into()],
        messages: vec![Message::user("现在几点？")],
        tools: vec![tool],
        effort: Some(Effort::Low),
        max_tokens: Some(2000),
    };
    let c1 = p.complete(req.clone(), CancellationToken::new()).await.unwrap();
    assert!(!c1.message.tool_calls.is_empty(), "模型未调用工具：{:?}", c1.message);
    let call_id = c1.message.tool_calls[0].id.clone();
    // 回放带工具调用的 assistant 消息（验证 reasoning_content 回放不报 400）
    req.messages.push(c1.message);
    req.messages.push(Message::tool_result(call_id, "2026-11-07 12:00", false));
    let mut rx = p.stream(req, CancellationToken::new()).await.unwrap();
    let mut deltas = 0;
    while let Some(ev) = rx.recv().await {
        match ev.unwrap() {
            StreamEvent::TextDelta(_) => deltas += 1,
            StreamEvent::Completed(c) => {
                println!("final: {} usage={:?}", c.message.text(), c.usage);
                assert!(c.message.text().contains("12"));
            }
            _ => {}
        }
    }
    assert!(deltas > 0);
}
