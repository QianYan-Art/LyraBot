//! QQ 适配器契约测试：本地 WebSocket 服务器模拟网关，wiremock 模拟开放平台 HTTP 接口。
//! 覆盖：Identify 载荷、READY、心跳带 seq、入站过滤与转换、op7 后 Resume、被动回复 msg_id/msg_seq、
//! 媒体上传后发送、致命关闭码停止重连、延迟消息落库并在下次入站后补发。
use futures_util::{SinkExt, StreamExt};
use qya_config::QqConfig;
use qya_core::Secret;
use qya_platform::qq::QqPlatform;
use qya_platform::{Outbound, Platform};
use qya_store::Store;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn recv_json(ws: &mut Ws) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("等待客户端消息超时").unwrap().unwrap();
        if let Message::Text(t) = m {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::text(v.to_string())).await.unwrap();
}

fn c2c(s: u64, id: &str, openid: &str, text: &str) -> Value {
    json!({"op": 0, "s": s, "t": "C2C_MESSAGE_CREATE", "d": {"id": id, "content": text, "author": {"user_openid": openid}}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_and_send_contract() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_port = listener.local_addr().unwrap().port();
    let http = MockServer::start().await;
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "T", "expires_in": "7200"}))).mount(&http).await;
    Mock::given(method("GET")).and(path("/gateway")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"url": format!("ws://127.0.0.1:{ws_port}")}))).mount(&http).await;
    Mock::given(method("POST")).and(path("/v2/users/OPENID1/messages")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "out1"}))).mount(&http).await;
    Mock::given(method("POST")).and(path("/v2/users/OPENID1/files")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"file_info": "FI"}))).mount(&http).await;

    let dir = std::env::temp_dir().join(format!("qya-qq-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = QqConfig {
        app_id: "APP".into(),
        secret: Secret::new("SECRET"),
        allowed_users: vec!["OPENID1".into()],
        api_base: http.uri(),
        token_url: format!("{}/token", http.uri()),
        reply_window_min: 60,
        max_replies: 5,
        proactive_monthly_quota: 4,
        inbox_dir: dir.join("inbox"),
    };
    let qq = Arc::new(QqPlatform::new(cfg, Store::in_memory().unwrap()));
    let (tx, mut inbox) = mpsc::channel(8);
    let runner = {
        let qq = qq.clone();
        tokio::spawn(async move { qq.run(tx).await })
    };

    // —— 第一次连接：Identify → READY → 心跳 → 入站 → op7 ——
    let (stream, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
    send(&mut ws, json!({"op": 10, "d": {"heartbeat_interval": 200}})).await;
    let ident = recv_json(&mut ws).await;
    assert_eq!(ident["op"], 2);
    assert_eq!(ident["d"]["token"], "QQBot T");
    assert_eq!(ident["d"]["intents"], json!((1u64 << 25) | (1 << 30) | (1 << 12) | (1 << 26)));
    send(&mut ws, json!({"op": 0, "s": 1, "t": "READY", "d": {"session_id": "S1"}})).await;
    let hb = recv_json(&mut ws).await;
    assert_eq!((hb["op"].clone(), hb["d"].clone()), (json!(1), json!(1)), "心跳应携带最新 seq");
    send(&mut ws, c2c(2, "MSG1", "OPENID1", "在吗")).await;
    send(&mut ws, c2c(3, "MSG2", "STRANGER", "你好")).await;
    send(&mut ws, c2c(4, "MSG1", "OPENID1", "在吗")).await; // 重复投递，应去重
    let got = tokio::time::timeout(Duration::from_secs(5), inbox.recv()).await.unwrap().unwrap();
    assert_eq!((got.text(), got.msg_id.as_deref()), ("在吗".to_owned(), Some("MSG1")));
    send(&mut ws, json!({"op": 7})).await;

    // —— 第二次连接：应 Resume 原会话 ——
    let (stream, _) = listener.accept().await.unwrap();
    let mut ws2 = tokio_tungstenite::accept_async(stream).await.unwrap();
    send(&mut ws2, json!({"op": 10, "d": {"heartbeat_interval": 30000}})).await;
    let resume = recv_json(&mut ws2).await;
    assert_eq!(resume["op"], 6);
    assert_eq!((resume["d"]["session_id"].clone(), resume["d"]["seq"].clone()), (json!("S1"), json!(4)));
    assert!(inbox.try_recv().is_err(), "陌生人与重复消息不应进入收件箱");

    // —— 出站：被动回复引用 MSG1，msg_seq 递增；媒体先上传再发 ——
    qq.send(Outbound::text("第一段")).await.unwrap();
    qq.send(Outbound::text("第二段")).await.unwrap();
    let img = dir.join("a.png");
    std::fs::write(&img, b"\x89PNG\r\n\x1a\n").unwrap();
    qq.send(Outbound { text: String::new(), media: vec![img], reply_to: None }).await.unwrap();
    let reqs = http.received_requests().await.unwrap();
    let sent: Vec<Value> = reqs.iter().filter(|r| r.url.path() == "/v2/users/OPENID1/messages").map(|r| serde_json::from_slice(&r.body).unwrap()).collect();
    assert_eq!(sent.len(), 3);
    assert_eq!((sent[0]["msg_id"].clone(), sent[0]["msg_seq"].clone(), sent[0]["content"].clone()), (json!("MSG1"), json!(1), json!("第一段")));
    assert_eq!(sent[1]["msg_seq"], 2);
    assert_eq!((sent[2]["msg_type"].clone(), sent[2]["media"]["file_info"].clone(), sent[2]["msg_seq"].clone()), (json!(7), json!("FI"), json!(3)));
    let upload: Value = serde_json::from_slice(&reqs.iter().find(|r| r.url.path().ends_with("/files")).unwrap().body).unwrap();
    assert_eq!(upload["file_type"], 1);
    assert!(upload["file_data"].as_str().is_some_and(|s| !s.is_empty()));
    let auth = reqs.iter().find(|r| r.url.path().ends_with("/messages")).unwrap().headers.get("authorization").unwrap().to_str().unwrap().to_owned();
    assert_eq!(auth, "QQBot T");

    // —— 致命关闭码：停止重连并返回错误 ——
    ws2.close(Some(CloseFrame { code: 4914u16.into(), reason: "sandbox".into() })).await.unwrap();
    let res = tokio::time::timeout(Duration::from_secs(5), runner).await.expect("致命关闭后应停止").unwrap();
    assert!(res.is_err());
}

#[tokio::test]
async fn proactive_quota_then_defer_until_next_inbound() {
    let http = MockServer::start().await;
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "T", "expires_in": 7200}))).mount(&http).await;
    Mock::given(method("POST")).and(path("/v2/users/OPENID1/messages")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "x"}))).mount(&http).await;
    let cfg = QqConfig {
        app_id: "APP".into(),
        secret: Secret::new("S"),
        allowed_users: vec!["OPENID1".into()],
        api_base: http.uri(),
        token_url: format!("{}/token", http.uri()),
        reply_window_min: 60,
        max_replies: 5,
        proactive_monthly_quota: 1,
        inbox_dir: std::env::temp_dir(),
    };
    let store = Store::in_memory().unwrap();
    let qq = QqPlatform::new(cfg, store.clone());
    // 没有被动窗口：第一条走主动（无 msg_id），第二条额度用尽被延迟
    qq.send(Outbound::text("后台任务完成了")).await.unwrap();
    qq.send(Outbound::text("定时提醒")).await.unwrap();
    let sent: Vec<Value> = http.received_requests().await.unwrap().iter().filter(|r| r.url.path().ends_with("/messages")).map(|r| serde_json::from_slice(&r.body).unwrap()).collect();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].get("msg_id").is_none() && sent[0]["content"] == "后台任务完成了");
    let used: Vec<String> = store.call(|c| {
        let mut st = c.prepare("SELECT value FROM kv WHERE key LIKE 'qq_proactive:%'")?;
        let v = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(v)
    }).await.unwrap();
    assert_eq!(used, vec!["1".to_owned()], "主动消息计数持久化");
    let queued = store.outbox_list("qq").await.unwrap();
    assert_eq!(queued.len(), 1, "被延迟的消息落库");
    assert_eq!(queued[0].payload["text"], "定时提醒");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_survives_restart_and_flushes_on_inbound() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_port = listener.local_addr().unwrap().port();
    let http = MockServer::start().await;
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "T", "expires_in": 7200}))).mount(&http).await;
    Mock::given(method("GET")).and(path("/gateway")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"url": format!("ws://127.0.0.1:{ws_port}")}))).mount(&http).await;
    Mock::given(method("POST")).and(path("/v2/users/OPENID1/messages")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "x"}))).mount(&http).await;
    Mock::given(method("POST")).and(path("/v2/users/OPENID1/files")).respond_with(ResponseTemplate::new(500)).mount(&http).await;
    let cfg = QqConfig {
        app_id: "APP".into(),
        secret: Secret::new("S"),
        allowed_users: vec!["OPENID1".into()],
        api_base: http.uri(),
        token_url: format!("{}/token", http.uri()),
        reply_window_min: 60,
        max_replies: 5,
        proactive_monthly_quota: 0,
        inbox_dir: std::env::temp_dir(),
    };
    let store = Store::in_memory().unwrap();
    // 第一个进程：没有任何通道，两条消息都进发件箱
    let before = QqPlatform::new(cfg.clone(), store.clone());
    before.send(Outbound::text("后台结果一")).await.unwrap();
    before.send(Outbound::text("后台结果二")).await.unwrap();
    drop(before);
    // 再排上：一个已被清理的文件、一个上传会失败的文件、一段文字
    let img = std::env::temp_dir().join(format!("qya-outbox-{}.png", std::process::id()));
    std::fs::write(&img, b"\x89PNG\r\n\x1a\n").unwrap();
    store.outbox_push("qq", json!({"media": "/nonexistent/gone.png"}), 50).await.unwrap();
    store.outbox_push("qq", json!({"media": img.display().to_string()}), 50).await.unwrap();
    store.outbox_push("qq", json!({"text": "后台结果三"}), 50).await.unwrap();
    assert_eq!(store.outbox_list("qq").await.unwrap().len(), 5);

    // “重启”后的进程：阿颜说话打开被动窗口，后台按顺序合并补发
    let qq = Arc::new(QqPlatform::new(cfg, store.clone()));
    let (tx, mut inbox) = mpsc::channel(8);
    let runner = {
        let qq = qq.clone();
        tokio::spawn(async move { qq.run(tx).await })
    };
    let (stream, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
    send(&mut ws, json!({"op": 10, "d": {"heartbeat_interval": 30000}})).await;
    assert_eq!(recv_json(&mut ws).await["op"], 2);
    send(&mut ws, c2c(1, "MSG9", "OPENID1", "我回来了")).await;
    tokio::time::timeout(Duration::from_secs(5), inbox.recv()).await.unwrap().unwrap();
    let mut sent = Vec::new();
    for _ in 0..50 {
        sent = http.received_requests().await.unwrap().iter().filter(|r| r.url.path().ends_with("/messages")).map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()).collect();
        if !sent.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(sent.len(), 1, "相邻文字合并为一条补发");
    assert_eq!((sent[0]["msg_id"].clone(), sent[0]["content"].clone()), (json!("MSG9"), json!("后台结果一

后台结果二")));
    // 已发出的文字与已不存在的文件被确认删除；上传失败的文件及其后的文字原样保留到下次
    let mut left = Vec::new();
    for _ in 0..50 {
        left = store.outbox_list("qq").await.unwrap();
        if left.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(left[0].payload["media"].as_str().unwrap().ends_with(".png") && left[1].payload["text"] == "后台结果三");
    runner.abort();
}
