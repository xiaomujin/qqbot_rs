//! 端到端集成测试：用本地 mock HTTP 服务器跑通完整链路。
//!
//! 验证范围（**不需要真实凭据，不触碰线上**）：
//!   access_token → 事件去重 → 被动窗口登记 → 路由 → 插件
//!   → 渲染 PNG → 分片上传 → 合并 → 发送消息
//!
//! 断言落在「mock 服务器实际收到的 HTTP 请求」上，因此覆盖了序列化、
//! 鉴权头、分片规划、msg_seq 分配等真实细节。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use qqbot_api::{ApiClient, ApiClientConfig, Event, Target};
use qqbot_core::{CoreError, DispatchConfig, Dispatcher, Router, SendRequest, Services, SessionRegistry};
use qqbot_media::MediaUploader;
use qqbot_render::{RenderConfig, RenderService};
use qqbot_store::{MessageStore, Scope, StoreConfig};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ---------------------------------------------------------------- mock server

#[derive(Debug, Clone)]
struct Hit {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

struct MockInner {
    hits: Mutex<Vec<Hit>>,
    /// 已收到的发消息请求数（用于模拟「前 N 次失败」）。
    sends: AtomicUsize,
    /// 前多少次发消息请求返回 err_code 40034005（被动窗口已过期）。
    fail_first_sends: usize,
}

#[derive(Clone)]
struct MockServer {
    addr: SocketAddr,
    inner: Arc<MockInner>,
}

impl MockServer {
    async fn start() -> Self {
        Self::builder().start().await
    }

    fn builder() -> MockBuilder {
        MockBuilder { fail_first_sends: 0 }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn hits(&self) -> Vec<Hit> {
        self.inner.hits.lock().unwrap().clone()
    }

    fn find(&self, predicate: impl Fn(&Hit) -> bool) -> Option<Hit> {
        self.hits().into_iter().find(|h| predicate(h))
    }

    fn all(&self, predicate: impl Fn(&Hit) -> bool) -> Vec<Hit> {
        self.hits().into_iter().filter(|h| predicate(h)).collect()
    }
}

struct MockBuilder {
    fail_first_sends: usize,
}

impl MockBuilder {
    fn fail_first_sends(mut self, n: usize) -> Self {
        self.fail_first_sends = n;
        self
    }

    async fn start(self) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock server");
        let addr = listener.local_addr().unwrap();
        let inner = Arc::new(MockInner {
            hits: Mutex::new(Vec::new()),
            sends: AtomicUsize::new(0),
            fail_first_sends: self.fail_first_sends,
        });

        let inner_for_task = inner.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                let inner = inner_for_task.clone();
                tokio::spawn(async move { handle_conn(stream, inner, addr).await });
            }
        });

        MockServer { addr, inner }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn handle_conn(mut stream: tokio::net::TcpStream, inner: Arc<MockInner>, addr: SocketAddr) {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 8192];

    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
        if buf.len() > 256 * 1024 {
            return;
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let header_value = |name: &str| -> Option<String> {
        head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    };

    let content_length: usize = header_value("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
        }
    }
    let body = String::from_utf8_lossy(&body[..body.len().min(content_length)]).to_string();

    inner.hits.lock().unwrap().push(Hit {
        method: method.clone(),
        path: path.clone(),
        authorization: header_value("authorization"),
        body: body.clone(),
    });

    let (status, payload) = route(&method, &path, addr, &inner);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

fn route(method: &str, path: &str, addr: SocketAddr, inner: &MockInner) -> (&'static str, String) {
    if path.contains("getAppAccessToken") {
        return ("200 OK", json!({"access_token": "MOCK_TOKEN", "expires_in": "7200"}).to_string());
    }
    if path.contains("/gateway/bot") {
        return (
            "200 OK",
            json!({"url": format!("ws://{addr}/websocket"), "shards": 1}).to_string(),
        );
    }
    if path.contains("/upload_prepare") {
        return (
            "200 OK",
            json!({
                "upload_id": "UP1",
                "block_size": "5242880",
                "parts": [{
                    "index": 0,
                    "presigned_url": format!("http://{addr}/presigned/0"),
                    "block_size": "5242880"
                }],
                "upload_config": {"concurrency": 1, "retry_timeout": 300, "retry_delay": 1}
            })
            .to_string(),
        );
    }
    if path.starts_with("/presigned/") {
        return ("200 OK", String::new());
    }
    // 资源收录会去下载附件。内容无所谓 —— 插件只负责把字节落盘。
    if path.starts_with("/test-image") {
        return ("200 OK", "FAKE-IMAGE-BYTES".to_string());
    }
    if path.contains("/upload_part_finish") {
        return ("200 OK", "{}".to_string());
    }
    if path.ends_with("/files") {
        return ("200 OK", json!({"file_info": "MOCK_FILE_INFO", "ttl": 3600}).to_string());
    }
    if path.ends_with("/messages") {
        let nth = inner.sends.fetch_add(1, Ordering::SeqCst);
        if nth < inner.fail_first_sends {
            // 官方错误码 40034005：回复消息 msg_id 已过期
            return (
                "200 OK",
                json!({"err_code": 40034005, "message": "回复消息msg_id已过期"}).to_string(),
            );
        }
        return (
            "200 OK",
            json!({"id": "OUT_MSG", "timestamp": "2026-09-28T10:00:00+08:00"}).to_string(),
        );
    }
    let _ = method;
    ("404 Not Found", json!({"err_code": 10001, "message": "unknown path"}).to_string())
}

// ---------------------------------------------------------------- harness

async fn build_stack(mock: &MockServer) -> Arc<Dispatcher> {
    build_stack_with(mock, None, None).await
}

async fn build_stack_with(
    mock: &MockServer,
    store: Option<Arc<MessageStore>>,
    resources: Option<qqbot_plugins::ResourcesConfig>,
) -> Arc<Dispatcher> {
    let cfg = ApiClientConfig {
        app_id: "mock-app".to_string(),
        client_secret: "mock-secret".to_string(),
        base_url: mock.base_url(),
    };
    let api = ApiClient::new(cfg).expect("api client");
    let media = MediaUploader::new(api.clone());
    let render = RenderService::new(RenderConfig {
        timeout: Duration::from_secs(10),
        ..RenderConfig::default()
    });
    let sessions = Arc::new(SessionRegistry::new(api.clone(), 2, 64, Duration::from_secs(10)));

    let mut services = Services::new(api, media, render, sessions);
    if let Some(s) = &store {
        services = services.with_store(Arc::clone(s));
    }
    let services = Arc::new(services);

    let mut router = Router::new();
    qqbot_plugins::register(
        &mut router,
        store,
        &qqbot_plugins::PluginsConfig {
            wordcloud_window: Duration::from_secs(30 * 24 * 3600),
            daily: None,
            resources,
        },
    )
    .await
    .expect("初始化插件失败");

    Arc::new(Dispatcher::new(router, services, DispatchConfig::default()))
}

fn event(name: &str, payload: &str) -> Arc<Event> {
    let raw: Box<serde_json::value::RawValue> = serde_json::from_str(payload).unwrap();
    Arc::new(Event::parse(Some(name), Some(&raw), None).unwrap().unwrap())
}

async fn feed(dispatcher: &Arc<Dispatcher>, name: &str, payload: &str) {
    let handle = dispatcher.handle(event(name, payload));
    tokio::time::timeout(Duration::from_secs(20), handle)
        .await
        .expect("事件处理超时");
}

// ---------------------------------------------------------------- tests

/// 入站消息必须落库，**群聊与单聊都要**，且按会话隔离。
#[tokio::test]
async fn inbound_messages_are_persisted_for_both_scopes() {
    let mock = MockServer::start().await;
    let dir = std::env::temp_dir().join(format!("qqbot-e2e-store-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = MessageStore::open(StoreConfig {
        path: dir.join("qqbot.db"),
        flush_interval: Duration::from_millis(20),
        ..StoreConfig::default()
    })
    .unwrap();

    let dispatcher = build_stack_with(&mock, Some(Arc::clone(&store)), None).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"P_G1","author":{"member_openid":"U1","username":"小明"},"content":"群里说的话","group_openid":"GP"}"#,
    )
    .await;
    feed(
        &dispatcher,
        "C2C_MESSAGE_CREATE",
        r#"{"id":"P_C1","author":{"user_openid":"U2","username":"小红"},"content":"私聊说的话"}"#,
    )
    .await;

    // 写线程是异步批提交，轮询等它落盘
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.count().await.unwrap() < 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        store.recent_texts(Scope::Group, "GP", 0, 10).await.unwrap(),
        vec!["群里说的话"]
    );
    assert_eq!(
        store.recent_texts(Scope::C2c, "U2", 0, 10).await.unwrap(),
        vec!["私聊说的话"]
    );
    // 群与私聊不能串台
    assert!(store.recent_texts(Scope::C2c, "GP", 0, 10).await.unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 纯文本路径：ping 用 reply_text，必须是 msg_type=0 且内容在 content 字段。
#[tokio::test]
async fn text_reply_carries_msg_id_and_incremented_msg_seq() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_AT_MESSAGE_CREATE",
        r#"{"id":"IN_1","author":{"member_openid":"U1","username":"小明"},"content":"ping","group_openid":"G1"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.method == "POST" && h.path == "/v2/groups/G1/messages")
        .expect("应向群 G1 发送消息");

    assert_eq!(
        send.authorization.as_deref(),
        Some("QQBot MOCK_TOKEN"),
        "Authorization 头格式错误"
    );

    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    assert_eq!(body["msg_type"], 0, "文本消息 msg_type 应为 0");
    assert_eq!(body["msg_id"], "IN_1", "被动回复必须携带 msg_id");
    assert_eq!(body["msg_seq"], 1, "首个被动回复 msg_seq 应为 1");
    assert!(body["content"].as_str().unwrap().contains("pong"));
    assert!(body.get("markdown").is_none(), "文本消息不应带 markdown 字段");
}

/// Markdown 路径：骰子必须用 msg_type=2，否则 `**加粗**` 在客户端不会渲染。
#[tokio::test]
async fn dice_reply_uses_markdown_so_bold_is_rendered() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_AT_MESSAGE_CREATE",
        r#"{"id":"IN_MD","author":{"member_openid":"U1"},"content":"骰子 2d6","group_openid":"GMD"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GMD/messages")
        .expect("应回复骰子");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();

    assert_eq!(body["msg_type"], 2, "骰子必须用 Markdown（msg_type=2）");
    assert!(
        body.get("content").is_none() || body["content"].is_null(),
        "Markdown 消息不应走 content 字段: {body}"
    );
    let md = body["markdown"]["content"].as_str().expect("缺少 markdown.content");
    assert!(md.contains("2d6"), "应回显骰子规格: {md}");
    assert!(md.contains("**"), "应保留加粗标记: {md}");
}
/// 全量模式回归：`GROUP_MESSAGE_CREATE` **不剥离** @机器人 前缀，
/// content 是 `<@openid> 骰子 2d6`。
///
/// 修复前这条会静默失败 —— 裸关键词正常、@机器人 却毫无反应，
/// 症状很像权限或订阅问题，实际是匹配串里多了个 `<@...>`。
#[tokio::test]
async fn full_mode_at_prefix_still_routes() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"IN_AT","author":{"member_openid":"U1"},"content":"<@0F1E2D3C4B5A69788796A5B4C3D2E1F0> 骰子 2d6","group_openid":"GAT"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GAT/messages")
        .expect("@机器人 的命令必须能触发（全量模式不剥离 @ 前缀）");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    assert_eq!(body["msg_type"], 2, "应当走骰子的 Markdown 路径");
    assert_eq!(body["msg_id"], "IN_AT", "关键词触发必须走被动回复");
}

/// 全量模式下的裸关键词（没有 @）同样要触发，且走被动回复。
#[tokio::test]
async fn full_mode_bare_keyword_routes_passively() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"IN_BARE","author":{"member_openid":"U1"},"content":"骰子 2d6","group_openid":"GBARE"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GBARE/messages")
        .expect("裸关键词必须能触发");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    assert_eq!(body["msg_id"], "IN_BARE", "必须是被动回复而不是主动消息");
}

#[tokio::test]
async fn repeated_replies_increment_msg_seq() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_AT_MESSAGE_CREATE",
        r#"{"id":"IN_9","author":{"member_openid":"U1"},"content":"骰子 1d6","group_openid":"G9"}"#,
    )
    .await;
    feed(
        &dispatcher,
        "GROUP_AT_MESSAGE_CREATE",
        r#"{"id":"IN_9","author":{"member_openid":"U1"},"content":"骰子 1d6","group_openid":"G9"}"#,
    )
    .await;

    let sends = mock.all(|h| h.path == "/v2/groups/G9/messages");
    assert_eq!(sends.len(), 1, "相同 msg_id 的事件必须被去重");

    feed(
        &dispatcher,
        "GROUP_AT_MESSAGE_CREATE",
        r#"{"id":"IN_10","author":{"member_openid":"U1"},"content":"骰子 1d6","group_openid":"G9"}"#,
    )
    .await;

    let seqs: Vec<u64> = mock
        .all(|h| h.path == "/v2/groups/G9/messages")
        .iter()
        .map(|h| {
            serde_json::from_str::<serde_json::Value>(&h.body).unwrap()["msg_seq"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(seqs, vec![1, 2], "msg_seq 必须严格递增: {seqs:?}");
}

#[tokio::test]
async fn c2c_reply_targets_user_endpoint() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "C2C_MESSAGE_CREATE",
        r#"{"id":"IN_2","author":{"user_openid":"U2","username":"小红"},"content":"ping"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.method == "POST" && h.path == "/v2/users/U2/messages")
        .expect("应向单聊用户 U2 发送消息");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    assert_eq!(body["msg_id"], "IN_2");
    assert!(body["content"].as_str().unwrap().contains("pong"));
}

/// 全链路：收到「词云」→ 渲染 PNG → 分片上传 → 合并 → 以 msg_type=7 发送图片。
#[tokio::test]
async fn wordcloud_command_renders_uploads_and_sends_image() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    // 语料要选 jieba 能切出 >= MIN_DISTINCT(3) 个实词的句子。
    // 「今天天气不错」只会切成 今天天气/不错 两个词，达不到门槛。
    for i in 0..3 {
        feed(
            &dispatcher,
            "GROUP_AT_MESSAGE_CREATE",
            &format!(
                r#"{{"id":"SEED_{i}","author":{{"member_openid":"U1"}},"content":"群里的大家早上好","group_openid":"GW"}}"#
            ),
        )
        .await;
    }

    feed(
        &dispatcher,
        "GROUP_AT_MESSAGE_CREATE",
        r#"{"id":"WC_1","author":{"member_openid":"U1","username":"小明"},"content":"词云","group_openid":"GW"}"#,
    )
    .await;

    let prepare = mock
        .find(|h| h.method == "POST" && h.path == "/v2/groups/GW/upload_prepare")
        .expect("应调用群聊富媒体预上传");
    let prep_body: serde_json::Value = serde_json::from_str(&prepare.body).unwrap();
    assert_eq!(prep_body["file_type"], 1, "图片 file_type 应为 1");
    for key in ["file_size", "file_name", "md5", "sha1", "md5_10m"] {
        assert!(prep_body.get(key).is_some(), "预上传缺少字段 {key}: {prep_body}");
    }
    assert_eq!(prep_body["md5"].as_str().unwrap().len(), 32);
    assert_eq!(prep_body["sha1"].as_str().unwrap().len(), 40);

    let put = mock
        .find(|h| h.method == "PUT" && h.path.starts_with("/presigned/"))
        .expect("应向预签名 URL PUT 分片");
    assert!(
        put.authorization.is_none(),
        "预签名 URL 不能携带 Authorization 头（会破坏对象存储签名）"
    );

    assert!(
        mock.find(|h| h.path == "/v2/groups/GW/upload_part_finish").is_some(),
        "应调用分片完成接口"
    );

    let merge = mock
        .find(|h| h.method == "POST" && h.path == "/v2/groups/GW/files")
        .expect("应调用上传合并接口");
    let merge_body: serde_json::Value = serde_json::from_str(&merge.body).unwrap();
    assert_eq!(merge_body["upload_id"], "UP1");
    assert_eq!(
        merge_body["srv_send_msg"], false,
        "必须显式声明 srv_send_msg=false，否则可能被当成「上传即发送」而占用主动消息频次"
    );

    let send = mock
        .find(|h| h.method == "POST" && h.path == "/v2/groups/GW/messages")
        .expect("应发送富媒体消息");
    let send_body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    assert_eq!(send_body["msg_type"], 7, "图片消息 msg_type 应为 7");
    assert_eq!(send_body["media"]["file_info"], "MOCK_FILE_INFO");
    assert_eq!(send_body["msg_id"], "WC_1");
    assert_eq!(send_body["msg_seq"], 1);

    // 秒传：同样的图片再次上传应命中缓存
    let before = mock.all(|h| h.path == "/v2/groups/GW/upload_prepare").len();
    feed(
        &dispatcher,
        "GROUP_AT_MESSAGE_CREATE",
        r#"{"id":"WC_2","author":{"member_openid":"U1","username":"小明"},"content":"词云","group_openid":"GW"}"#,
    )
    .await;
    let after = mock.all(|h| h.path == "/v2/groups/GW/upload_prepare").len();
    assert_eq!(before, after, "相同图片应命中 file_info 缓存（秒传）");

    let sends = mock.all(|h| h.path == "/v2/groups/GW/messages");
    assert_eq!(sends.len(), 2, "两次词云都应发送图片");
    let last: serde_json::Value = serde_json::from_str(&sends[1].body).unwrap();
    assert_eq!(last["msg_type"], 7);
    assert_eq!(last["media"]["file_info"], "MOCK_FILE_INFO");
    assert_eq!(last["msg_id"], "WC_2");
    assert_eq!(last["msg_seq"], 2, "同一会话 msg_seq 应持续递增");
}

#[tokio::test]
async fn help_lists_registered_routes() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_AT_MESSAGE_CREATE",
        r#"{"id":"H1","author":{"member_openid":"U1"},"content":"帮助","group_openid":"GH"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GH/messages")
        .expect("应回复帮助");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    assert_eq!(body["msg_type"], 2, "帮助使用 Markdown (msg_type=2)");
    let content = body["markdown"]["content"].as_str().unwrap();
    assert!(content.contains("骰子"), "帮助应包含已注册指令: {content}");
    assert!(content.contains("词云"));
}

#[tokio::test]
async fn unknown_event_is_ignored_without_side_effects() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(&dispatcher, "SOME_FUTURE_EVENT", r#"{"whatever":1}"#).await;

    let sends = mock.all(|h| h.path.ends_with("/messages"));
    assert!(sends.is_empty(), "未知事件不应触发发送");
}

/// 服务端判定被动窗口已过期（err_code 40034005）时，必须自动降级为主动消息重试一次。
#[tokio::test]
async fn expired_passive_window_falls_back_to_active_message() {
    let mock = MockServer::builder().fail_first_sends(1).start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_AT_MESSAGE_CREATE",
        r#"{"id":"E1","author":{"member_openid":"U1"},"content":"ping","group_openid":"GE"}"#,
    )
    .await;

    let sends = mock.all(|h| h.path == "/v2/groups/GE/messages");
    assert_eq!(sends.len(), 2, "首次被动发送失败后应自动重试一次");

    let first: serde_json::Value = serde_json::from_str(&sends[0].body).unwrap();
    assert_eq!(first["msg_id"], "E1", "首次应走被动回复");
    assert_eq!(first["msg_seq"], 1);

    let retry: serde_json::Value = serde_json::from_str(&sends[1].body).unwrap();
    assert!(
        retry.get("msg_id").is_none(),
        "重试必须改走主动消息（不带 msg_id）: {retry}"
    );
    assert!(retry.get("msg_seq").is_none(), "主动消息不应带 msg_seq");
    assert!(retry["content"].as_str().unwrap().contains("pong"));
}

/// 事件类被动回复：必须携带 `event_id`，且不带 `msg_id` / `msg_seq`。
///
/// 官方：「被动消息（响应事件）携带 event_id」——用于机器人被拉群、按钮回调等场景。
#[tokio::test]
async fn event_reply_carries_event_id_instead_of_msg_id() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;
    let services = dispatcher.services().clone();

    services
        .send(
            SendRequest::text(Target::group("GEV"), "欢迎使用")
                .responding_to_event("EVENT-777"),
        )
        .await
        .expect("事件回复应发送成功");

    let send = mock
        .find(|h| h.path == "/v2/groups/GEV/messages")
        .expect("应发送事件回复");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();

    assert_eq!(body["event_id"], "EVENT-777", "事件回复必须携带 event_id");
    assert!(body.get("msg_id").is_none(), "事件回复不应带 msg_id: {body}");
    assert!(body.get("msg_seq").is_none(), "事件回复不应带 msg_seq: {body}");
    assert_eq!(body["msg_type"], 0);
}

/// 本地主动消息配额必须前置拦截，被拦下的请求不应产生任何线上调用。
#[tokio::test]
async fn active_quota_is_enforced_before_hitting_the_api() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;
    let services = dispatcher.services().clone();

    let target = Target::group("GQ");
    let mut accepted = 0usize;
    let mut rejected = 0usize;
    for _ in 0..25 {
        match services.send(SendRequest::text(target.clone(), "hi")).await {
            Ok(_) => accepted += 1,
            Err(CoreError::QuotaExceeded) => rejected += 1,
            Err(err) => panic!("意外错误: {err}"),
        }
    }

    assert_eq!(accepted, 20, "单关系维度上限为 20 条/分钟");
    assert_eq!(rejected, 5);

    let sends = mock.all(|h| h.path == "/v2/groups/GQ/messages");
    assert_eq!(sends.len(), 20, "被本地配额拦截的请求不应发到线上");
}

// ------------------------------------------------------------ 资源管理

/// 造一个带资源库的栈，并放一条群资源。返回 (dispatcher, 临时目录)。
async fn stack_with_resource(
    mock: &MockServer,
    group: &str,
    keyword: &str,
    scope: qqbot_store::ResourceScope,
    tag: &str,
) -> (Arc<Dispatcher>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("qqbot-res-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");

    let store = Arc::new(
        qqbot_store::open_resource_store(dir.join("res.db"))
            .await
            .expect("打开资源库"),
    );

    // 内容无所谓：测试断言的是「读到了文件并作为富媒体发出」。
    let img = dir.join("map.png");
    std::fs::write(&img, b"\x89PNG\r\n\x1a\nfake-image").expect("写素材");

    let owner = match scope {
        qqbot_store::ResourceScope::Group => group.to_string(),
        qqbot_store::ResourceScope::System => qqbot_store::SYSTEM_OWNER.to_string(),
    };
    store
        .upsert(qqbot_store::ResourceSpec {
            scope,
            owner_id: owner,
            name: keyword.to_string(),
            path: img,
            file_name: "map.png".to_string(),
            file_type: 1,
            description: None,
        })
        .await
        .expect("写入资源");

    // 资源管理依赖消息库（收录时要回查上一条带图片的消息），所以两个都要建。
    let messages = MessageStore::open_async(StoreConfig {
        path: dir.join("msg.db"),
        flush_interval: Duration::from_millis(10),
        sweep_interval: Duration::from_secs(3600),
        ..StoreConfig::default()
    })
    .await
    .expect("打开消息库");

    let cfg = qqbot_plugins::ResourcesConfig {
        store,
        messages: Arc::clone(&messages),
        basepath: dir.join("collected"),
        controllers: None,
    };
    (build_stack_with(mock, Some(messages), Some(cfg)).await, dir)
}

/// 关键词触发：应当读到磁盘上的文件、上传富媒体、并走**被动回复**。
#[tokio::test]
async fn resource_keyword_sends_the_file_passively() {
    let mock = MockServer::start().await;
    let (dispatcher, dir) =
        stack_with_resource(&mock, "GRES", "地图", qqbot_store::ResourceScope::Group, "send").await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"IN_RES","author":{"member_openid":"U1"},"content":"地图","group_openid":"GRES"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.method == "POST" && h.path == "/v2/groups/GRES/messages")
        .expect("应当发出资源消息");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    assert_eq!(body["msg_type"], 7, "资源必须以富媒体发送");
    assert_eq!(body["msg_id"], "IN_RES", "关键词触发必须走被动回复");
    assert!(body["media"]["file_info"].is_string(), "缺少 file_info: {body}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 群隔离：A 群收录的资源，B 群不能触发。
#[tokio::test]
async fn group_resource_is_invisible_to_other_groups() {
    let mock = MockServer::start().await;
    let (dispatcher, dir) =
        stack_with_resource(&mock, "GA", "本群专属", qqbot_store::ResourceScope::Group, "iso").await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"IN_ISO","author":{"member_openid":"U1"},"content":"本群专属","group_openid":"GB"}"#,
    )
    .await;

    assert!(
        mock.find(|h| h.path == "/v2/groups/GB/messages").is_none(),
        "B 群不该看到 A 群的资源"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 系统资源对所有群可见。
#[tokio::test]
async fn system_resource_is_visible_everywhere() {
    let mock = MockServer::start().await;
    let (dispatcher, dir) = stack_with_resource(
        &mock,
        "",
        "全局图",
        qqbot_store::ResourceScope::System,
        "sys",
    )
    .await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"IN_SYS","author":{"member_openid":"U1"},"content":"全局图","group_openid":"GANY"}"#,
    )
    .await;

    assert!(
        mock.find(|h| h.path == "/v2/groups/GANY/messages").is_some(),
        "系统资源应当在任何群都能触发"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 非资源关键词必须放行给后面的插件，而不是被监听器吞掉。
#[tokio::test]
async fn non_resource_keyword_falls_through() {
    let mock = MockServer::start().await;
    let (dispatcher, dir) =
        stack_with_resource(&mock, "GF", "地图", qqbot_store::ResourceScope::Group, "pass").await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"IN_PING","author":{"member_openid":"U1"},"content":"ping","group_openid":"GF"}"#,
    )
    .await;

    let send = mock.find(|h| h.path == "/v2/groups/GF/messages").expect("ping 应当仍然生效");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    assert!(
        body["content"].as_str().unwrap_or_default().contains("pong"),
        "ping 被资源监听器吞掉了: {body}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 用户的实际用法：**先发图片，再发命令**（两条独立消息）。
///
/// 图片消息的 content 是空的，命令消息本身没有附件 —— 两者靠
/// 「回消息库找上一条带附件的消息」关联起来。
#[tokio::test]
async fn collect_picks_up_an_image_from_a_previous_message() {
    let mock = MockServer::start().await;
    let dir = std::env::temp_dir().join(format!("qqbot-collect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");

    let store = Arc::new(
        qqbot_store::open_resource_store(dir.join("res.db"))
            .await
            .expect("打开资源库"),
    );
    let messages = MessageStore::open_async(StoreConfig {
        path: dir.join("msg.db"),
        flush_interval: Duration::from_millis(10),
        sweep_interval: Duration::from_secs(3600),
        ..StoreConfig::default()
    })
    .await
    .expect("打开消息库");
    let cfg = qqbot_plugins::ResourcesConfig {
        store: Arc::clone(&store),
        messages: Arc::clone(&messages),
        basepath: dir.join("collected"),
        controllers: None,
    };
    let dispatcher = build_stack_with(&mock, Some(messages), Some(cfg)).await;

    // 1) 先发图片：content 为空，图片在 attachments 里。
    let url = format!("{}/test-image.png", mock.base_url());
    let img = format!(
        r#"{{"id":"IN_IMG","author":{{"member_openid":"U1"}},"content":"","group_openid":"GCOL","attachments":[{{"url":"{url}","content_type":"image/png","filename":"a.png","size":16}}]}}"#
    );
    feed(&dispatcher, "GROUP_MESSAGE_CREATE", &img).await;
    // 入库是**异步批量**的（写线程每 flush_interval 提交一次），
    // 不等一下的话命令会跑在图片落库之前。
    tokio::time::sleep(Duration::from_millis(80)).await;

    // 2) 再发命令：本身没有附件，必须回消息库找回上一条的图。
    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"IN_CMD","author":{"member_openid":"U1","member_role":"admin"},"content":"收录 测试","group_openid":"GCOL"}"#,
    )
    .await;

    let items = store
        .list(qqbot_store::ResourceScope::Group, "GCOL")
        .await
        .expect("列资源");
    assert_eq!(items.len(), 1, "应当收录到一条资源");
    assert_eq!(items[0].name, "测试");
    let saved = std::fs::read(&items[0].path).expect("素材应当已落盘");
    assert_eq!(saved, b"FAKE-IMAGE-BYTES", "落盘的应当是下载到的字节");

    let _ = std::fs::remove_dir_all(&dir);
}

/// **允许收录别人发的图**：同会话内不做发送者限制。
///
/// 群里常见的用法就是「有人发了张图，管理员顺手收录」。
#[tokio::test]
async fn collect_accepts_another_senders_image() {
    let mock = MockServer::start().await;
    let dir = std::env::temp_dir().join(format!("qqbot-other-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");

    let store = Arc::new(
        qqbot_store::open_resource_store(dir.join("res.db"))
            .await
            .expect("打开资源库"),
    );
    let messages = MessageStore::open_async(StoreConfig {
        path: dir.join("msg.db"),
        flush_interval: Duration::from_millis(10),
        sweep_interval: Duration::from_secs(3600),
        ..StoreConfig::default()
    })
    .await
    .expect("打开消息库");
    let cfg = qqbot_plugins::ResourcesConfig {
        store: Arc::clone(&store),
        messages: Arc::clone(&messages),
        basepath: dir.join("collected"),
        controllers: None,
    };
    let dispatcher = build_stack_with(&mock, Some(messages), Some(cfg)).await;

    // U1 发图，U2 收录。
    let img = format!(
        r#"{{"id":"IN_IMG2","author":{{"member_openid":"U1"}},"content":"","group_openid":"GST","attachments":[{{"url":"{}/test-image.png"}}]}}"#,
        mock.base_url()
    );
    feed(&dispatcher, "GROUP_MESSAGE_CREATE", &img).await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"IN_CMD2","author":{"member_openid":"U2","member_role":"admin"},"content":"收录 别人发的","group_openid":"GST"}"#,
    )
    .await;

    let items = store
        .list(qqbot_store::ResourceScope::Group, "GST")
        .await
        .expect("列资源");
    assert_eq!(items.len(), 1, "别人发的图也应当能收录");
    assert_eq!(items[0].name, "别人发的");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **回复收录**：命令消息本身带 `message_type = 103` 与 `msg_elements`，
/// 被引用消息的图片就在里面。
///
/// 这条路比「扫消息库」精确得多，而且不依赖消息是否已落盘 ——
/// 所以它排在扫库之前。
#[tokio::test]
async fn collect_reads_the_quoted_message_attachment() {
    let mock = MockServer::start().await;
    let dir = std::env::temp_dir().join(format!("qqbot-quote-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");

    let store = Arc::new(
        qqbot_store::open_resource_store(dir.join("res.db"))
            .await
            .expect("打开资源库"),
    );
    let messages = MessageStore::open_async(StoreConfig {
        path: dir.join("msg.db"),
        flush_interval: Duration::from_millis(10),
        sweep_interval: Duration::from_secs(3600),
        ..StoreConfig::default()
    })
    .await
    .expect("打开消息库");
    let cfg = qqbot_plugins::ResourcesConfig {
        store: Arc::clone(&store),
        messages: Arc::clone(&messages),
        basepath: dir.join("collected"),
        controllers: None,
    };
    let dispatcher = build_stack_with(&mock, Some(messages), Some(cfg)).await;

    // 结构照抄实测的回复消息：content 是用户输入，图在被引用的消息里。
    let url = format!("{}/test-image.png", mock.base_url());
    let reply = format!(
        concat!(
            r#"{{"id":"IN_QUOTE","author":{{"member_openid":"U1","member_role":"admin"}},"content":" 收录 引用图","group_openid":"GQ","message_type":103,"msg_elements":[{{"message_type":103,"content":"1\n2","attachments":[{{"content_type":"image/jpeg","filename":"a.jpeg","size":47195,"url":"{}"}}]}}]}}"#
        ),
        url
    );
    feed(&dispatcher, "GROUP_MESSAGE_CREATE", &reply).await;

    let items = store
        .list(qqbot_store::ResourceScope::Group, "GQ")
        .await
        .expect("列资源");
    assert_eq!(items.len(), 1, "应当从引用消息里收录成功");
    assert_eq!(items[0].name, "引用图");
    let saved = std::fs::read(&items[0].path).expect("素材应当已落盘");
    assert_eq!(saved, b"FAKE-IMAGE-BYTES");

    let _ = std::fs::remove_dir_all(&dir);
}
