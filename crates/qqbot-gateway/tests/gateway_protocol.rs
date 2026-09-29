//! 网关协议测试：用本地 mock WebSocket 服务验证 Identify / Heartbeat / **Resume** / 分片。
//!
//! 这是 P0 中此前唯一没有测试覆盖的部分——尤其 Resume：
//! 断线后必须带着 `session_id` 与最新的 `seq` 发送 op 6，否则会漏掉断线期间的事件。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use qqbot_api::{ApiClient, ApiClientConfig};
use qqbot_gateway::{spawn_gateway, GatewayConfig};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

// ------------------------------------------------------------ 记录到的上行帧

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Identify { shard: [u32; 2], intents: u32, token: String },
    Resume { session_id: String, seq: i64, token: String },
    Heartbeat { seq: Option<i64> },
}

// ------------------------------------------------------------ mock HTTP（取 token）

async fn serve_http(listener: TcpListener) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else { break };
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let body = r#"{"access_token":"MOCK_TOKEN","expires_in":"7200"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.flush().await;
        });
    }
}

// ------------------------------------------------------------ mock WebSocket 网关

struct WsMock {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl WsMock {
    fn url(&self) -> String {
        format!("ws://{}/websocket", self.addr)
    }

    fn snapshot(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    async fn wait_for(&self, pred: impl Fn(&Seen) -> bool) -> Seen {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(found) = self.snapshot().into_iter().find(|s| pred(s)) {
                return found;
            }
            if Instant::now() > deadline {
                panic!("等待超时。已观察到: {:#?}", self.snapshot());
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
}

async fn send_json<S>(ws: &mut WebSocketStream<S>, value: serde_json::Value)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let _ = ws.send(Message::text(value.to_string())).await;
}

/// `drop_after_ready` 指定「第几个连接在发送 READY 后立刻断开」，用于触发客户端 Resume。
/// mock 服务端行为开关，用于构造「断线」与「会话失效」两种场景。
#[derive(Clone, Copy, Default)]
struct Behavior {
    /// 该序号连接在发送 READY 后立刻断开 → 触发客户端 Resume
    drop_after_ready: Option<usize>,
    /// 该序号连接收到 Identify 后回 op 9 → 触发客户端重新 Identify
    invalid_session: Option<usize>,
    /// 发送 READY 后先推一个**解析不了**的事件，再推一个正常事件
    malformed_then_valid: bool,
}

impl Behavior {
    fn none() -> Self {
        Self::default()
    }
    fn drop_after_ready(idx: usize) -> Self {
        Self { drop_after_ready: Some(idx), ..Self::default() }
    }
    fn invalid_session(idx: usize) -> Self {
        Self { invalid_session: Some(idx), ..Self::default() }
    }
    fn malformed_then_valid() -> Self {
        Self { malformed_then_valid: true, ..Self::default() }
    }
}

async fn serve_ws(
    listener: TcpListener,
    seen: Arc<Mutex<Vec<Seen>>>,
    conns: Arc<AtomicUsize>,
    behavior: Behavior,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { break };
        let seen = seen.clone();
        let conns = conns.clone();

        tokio::spawn(async move {
            let idx = conns.fetch_add(1, Ordering::SeqCst);
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { return };

            // 连接建立后的第一条必须是 Hello
            send_json(&mut ws, json!({"op": 10, "d": {"heartbeat_interval": 1000}})).await;

            while let Some(Ok(msg)) = ws.next().await {
                let Message::Text(text) = msg else { continue };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(text.as_str()) else { continue };

                match v["op"].as_u64() {
                    Some(2) => {
                        let shard = [
                            v["d"]["shard"][0].as_u64().unwrap_or(0) as u32,
                            v["d"]["shard"][1].as_u64().unwrap_or(0) as u32,
                        ];
                        seen.lock().unwrap().push(Seen::Identify {
                            shard,
                            intents: v["d"]["intents"].as_u64().unwrap_or(0) as u32,
                            token: v["d"]["token"].as_str().unwrap_or_default().to_string(),
                        });

                        if behavior.invalid_session == Some(idx) {
                            // 服务端判定 session 无效：客户端必须清空 session 并重新 Identify
                            send_json(&mut ws, json!({"op": 9})).await;
                            continue;
                        }

                        send_json(&mut ws, json!({
                            "op": 0, "s": 1, "t": "READY",
                            "d": {
                                "version": 1,
                                "session_id": "SESS-1",
                                "user": {"id": "1", "username": "MockBot", "bot": true},
                                "shard": shard
                            }
                        }))
                        .await;

                        if behavior.malformed_then_valid {
                            // id 必须是字符串，这里给整数 → 客户端解析必然失败
                            send_json(
                                &mut ws,
                                json!({"op": 0, "s": 2, "t": "C2C_MESSAGE_CREATE",
                                       "d": {"id": 12345}}),
                            )
                            .await;
                            // 紧接着一个完全合法的事件：连接若被拖垮，它就收不到
                            send_json(
                                &mut ws,
                                json!({"op": 0, "s": 3, "t": "C2C_MESSAGE_CREATE",
                                       "d": {"id": "OK1", "author": {"user_openid": "U1"},
                                             "content": "survived"}}),
                            )
                            .await;
                        }

                        if behavior.drop_after_ready == Some(idx) {
                            // 模拟网络中断：直接关闭，迫使客户端走 Resume 分支
                            let _ = ws.close(None).await;
                            break;
                        }
                    }
                    Some(6) => {
                        seen.lock().unwrap().push(Seen::Resume {
                            session_id: v["d"]["session_id"].as_str().unwrap_or_default().to_string(),
                            seq: v["d"]["seq"].as_i64().unwrap_or(0),
                            token: v["d"]["token"].as_str().unwrap_or_default().to_string(),
                        });
                        send_json(&mut ws, json!({"op": 0, "s": 100, "t": "RESUMED", "d": ""})).await;
                    }
                    Some(1) => {
                        seen.lock().unwrap().push(Seen::Heartbeat { seq: v["d"].as_i64() });
                        send_json(&mut ws, json!({"op": 11})).await;
                    }
                    _ => {}
                }
            }
        });
    }
}

async fn start_mock(behavior: Behavior) -> (WsMock, String) {
    let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();
    tokio::spawn(serve_http(http_listener));

    let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = ws_listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let conns = Arc::new(AtomicUsize::new(0));
    tokio::spawn(serve_ws(ws_listener, seen.clone(), conns, behavior));

    (WsMock { addr: ws_addr, seen }, format!("http://{http_addr}"))
}

fn api_for(base_url: &str) -> ApiClient {
    ApiClient::new(ApiClientConfig {
        app_id: "mock-app".to_string(),
        client_secret: "mock-secret".to_string(),
        base_url: base_url.to_string(),
    })
    .unwrap()
}

// ------------------------------------------------------------ 测试

#[tokio::test]
async fn identify_ready_and_heartbeat() {
    let (ws, base) = start_mock(Behavior::none()).await;
    let mut gateway = spawn_gateway(
        api_for(&base),
        GatewayConfig::default().with_url(ws.url()),
    )
    .await
    .unwrap();

    // READY 应作为事件冒泡到上层
    let event = tokio::time::timeout(Duration::from_secs(15), gateway.recv())
        .await
        .expect("等待 READY 超时")
        .expect("事件通道关闭");
    assert_eq!(event.name(), "READY");

    // Identify 的 token 必须带 QQBot 前缀（漏掉会被服务端以 op 9 拒绝）
    let identify = ws
        .wait_for(|s| matches!(s, Seen::Identify { .. }))
        .await;
    match identify {
        Seen::Identify { shard, intents, token } => {
            assert_eq!(shard, [0, 1], "单分片时应为 [0, 1]");
            assert_eq!(token, "QQBot MOCK_TOKEN", "Identify token 必须带 QQBot 前缀");
            assert_eq!(intents, qqbot_api::DEFAULT_INTENTS.bits());
        }
        other => panic!("期望 Identify，实际 {other:?}"),
    }

    // 心跳应携带最近一次收到的 seq
    let heartbeat = ws.wait_for(|s| matches!(s, Seen::Heartbeat { .. })).await;
    match heartbeat {
        Seen::Heartbeat { seq } => assert_eq!(seq, Some(1), "心跳 d 应为最近收到的 s"),
        other => panic!("期望 Heartbeat，实际 {other:?}"),
    }

    gateway.shutdown().await;
}

/// P0 的核心：断线后必须用 op 6 Resume 续上，而不是重新 Identify。
#[tokio::test]
async fn resumes_session_after_disconnect() {
    // 第 0 个连接在 READY 后立刻断开
    let (ws, base) = start_mock(Behavior::drop_after_ready(0)).await;
    let gateway = spawn_gateway(
        api_for(&base),
        GatewayConfig::default().with_url(ws.url()),
    )
    .await
    .unwrap();

    let resume = ws.wait_for(|s| matches!(s, Seen::Resume { .. })).await;
    match resume {
        Seen::Resume { session_id, seq, token } => {
            assert_eq!(session_id, "SESS-1", "Resume 必须携带上次的 session_id");
            assert_eq!(seq, 1, "Resume 必须携带最新的 seq，否则会漏事件");
            assert_eq!(token, "QQBot MOCK_TOKEN");
        }
        other => panic!("期望 Resume，实际 {other:?}"),
    }

    // 断线重连后不应再发一次 Identify（否则 session 会被重置、事件丢失）
    let identifies = ws.snapshot().iter().filter(|s| matches!(s, Seen::Identify { .. })).count();
    assert_eq!(identifies, 1, "Resume 成功后不应重复 Identify");

    gateway.shutdown().await;
}

#[tokio::test]
async fn sharding_opens_one_connection_per_shard() {
    let (ws, base) = start_mock(Behavior::none()).await;
    let gateway = spawn_gateway(
        api_for(&base),
        GatewayConfig::default().with_url(ws.url()).with_shards(2),
    )
    .await
    .unwrap();

    assert_eq!(gateway.shard_count(), 2);

    ws.wait_for(|s| matches!(s, Seen::Identify { shard, .. } if *shard == [0, 2])).await;
    ws.wait_for(|s| matches!(s, Seen::Identify { shard, .. } if *shard == [1, 2])).await;

    let shards: Vec<[u32; 2]> = ws
        .snapshot()
        .into_iter()
        .filter_map(|s| match s {
            Seen::Identify { shard, .. } => Some(shard),
            _ => None,
        })
        .collect();
    assert!(shards.contains(&[0, 2]), "缺少分片 [0,2]: {shards:?}");
    assert!(shards.contains(&[1, 2]), "缺少分片 [1,2]: {shards:?}");

    gateway.shutdown().await;
}

/// 单个事件解析失败**不能**拖垮长连接——否则一个字段的类型差异就会导致
/// 断线重连、期间事件全部丢失。线上确实遇到过（timestamp 下发成整数）。
#[tokio::test]
async fn malformed_event_is_skipped_without_dropping_connection() {
    let (ws, base) = start_mock(Behavior::malformed_then_valid()).await;
    let mut gateway = spawn_gateway(
        api_for(&base),
        GatewayConfig::default().with_url(ws.url()),
    )
    .await
    .unwrap();

    // 第一个事件是 READY，第二个必须是坏事件之后那个合法事件
    let mut seen = Vec::new();
    for _ in 0..2 {
        let ev = tokio::time::timeout(Duration::from_secs(15), gateway.recv())
            .await
            .expect("等待事件超时")
            .expect("事件通道关闭");
        seen.push(ev);
    }

    assert_eq!(seen[0].name(), "READY");
    let msg = seen[1].as_message().expect("第二个应是消息事件");
    assert_eq!(msg.content, "survived", "坏事件之后的合法事件必须仍能送达");

    gateway.shutdown().await;
}

/// op 9 InvalidSession：必须清空 session 并重新 Identify，而不是继续 Resume。
#[tokio::test]
async fn invalid_session_forces_reidentify() {
    let (ws, base) = start_mock(Behavior::invalid_session(0)).await;
    let gateway = spawn_gateway(
        api_for(&base),
        GatewayConfig::default().with_url(ws.url()),
    )
    .await
    .unwrap();

    // 第 0 个连接被回以 op 9，客户端重连后应重新 Identify（共 2 次）
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let identifies = ws
            .snapshot()
            .iter()
            .filter(|s| matches!(s, Seen::Identify { .. }))
            .count();
        if identifies >= 2 {
            break;
        }
        assert!(Instant::now() < deadline, "未在超时内重新 Identify: {:#?}", ws.snapshot());
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    let resumes = ws
        .snapshot()
        .iter()
        .filter(|s| matches!(s, Seen::Resume { .. }))
        .count();
    assert_eq!(resumes, 0, "session 被判无效后不应尝试 Resume");

    gateway.shutdown().await;
}
