//! 端到端集成测试：用本地 mock HTTP 服务器跑通完整链路。
//!
//! 验证范围（**不需要真实凭据，不触碰线上**）：
//!   access_token → 事件去重 → 被动窗口登记 → 路由 → 插件
//!   → 渲染 PNG → 分片上传 → 合并 → 发送消息
//!
//! 断言落在「mock 服务器实际收到的 HTTP 请求」上，因此覆盖了序列化、
//! 鉴权头、分片规划、msg_seq 分配等真实细节。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// `/regular/maps` 是否返回上游故障。
    fail_maps: AtomicBool,
    /// 三角洲接口是否返回上游的 `code=-101`（模拟未握手 / 系统繁忙）。
    delta_busy: AtomicBool,
    /// 日报接口是否返回 `code != 200`。
    daily_broken: AtomicBool,
    /// BA 图片接口是否返回「模糊搜索」（code 101）。
    ba_fuzzy: AtomicBool,
    /// 番剧更新页是否返回一个解析不出条目的页面（模拟站点改版）。
    bangumi_empty: AtomicBool,
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
        MockBuilder {
        fail_first_sends: 0,
        fail_maps: false,
        delta_busy: false,
        daily_broken: false,
        ba_fuzzy: false,
        bangumi_empty: false,
    }
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
    fail_maps: bool,
    delta_busy: bool,
    daily_broken: bool,
    ba_fuzzy: bool,
    bangumi_empty: bool,
}

impl MockBuilder {
    fn fail_first_sends(mut self, n: usize) -> Self {
        self.fail_first_sends = n;
        self
    }

    fn fail_maps(mut self, on: bool) -> Self {
        self.fail_maps = on;
        self
    }

    fn delta_busy(mut self, on: bool) -> Self {
        self.delta_busy = on;
        self
    }

    fn daily_broken(mut self, on: bool) -> Self {
        self.daily_broken = on;
        self
    }

    fn ba_fuzzy(mut self, on: bool) -> Self {
        self.ba_fuzzy = on;
        self
    }

    fn bangumi_empty(mut self, on: bool) -> Self {
        self.bangumi_empty = on;
        self
    }

    async fn start(self) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock server");
        let addr = listener.local_addr().unwrap();
        let inner = Arc::new(MockInner {
            hits: Mutex::new(Vec::new()),
            sends: AtomicUsize::new(0),
            fail_first_sends: self.fail_first_sends,
            fail_maps: AtomicBool::new(self.fail_maps),
            delta_busy: AtomicBool::new(self.delta_busy),
            daily_broken: AtomicBool::new(self.daily_broken),
            ba_fuzzy: AtomicBool::new(self.ba_fuzzy),
            bangumi_empty: AtomicBool::new(self.bangumi_empty),
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
    // 塔科夫静态 JSON：弹药数据。
    if path.starts_with("/regular/items") {
        return (
            "200 OK",
            concat!(
                r#"{"data":{"items":{"#,
                r#""a1":{"normalizedName":"556x45mm-m855","basePrice":180,"properties":{"propertiesType":"ItemPropertiesAmmo","caliber":"Caliber556x45NATO","damage":54,"penetrationPower":31,"armorDamage":37,"initialSpeed":922,"projectileCount":1,"tracer":false}},"#,
                r#""a2":{"normalizedName":"545x39mm-bp","basePrice":110,"properties":{"propertiesType":"ItemPropertiesAmmo","caliber":"Caliber545x39","damage":51,"penetrationPower":37,"armorDamage":42,"initialSpeed":890,"projectileCount":1,"tracer":false}},"#,
                r#""a3":{"normalizedName":"f-1-grenade","basePrice":100,"properties":{"propertiesType":"ItemPropertiesGrenade","damage":80}},"#,
                // 跳蚤市场用：一件有跳蚤价、一件只有商人价。
                r#""5447a9cd4bdc2dbd208b4567":{"normalizedName":"colt-m4a1-556x45-assault-rifle","basePrice":18397,"lastLowPrice":23932,"avg24hPrice":93642,"low24hPrice":20000,"high24hPrice":185000,"weight":3.4},"#,
                r#""5c0e53c886f7744a13f54933":{"normalizedName":"slick-body-armor","basePrice":100000,"lastLowPrice":null,"avg24hPrice":null}"#,
                r#"}}}"#,
            )
            .to_string(),
        );
    }
    // 三角洲（kkrb）：握手三步 + 数据。
    if path == "/getMenu" {
        // 真实站点在这一步下发会话标记；这里只要确认请求发到了。
        return ("200 OK", json!({"code": 1, "menu": []}).to_string());
    }
    if path == "/getOVData" {
        if inner.delta_busy.load(Ordering::Relaxed) {
            return ("200 OK", json!({"code": -101, "msg": "系统繁忙，请稍后再试"}).to_string());
        }
        return (
            "200 OK",
            concat!(
                r#"{"code":1,"msg":"获取成功","data":{"#,
                r#""bdData":{"db":{"password":"0533","updated":"20260930000002"},"cgxg":{"password":"0637","updated":"20260930000002"},"bks":{"password":"0593","updated":"20260930000002"},"htjd":{"password":"0774","updated":"20260930000002"},"cxjy":{"password":"0352","updated":"20260930000002"}},"#,
                r#""bcicData":[{"name":"快递箱","energy":"8"},{"name":"手提箱","energy":"16"}],"#,
                r#""ariiData":[{"activityName":"研发部门 - 集市","activityTime":"2026/09/25 - 2025/10/02","itemName":"锈迹斑斑的海盗铜币","currectPrice":36046,"activitySuggestedPrice":13052}]"#,
                r#"}}"#,
            )
            .to_string(),
        );
    }
    // 握手的前两步：只要求 200。
    if path == "/overview" {
        return ("200 OK", "<html>overview</html>".to_string());
    }
    // 日报：上游返回 JSON，里面是长图地址。
    if path.starts_with("/daily-api") {
        if inner.daily_broken.load(Ordering::Relaxed) {
            // 上游失败时 HTTP 仍是 200，只在 body 里带 code —— 与官方 API 同一个坑。
            return ("200 OK", json!({"code": 500, "message": "服务异常"}).to_string());
        }
        return (
            "200 OK",
            json!({
                "code": 200,
                // 用 addr 拼回自己 —— mock 服务器不知道自己的对外地址。
                "data": { "image": format!("http://{addr}/daily-long.png") },
            })
            .to_string(),
        );
    }
    if path == "/daily-long.png" {
        return ("200 OK", "FAKE-DAILY-LONG-IMAGE".to_string());
    }
    // 塔科夫静态 JSON：任务与商人。
    if path.starts_with("/regular/tasks") {
        return (
            "200 OK",
            concat!(
                r#"{"data":{"tasks":{"#,
                r#""k1":{"normalizedName":"gunsmith-part-1","trader":"t1","minPlayerLevel":5,"kappaRequired":true,"lightkeeperRequired":false,"experience":3000,"objectives":[{},{}],"wikiLink":"https://x/1"},"#,
                r#""k2":{"normalizedName":"first-in-line","trader":"t2","minPlayerLevel":1,"kappaRequired":false,"lightkeeperRequired":true,"experience":500,"objectives":[]}"#,
                r#"}}}"#,
            )
            .to_string(),
        );
    }
    if path.starts_with("/regular/traders") {
        return (
            "200 OK",
            json!({"data": {"t1": {"normalizedName": "mechanic"}, "t2": {"normalizedName": "prapor"}}})
                .to_string(),
        );
    }
    // B 站稿件信息 + 封面。
    if path.starts_with("/x/web-interface/view") {
        return (
            "200 OK",
            json!({
                "code": 0,
                "message": "0",
                "data": {
                    "bvid": "BV1mokxBtEZh",
                    "aid": 115914909417877i64,
                    "title": "测试稿件",
                    "pic": format!("http://{addr}/cover.jpg"),
                    "pubdate": 1_759_000_000,
                    "owner": {"name": "测试UP"},
                    "stat": {"view": 12345, "danmaku": 67, "coin": 8, "like": 9, "reply": 10, "share": 11}
                }
            })
            .to_string(),
        );
    }
    if path.starts_with("/cover.jpg") {
        return ("200 OK", "FAKE-COVER".to_string());
    }
    // 番剧更新页：结构照 agedm.io/update 的片段。
    if path.starts_with("/update") {
        if inner.bangumi_empty.load(Ordering::Relaxed) {
            return ("200 OK", "<html><body>改版了</body></html>".to_string());
        }
        return (
            "200 OK",
            concat!(
                r#"<span class="video_item--info rounded-1 text-truncate">第12集(完结)</span>"#,
                r#"<a href="http://x/detail/1" class="link-light text-decoration-none stretched-link">柔光魔女 &amp; 公司</a>"#,
                r#"<span class="video_item--info rounded-1 text-truncate">第01集</span>"#,
                r#"<a href="http://x/detail/2" class="link-light text-decoration-none stretched-link">转生贵族</a>"#,
            )
            .to_string(),
        );
    }
    // UP 主信息：订阅时用它校验 UID。
    if path.starts_with("/x/web-interface/card") {
        if path.contains("mid=999") {
            return ("200 OK", json!({"code": -404, "message": "啥都木有"}).to_string());
        }
        return (
            "200 OK",
            json!({"code": 0, "message": "OK", "data": {"card": {"name": "测试UP主"}}}).to_string(),
        );
    }
    // 塔科夫服务器状态：形状照 status.escapefromtarkov.com 的响应。
    if path.starts_with("/api/services") {
        return (
            "200 OK",
            json!([
                {"name": "Website", "status": 0},
                {"name": "Matchmaking", "status": 2},
                {"name": "Something New", "status": 9},
            ])
            .to_string(),
        );
    }
    if path.starts_with("/api/global/status") {
        return ("200 OK", json!({"status": 1, "message": "正在维护"}).to_string());
    }
    // BA 图片查询：形状照 arona 的响应。
    if path.starts_with("/api/v2/image") {
        if inner.ba_fuzzy.load(Ordering::Relaxed) {
            return (
                "200 OK",
                json!({
                    "code": 101,
                    "message": "Fuzzy Search",
                    "data": [
                        {"name": "汉堡", "type": "file", "content": "/student_rank/泉.png"},
                        {"name": "水汉堡", "type": "file", "content": "/student_rank/泳装泉.png"},
                    ],
                })
                .to_string(),
            );
        }
        return (
            "200 OK",
            json!({
                "code": 200,
                "message": "OK",
                "data": [{"name": "爱丽丝", "type": "file", "content": "/student_rank/爱丽丝.png"}],
            })
            .to_string(),
        );
    }
    // BA 图片 CDN。
    if path.starts_with("/image/s/") {
        return ("200 OK", "FAKE-BA-IMAGE".to_string());
    }
    // 塔科夫 BOSS 刷新率：形状照 `json.tarkov.dev/regular/maps`。
    // `maps` 是**按 id 键控的对象**，不是数组。
    if path.starts_with("/regular/maps") {
        if inner.fail_maps.load(Ordering::Relaxed) {
            return ("503 Service Unavailable", json!({"error": "down"}).to_string());
        }
        return (
            "200 OK",
            concat!(
                r#"{"data":{"maps":{"#,
                r#""m1":{"normalizedName":"customs","bosses":[{"mob":"bossReshala","spawnChance":0.4},{"mob":"bossReshala","spawnChance":0.6}]},"#,
                r#""m2":{"normalizedName":"reserve","bosses":[{"mob":"bossGlukhar","spawnChance":1.0}]}"#,
                r#"}}}"#,
            )
            .to_string(),
        );
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
    // 超时给得很宽，是因为**这个测试进程本身**会把渲染服务挤爆：
    // 几十个测试并行跑，每个都可能触发渲染，而 debug 构建的 resvg
    // 比 release 慢 25~31 倍（见 AGENTS.md）。10 秒会被偶发超过，
    // 表现为「渲染超时 → 插件降级成文字」，于是断言图片的测试随机失败。
    //
    // 这不是产品问题：release 下一张卡片 47ms，超时是 5 秒，余量 100 倍。
    // 但测试必须稳定，所以这里不按产品的预算来。
    let render = RenderService::new(RenderConfig {
        timeout: Duration::from_secs(120),
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
            // 指向 mock：日报要下载长图再转成 file_info 发送，
            // 这正是「涉及发送链路」的那一类，必须有端到端覆盖。
            daily: Some(qqbot_plugins::DailyConfig {
                api_url: format!("{}/daily-api", mock.base_url()),
                token: "test-token".to_string(),
                cache: Duration::from_secs(3600),
            }),
            resources,
            // 指向 mock，否则塔科夫的 GraphQL 会真的打出去。
            tarkov: qqbot_plugins::TarkovConfig {
                graphql_url: String::new(),
                maps_url: format!("{}/regular/maps", mock.base_url()),
                status_base: mock.base_url(),
                // 静态图从目录读，所以测试里造一个目录放两张假图。
                images_dir: Some(tarkov_image_dir()),
                ..Default::default()
            },
            ba: qqbot_plugins::BaConfig {
                api: format!("{}/api/v2/image?name=", mock.base_url()),
                cdn: format!("{}/image/s", mock.base_url()),
            },
            bili: qqbot_plugins::BiliConfig {
                view_api: format!("{}/x/web-interface/view?bvid=", mock.base_url()),
                card_api: format!("{}/x/web-interface/card?mid=", mock.base_url()),
                store: None,
            },
            bangumi: qqbot_plugins::BangumiConfig {
                update_url: format!("{}/update", mock.base_url()),
            },
            ammo: qqbot_plugins::AmmoConfig {
                items_url: format!("{}/regular/items", mock.base_url()),
                store: None,
            },
            market: qqbot_plugins::MarketConfig {
                items_url: format!("{}/regular/items", mock.base_url()),
                store: None,
            },
            delta: qqbot_plugins::DeltaConfig {
                home_url: format!("{}/", mock.base_url()),
                overview_url: format!("{}/overview", mock.base_url()),
                menu_url: format!("{}/getMenu", mock.base_url()),
                data_url: format!("{}/getOVData", mock.base_url()),
            },
            task: qqbot_plugins::TaskConfig {
                tasks_url: format!("{}/regular/tasks", mock.base_url()),
                traders_url: format!("{}/regular/traders", mock.base_url()),
                store: None,
            },
        },
    )
    .await
    .expect("初始化插件失败");

    Arc::new(Dispatcher::new(router, services, DispatchConfig::default()))
}

/// 塔科夫静态图的临时目录。内容无所谓：断言的是「读到了文件并作为富媒体发出」。
fn tarkov_image_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("qqbot-tkf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建图目录");
    for name in ["Customs.jpg", "TaskProcess.jpg", "headset.png"] {
        std::fs::write(dir.join(name), b"\x89PNG\r\n\x1a\nfake").expect("写图");
    }
    dir
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
        store.recent_texts(Scope::Group, "GP", None, 0, 10).await.unwrap(),
        vec!["群里说的话"]
    );
    assert_eq!(
        store.recent_texts(Scope::C2c, "U2", None, 0, 10).await.unwrap(),
        vec!["私聊说的话"]
    );
    // 群与私聊不能串台
    assert!(store.recent_texts(Scope::C2c, "GP", None, 0, 10).await.unwrap().is_empty());

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

/// 造一个带**空**资源库的栈。返回 (dispatcher, 临时目录, 资源库)。
///
/// 与 `stack_with_resource` 分开，是为了能测「先收录再触发」这条路 ——
/// 那条路才是用户实际要走的（`系统收录 <关键词> <路径>`），
/// 而预先塞好资源的写法绕过了整个命令处理。
async fn resource_dir_and_store(
    tag: &str,
) -> (std::path::PathBuf, Arc<qqbot_store::ResourceStore>) {
    let dir = std::env::temp_dir().join(format!("qqbot-res-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");

    let store = Arc::new(
        qqbot_store::open_resource_store(dir.join("res.db"))
            .await
            .expect("打开资源库"),
    );
    (dir, store)
}

/// 用给定的资源库装配一个栈。
///
/// **必须在插入资源之后再调用** —— 插件在构造时就把关键词建成内存索引，
/// 先装配后插入的话，资源永远进不了索引。
async fn build_resource_stack(
    mock: &MockServer,
    dir: &std::path::Path,
    store: Arc<qqbot_store::ResourceStore>,
) -> Arc<Dispatcher> {
    let dir = dir.to_path_buf();

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
    build_stack_with(mock, Some(messages), Some(cfg)).await
}

/// 造一个带**空**资源库的栈。返回 (dispatcher, 临时目录, 资源库)。
async fn stack_with_resources(
    mock: &MockServer,
    tag: &str,
) -> (Arc<Dispatcher>, std::path::PathBuf, Arc<qqbot_store::ResourceStore>) {
    let (dir, store) = resource_dir_and_store(tag).await;
    let dispatcher = build_resource_stack(mock, &dir, Arc::clone(&store)).await;
    (dispatcher, dir, store)
}

/// 造一个带资源库的栈，并放一条资源。返回 (dispatcher, 临时目录)。
async fn stack_with_resource(
    mock: &MockServer,
    group: &str,
    keyword: &str,
    scope: qqbot_store::ResourceScope,
    tag: &str,
) -> (Arc<Dispatcher>, std::path::PathBuf) {
    // 先建库、写资源，**再**装配 —— 顺序反了资源就进不了索引。
    let (dir, store) = resource_dir_and_store(tag).await;

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

    (build_resource_stack(mock, &dir, store).await, dir)
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

/// 词云的 8 种组合要能路由，且标题带上窗口 —— 否则用户分不清看的是哪一段。
#[tokio::test]
async fn wordcloud_window_variant_routes_and_names_the_window() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"WC_1","author":{"member_openid":"U1","username":"小明"},"content":"本群今日词云","group_openid":"GW"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GW/messages")
        .expect("应当回复（语料不足提示）");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let md = body["markdown"]["content"].as_str().unwrap_or_default();
    assert!(md.contains("本群今日词云"), "标题要指明窗口，否则分不清看的是哪段: {md}");
}

/// 全量模式下机器人能看到**所有**群消息，所以近似说法不能触发渲染。
#[tokio::test]
async fn wordcloud_lookalike_does_not_trigger() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"WC_2","author":{"member_openid":"U1"},"content":"本群今日词云好看吗","group_openid":"GW2"}"#,
    )
    .await;

    assert!(
        mock.find(|h| h.path == "/v2/groups/GW2/messages").is_none(),
        "近似说法不该触发词云渲染"
    );
}

/// B8 塔科夫时间：纯本地换算，回两行时刻（左 / 右相差 12 小时）。
#[tokio::test]
async fn tarkov_time_replies_with_two_clocks() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"TKF_1","author":{"member_openid":"U1"},"content":"塔科夫时间","group_openid":"GTKF"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GTKF/messages")
        .expect("应当回复塔科夫时间");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "与源项目一致：两行时刻: {text}");
    for line in lines {
        assert_eq!(line.len(), 8, "HH:MM:SS: {text}");
        assert_eq!(line.as_bytes()[2], b':', "HH:MM:SS: {text}");
    }
}

/// B7 BOSS 刷新率：GraphQL 结果按地图聚合，同一 BOSS 的多个刷新点取平均。
#[tokio::test]
async fn boss_chance_aggregates_by_map_and_averages() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        // 用 `boss刷` 而不是 `boss刷新率`：后者是 B9–B15 的静态图命令
        // （cq-bot 里也是），这里要测的是实时数据那条路。
        r#"{"id":"BOSS_1","author":{"member_openid":"U1"},"content":"boss刷","group_openid":"GB"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GB/messages")
        .expect("应当回复 BOSS 刷新率");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    assert!(text.contains("customs"), "应当列出地图名: {text}");
    assert!(text.contains("bossReshala: 50%"), "(0.4+0.6)/2 = 50%: {text}");
    assert!(text.contains("bossGlukhar: 100%"), "1.0 是比例不是百分数: {text}");
}

/// 上游故障时要给出可读提示，而不是把 422 原文丢给用户。
#[tokio::test]
async fn boss_chance_reports_upstream_failure_gracefully() {
    let mock = MockServer::builder().fail_maps(true).start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"BOSS_2","author":{"member_openid":"U1"},"content":"boss概率","group_openid":"GB2"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GB2/messages")
        .expect("失败也要回复，不能静默");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    assert!(text.contains("查询失败"), "应当是给用户看的话: {text}");
    assert!(!text.contains("422"), "不该把上游原文丢给用户: {text}");
}

/// E3 BA 图片：官方 API 不接受远程 URL 直发，必须先下载再上传。
#[tokio::test]
async fn ba_image_is_downloaded_then_uploaded() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"BA_1","author":{"member_openid":"U1"},"content":"ba 爱丽丝","group_openid":"GBA"}"#,
    )
    .await;

    assert!(
        mock.find(|h| h.path.contains("/image/s/")).is_some(),
        "应当去 CDN 下载图片"
    );
    assert!(
        mock.find(|h| h.path.contains("/upload_prepare")).is_some(),
        "必须先上传拿 file_info，不能直发远程 URL"
    );
    let send = mock
        .find(|h| h.path == "/v2/groups/GBA/messages")
        .expect("应当回复图片");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    assert_eq!(body["msg_type"], 7, "富媒体必须用 msg_type=7: {body}");
}

/// 模糊搜索要列候选让人重问，而不是报错或发一堆图。
#[tokio::test]
async fn ba_fuzzy_search_lists_candidates_instead_of_images() {
    let mock = MockServer::builder().ba_fuzzy(true).start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"BA_2","author":{"member_openid":"U1"},"content":"ba 泉","group_openid":"GBA2"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GBA2/messages")
        .expect("应当回复候选");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    assert!(text.contains("是想问什么呢"), "{text}");
    assert!(text.contains("汉堡") && text.contains("水汉堡"), "要列出全部候选: {text}");
    assert!(
        mock.find(|h| h.path.contains("/image/s/")).is_none(),
        "模糊搜索不该去下载图片"
    );
}

/// A2 区间记法：`.r 5 10` 取 [5,10] 内的整数，两数自动排序。
#[tokio::test]
async fn dice_range_form_stays_within_bounds() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    // 故意把大数写前面，验证自动排序。
    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"DICE_R1","author":{"member_openid":"U1"},"content":".r 10 5","group_openid":"GD"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GD/messages")
        .expect("应当回复骰子");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    assert!(text.contains("范围：[5-10]"), "两数应当自动排序: {text}");
    let value: i64 = text
        .rsplit("结果：")
        .next()
        .and_then(|s| s.trim().parse().ok())
        .expect("应当给出结果");
    assert!((5..=10).contains(&value), "结果必须落在区间内: {text}");
}

/// `。` 是句末标点，`。roll` 这类正常句子不能被骰子吞掉。
#[tokio::test]
async fn dice_range_form_ignores_ordinary_sentences() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"DICE_R2","author":{"member_openid":"U1"},"content":"。roll点吧","group_openid":"GD2"}"#,
    )
    .await;

    assert!(
        mock.find(|h| h.path == "/v2/groups/GD2/messages").is_none(),
        "普通句子不该触发骰子"
    );
}

/// B1 服务器状态：服务列表 + 总体状态，中文名与状态图标都要对上。
#[tokio::test]
async fn tarkov_server_status_lists_services_in_chinese() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"SRV_1","author":{"member_openid":"U1"},"content":"服务器","group_openid":"GS"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GS/messages")
        .expect("应当回复服务器状态");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    assert!(text.contains("服务器状态速报："), "{text}");
    assert!(text.contains("游戏官网：🟢服务正常"), "{text}");
    assert!(text.contains("战局匹配：🟡部分故障"), "{text}");
    assert!(text.contains("总体状态：⚙️正在更新"), "{text}");
    assert!(text.contains("信息：正在维护"), "{text}");
}

/// C1：链接消息自动展开成卡片，且图与文字在**同一条**消息里 ——
/// 拆成两条会白占一次被动回复配额（群聊一共只有 5 次）。
#[tokio::test]
async fn bilibili_link_expands_into_one_card_message() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"BILI_1","author":{"member_openid":"U1"},"content":"https://www.bilibili.com/video/BV1mokxBtEZh","group_openid":"GBL"}"#,
    )
    .await;

    let sends = mock.all(|h| h.path == "/v2/groups/GBL/messages");
    assert_eq!(sends.len(), 1, "图与文字必须在同一条消息里，不能拆开");
    let body: serde_json::Value = serde_json::from_str(&sends[0].body).unwrap();
    assert_eq!(body["msg_type"], 7, "卡片是富媒体: {body}");
    assert!(body["media"]["file_info"].is_string(), "应当带 file_info: {body}");
    let content = body["content"].as_str().unwrap_or_default();
    assert!(content.contains("测试稿件"), "{content}");
    assert!(content.contains("播放：1.2万 弹幕：67"), "{content}");
    assert!(content.contains("UP：测试UP"), "{content}");
}

/// 同一条链接在一个会话里只展开一次 —— 群里常被几个人先后转发。
#[tokio::test]
async fn bilibili_link_is_not_expanded_twice_in_a_row() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    for id in ["BILI_D1", "BILI_D2"] {
        let payload = format!(
            r#"{{"id":"{id}","author":{{"member_openid":"U1"}},"content":"https://www.bilibili.com/video/BV1mokxBtEZh","group_openid":"GBL2"}}"#
        );
        feed(&dispatcher, "GROUP_MESSAGE_CREATE", &payload).await;
    }

    assert_eq!(
        mock.all(|h| h.path == "/v2/groups/GBL2/messages").len(),
        1,
        "同一条链接短时间内不该重复展开"
    );
}

/// 订阅测试用的栈：订阅表与资源表**共用一个** `ResourceStore`。
static BILI_SEQ: AtomicUsize = AtomicUsize::new(0);

async fn bili_stack(
    mock: &MockServer,
) -> (Arc<Dispatcher>, Arc<qqbot_store::ResourceStore>, std::path::PathBuf) {
    let n = BILI_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("qqbot-bili-{}-{n}", std::process::id()));
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
    let dispatcher = build_stack_with(mock, Some(messages), Some(cfg)).await;
    (dispatcher, store, dir)
}

/// C4/C5：管理员订阅 → 落库 → 退订 → 清空。
#[tokio::test]
async fn bili_subscribe_then_unsubscribe() {
    let mock = MockServer::start().await;
    let (dispatcher, store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"SUB_1","author":{"member_openid":"U1","member_role":"admin"},"content":"哔哩订阅 12345","group_openid":"GSB"}"#,
    )
    .await;

    let subs = store.bili_subscriptions("GSB").await.unwrap();
    assert_eq!(subs.len(), 1, "应当落库一条订阅: {subs:?}");
    assert_eq!(subs[0].uid, "12345");
    assert_eq!(subs[0].name, "测试UP主", "昵称应当来自接口");

    let send = mock
        .find(|h| h.path == "/v2/groups/GSB/messages")
        .expect("应当回复订阅结果");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    assert!(text.contains("订阅成功"), "{text}");
    assert!(text.contains("测试UP主"), "{text}");

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"SUB_2","author":{"member_openid":"U1","member_role":"admin"},"content":"哔哩退订 12345","group_openid":"GSB"}"#,
    )
    .await;
    assert!(
        store.bili_subscriptions("GSB").await.unwrap().is_empty(),
        "退订后不该还有记录"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 订阅与退订都要求群管理员 —— 源项目只给订阅加了检查，
/// 那意味着任何人都能悄悄拆掉别人配好的订阅。
#[tokio::test]
async fn bili_subscribe_requires_group_admin() {
    let mock = MockServer::start().await;
    let (dispatcher, store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"SUB_3","author":{"member_openid":"U9"},"content":"哔哩订阅 12345","group_openid":"GSB2"}"#,
    )
    .await;

    assert!(
        store.bili_subscriptions("GSB2").await.unwrap().is_empty(),
        "非管理员不该订阅成功"
    );
    let send = mock
        .find(|h| h.path == "/v2/groups/GSB2/messages")
        .expect("应当说明原因，而不是静默无视");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    assert!(text.contains("群管理员"), "{text}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// UID 不存在时要说清楚，而不是写进一条永远推不出东西的订阅。
#[tokio::test]
async fn bili_subscribe_rejects_unknown_uid() {
    let mock = MockServer::start().await;
    let (dispatcher, store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"SUB_4","author":{"member_openid":"U1","member_role":"owner"},"content":"哔哩订阅 999","group_openid":"GSB3"}"#,
    )
    .await;

    assert!(
        store.bili_subscriptions("GSB3").await.unwrap().is_empty(),
        "UID 不存在时不该落库"
    );
    let send = mock
        .find(|h| h.path == "/v2/groups/GSB3/messages")
        .expect("应当回复失败原因");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    assert!(text.contains("订阅失败"), "{text}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// E4 番剧日历：抓更新页 → 提取条目 → 复用 `card.svg` 渲染成图。
///
/// 源项目靠 Chromium 截图，本项目必须纯 Rust 渲染。
#[tokio::test]
async fn bangumi_calendar_renders_a_card() {
    let mock = MockServer::start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"BG_1","author":{"member_openid":"U1"},"content":"今日番剧","group_openid":"GBG"}"#,
    )
    .await;

    assert!(
        mock.find(|h| h.path.starts_with("/update")).is_some(),
        "应当抓取更新页"
    );
    let send = match mock.find(|h| h.path == "/v2/groups/GBG/messages") {
        Some(hit) => hit,
        None => panic!(
            "应当回复卡片，实际收到的请求: {:?}",
            mock.hits().iter().map(|h| h.path.clone()).collect::<Vec<_>>()
        ),
    };
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    assert_eq!(body["msg_type"], 7, "卡片是图片: {body}");
    assert!(body["media"]["file_info"].is_string(), "应当走富媒体上传: {body}");
}

/// 站点改版导致提取为空时，要给一句可操作的提示，而不是一张空卡片。
#[tokio::test]
async fn bangumi_reports_when_nothing_was_parsed() {
    let mock = MockServer::builder().bangumi_empty(true).start().await;
    let dispatcher = build_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"BG_2","author":{"member_openid":"U1"},"content":"最新番剧","group_openid":"GBG2"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GBG2/messages")
        .expect("应当回复提示");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    assert!(text.contains("没有解析出"), "{text}");
}

/// F3 图语：`图语 <文字>` 之后，同一个人发的下一张图会被配上文字重发。
#[tokio::test]
async fn caption_is_applied_to_the_next_image() {
    let mock = MockServer::start().await;
    let (dispatcher, _store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"CAP_1","author":{"member_openid":"U1"},"content":"图语 你好呀","group_openid":"GCAP"}"#,
    )
    .await;

    let url = format!("{}/test-image.png", mock.base_url());
    let img = format!(
        r#"{{"id":"CAP_2","author":{{"member_openid":"U1"}},"content":"","group_openid":"GCAP","attachments":[{{"url":"{url}","content_type":"image/png","filename":"a.png"}}]}}"#
    );
    feed(&dispatcher, "GROUP_MESSAGE_CREATE", &img).await;

    let card = mock
        .all(|h| h.path == "/v2/groups/GCAP/messages")
        .into_iter()
        .find(|h| {
            let body: serde_json::Value = serde_json::from_str(&h.body).unwrap_or_default();
            body["msg_type"] == 7
        })
        .expect("应当把图片配字重发");
    let body: serde_json::Value = serde_json::from_str(&card.body).unwrap();
    assert_eq!(body["content"], "你好呀", "图与文字要在同一条消息里: {body}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 图语是一次性的：配过一次就不再对后面的图生效。
#[tokio::test]
async fn caption_is_consumed_by_the_first_image_only() {
    let mock = MockServer::start().await;
    let (dispatcher, _store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"CAP_3","author":{"member_openid":"U1"},"content":"图语 只用一次","group_openid":"GCAP2"}"#,
    )
    .await;

    let url = format!("{}/test-image.png", mock.base_url());
    for id in ["CAP_4", "CAP_5"] {
        let img = format!(
            r#"{{"id":"{id}","author":{{"member_openid":"U1"}},"content":"","group_openid":"GCAP2","attachments":[{{"url":"{url}","filename":"a.png"}}]}}"#
        );
        feed(&dispatcher, "GROUP_MESSAGE_CREATE", &img).await;
    }

    let cards = mock
        .all(|h| h.path == "/v2/groups/GCAP2/messages")
        .into_iter()
        .filter(|h| {
            let body: serde_json::Value = serde_json::from_str(&h.body).unwrap_or_default();
            body["msg_type"] == 7
        })
        .count();
    assert_eq!(cards, 1, "只有第一张图该被配字");

    let _ = std::fs::remove_dir_all(&dir);
}

/// B6：管理员导入弹药 → 检索 → 出卡片。
#[tokio::test]
async fn ammo_import_then_search_renders_a_card() {
    let mock = MockServer::start().await;
    let (dispatcher, store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"AM_1","author":{"member_openid":"U1","member_role":"admin"},"content":"更新子弹","group_openid":"GAM"}"#,
    )
    .await;

    // 手雷的 propertiesType 不是弹药，必须被过滤掉。
    assert_eq!(store.ammo_count().await.unwrap(), 2, "应当只导入两条真弹药");

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"AM_2","author":{"member_openid":"U1"},"content":"查子弹 5.45 bp","group_openid":"GAM"}"#,
    )
    .await;

    let card = mock
        .all(|h| h.path == "/v2/groups/GAM/messages")
        .into_iter()
        .find(|h| {
            let body: serde_json::Value = serde_json::from_str(&h.body).unwrap_or_default();
            body["msg_type"] == 7
        })
        .expect("应当回一张卡片");
    let body: serde_json::Value = serde_json::from_str(&card.body).unwrap();
    assert!(body["media"]["file_info"].is_string(), "应当走富媒体上传: {body}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 没导入过时要给可操作的提示，而不是「没有匹配」。
#[tokio::test]
async fn ammo_search_before_import_says_so() {
    let mock = MockServer::start().await;
    let (dispatcher, _store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"AM_3","author":{"member_openid":"U1"},"content":"查子弹 m855","group_openid":"GAM2"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GAM2/messages")
        .expect("应当回复提示");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    assert!(text.contains("更新子弹"), "要告诉用户怎么解决: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 导入是管理员操作。
#[tokio::test]
async fn ammo_import_requires_admin() {
    let mock = MockServer::start().await;
    let (dispatcher, store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"AM_4","author":{"member_openid":"U9"},"content":"更新子弹","group_openid":"GAM3"}"#,
    )
    .await;

    assert_eq!(store.ammo_count().await.unwrap(), 0, "非管理员不该触发导入");
    let _ = std::fs::remove_dir_all(&dir);
}

/// B4 / B5：管理员导入任务 → 检索 → 出卡片。
#[tokio::test]
async fn task_import_then_search_renders_a_card() {
    let mock = MockServer::start().await;
    let (dispatcher, store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"TK_1","author":{"member_openid":"U1","member_role":"admin"},"content":"更新任务","group_openid":"GTK"}"#,
    )
    .await;

    assert_eq!(store.task_count().await.unwrap(), 2, "应当导入两条");
    let imported = store.search_tasks(vec!["gunsmith".into()], 10).await.unwrap();
    assert_eq!(imported[0].trader, "mechanic", "商人 id 要换成人看得懂的 slug");

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"TK_2","author":{"member_openid":"U1"},"content":"查任务 gunsmith","group_openid":"GTK"}"#,
    )
    .await;

    let card = mock
        .all(|h| h.path == "/v2/groups/GTK/messages")
        .into_iter()
        .find(|h| {
            let body: serde_json::Value = serde_json::from_str(&h.body).unwrap_or_default();
            body["msg_type"] == 7
        })
        .expect("应当回一张卡片");
    let body: serde_json::Value = serde_json::from_str(&card.body).unwrap();
    assert!(body["media"]["file_info"].is_string(), "应当走富媒体上传: {body}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 没导入过时要给可操作的提示。
#[tokio::test]
async fn task_search_before_import_says_so() {
    let mock = MockServer::start().await;
    let (dispatcher, _store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"TK_3","author":{"member_openid":"U1"},"content":"查任务 gunsmith","group_openid":"GTK2"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GTK2/messages")
        .expect("应当回复提示");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    assert!(text.contains("更新任务"), "要告诉用户怎么解决: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// B2：管理员导入物品 → 关键词搜 → 出卡片。
#[tokio::test]
async fn market_search_renders_a_card() {
    let mock = MockServer::start().await;
    let (dispatcher, store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"MK_1","author":{"member_openid":"U1","member_role":"admin"},"content":"更新物品","group_openid":"GMK"}"#,
    )
    .await;

    // 弹药条目也有 slug，所以一并入库；这里只断言目标物品在。
    assert!(store.item_count().await.unwrap() >= 4, "应当导入物品");
    let m4 = store.search_items(vec!["m4a1".into()], 10).await.unwrap();
    assert_eq!(m4.len(), 1);
    assert_eq!(m4[0].avg24h_price, Some(93642));

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"MK_2","author":{"member_openid":"U1"},"content":"跳蚤 m4a1","group_openid":"GMK"}"#,
    )
    .await;

    let card = mock
        .all(|h| h.path == "/v2/groups/GMK/messages")
        .into_iter()
        .find(|h| {
            let body: serde_json::Value = serde_json::from_str(&h.body).unwrap_or_default();
            body["msg_type"] == 7
        })
        .expect("应当回一张卡片");
    let body: serde_json::Value = serde_json::from_str(&card.body).unwrap();
    assert!(body["media"]["file_info"].is_string(), "应当走富媒体上传: {body}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// B3：参数是 24 位 id 时走详情视图，不是关键词搜。
#[tokio::test]
async fn market_item_id_gives_the_detail_view() {
    let mock = MockServer::start().await;
    let (dispatcher, _store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"MK_3","author":{"member_openid":"U1","member_role":"admin"},"content":"更新物品","group_openid":"GMK2"}"#,
    )
    .await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"MK_4","author":{"member_openid":"U1"},"content":"跳蚤 5447a9cd4bdc2dbd208b4567","group_openid":"GMK2"}"#,
    )
    .await;

    let card = mock
        .all(|h| h.path == "/v2/groups/GMK2/messages")
        .into_iter()
        .find(|h| {
            let body: serde_json::Value = serde_json::from_str(&h.body).unwrap_or_default();
            body["msg_type"] == 7
        })
        .expect("按 id 查也应当回卡片");
    assert!(serde_json::from_str::<serde_json::Value>(&card.body).is_ok());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 没导入过时要给可操作的提示。
#[tokio::test]
async fn market_search_before_import_says_so() {
    let mock = MockServer::start().await;
    let (dispatcher, _store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"MK_5","author":{"member_openid":"U1"},"content":"跳蚤 m4a1","group_openid":"GMK3"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GMK3/messages")
        .expect("应当回复提示");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    assert!(text.contains("更新物品"), "要告诉用户怎么解决: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// D1–D3：三条命令各出一张卡片，且都要先完成握手。
#[tokio::test]
async fn delta_commands_render_cards_after_handshake() {
    let mock = MockServer::start().await;
    let (dispatcher, _store, dir) = bili_stack(&mock).await;

    for (id, cmd, group) in [
        ("DF_1", "集市", "GDF"),
        ("DF_2", "脑机", "GDF2"),
        ("DF_3", "密码", "GDF3"),
    ] {
        let msg = format!(
            r#"{{"id":"{id}","author":{{"member_openid":"U1"}},"content":"{cmd}","group_openid":"{group}"}}"#
        );
        feed(&dispatcher, "GROUP_MESSAGE_CREATE", &msg).await;

        let card = mock
            .all(|h| h.path == format!("/v2/groups/{group}/messages"))
            .into_iter()
            .find(|h| {
                let body: serde_json::Value = serde_json::from_str(&h.body).unwrap_or_default();
                body["msg_type"] == 7
            })
            .unwrap_or_else(|| panic!("{cmd} 应当回一张卡片"));
        assert!(serde_json::from_str::<serde_json::Value>(&card.body).is_ok());
    }

    // 握手必须先发生，否则真实站点会稳定返回 code=-101。
    assert!(!mock.all(|h| h.path == "/getMenu").is_empty(), "必须先调 getMenu");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 上游用 code 表达失败时要说清楚，不能默默回一张空卡片。
#[tokio::test]
async fn delta_reports_upstream_code_failure() {
    let mock = MockServer::builder().delta_busy(true).start().await;
    let (dispatcher, _store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"DF_4","author":{"member_openid":"U1"},"content":"密码","group_openid":"GDF4"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GDF4/messages")
        .expect("应当回复错误提示");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    assert!(text.contains("-101"), "要带上游的 code: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// D4：一图流把三块数据渲染成**一张**分段卡片。
#[tokio::test]
async fn delta_overview_renders_one_sectioned_card() {
    let mock = MockServer::start().await;
    let (dispatcher, _store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"DF_5","author":{"member_openid":"U1"},"content":"一图流","group_openid":"GDF5"}"#,
    )
    .await;

    // 「一图流」顾名思义只发一张图：多发一张就白占一次被动回复配额。
    let media = mock
        .all(|h| h.path == "/v2/groups/GDF5/messages")
        .into_iter()
        .filter(|h| {
            let body: serde_json::Value = serde_json::from_str(&h.body).unwrap_or_default();
            body["msg_type"] == 7
        })
        .count();
    assert_eq!(media, 1, "应当只发一张卡片（一图流 = 一张，不是每段一张）");

    let _ = std::fs::remove_dir_all(&dir);
}

/// B9–B15 走的正是这条路：`系统收录 <关键词> <路径>` → 关键词触发。
///
/// 之前只测了「库里已经有资源」的情况，那条路绕过了整个命令处理，
/// 所以收录命令本身（参数解析、权限、落盘、索引重建）一直没被端到端验证过。
#[tokio::test]
async fn system_collect_then_keyword_triggers() {
    let mock = MockServer::start().await;
    let (dispatcher, dir, store) = stack_with_resources(&mock, "collect").await;

    // 用户手上就是这样一个文件，路径由他提供。
    let img = dir.join("Customs.jpg");
    std::fs::write(&img, b"\x89PNG\r\n\x1a\nfake-map").expect("写素材");
    let cmd = format!(
        "{{\"id\":\"IN_C1\",\"author\":{{\"member_openid\":\"{}\"}},\"content\":\"系统收录 海关地图 {}\",\"group_openid\":\"GCOL\"}}",
        qqbot_store::DEFAULT_SYSTEM_CONTROLLER,
        // Windows 路径里的反斜杠是非法 JSON 转义（`\U`），必须转义后再塞进消息体。
        img.display().to_string().replace('\\', "\\\\")
    );
    feed(&dispatcher, "GROUP_MESSAGE_CREATE", &cmd).await;

    let ack = mock
        .find(|h| h.path == "/v2/groups/GCOL/messages")
        .expect("收录应当有回执");
    let ack_body: serde_json::Value = serde_json::from_str(&ack.body).unwrap();
    // 资源插件走的是 Markdown（`msg_type: 2`），文字在 `markdown.content` 里，
    // 不是纯文本的 `content`。
    let text = ack_body["markdown"]["content"].as_str().unwrap_or_default();
    assert!(text.contains("已收录"), "回执要说清楚结果: {ack_body}");

    // 库里真的有了，而且关键词是整串。
    let entries = store.keywords().await.expect("读关键词");
    assert!(
        entries.iter().any(|e| e.keyword == "海关地图"),
        "应当收录为关键词「海关地图」: {entries:?}"
    );

    // 现在发这个关键词，应当把图发出来。
    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"IN_C2","author":{"member_openid":"U1"},"content":"海关地图","group_openid":"GCOL"}"#,
    )
    .await;

    let sent = mock
        .all(|h| h.path == "/v2/groups/GCOL/messages")
        .into_iter()
        .find(|h| {
            let b: serde_json::Value = serde_json::from_str(&h.body).unwrap_or_default();
            b["msg_type"] == 7
        })
        .expect("收录过的关键词应当把图发出来");
    assert!(serde_json::from_str::<serde_json::Value>(&sent.body).is_ok());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 收录是系统控制者的权限；别人发同样的命令不该写进库。
#[tokio::test]
async fn system_collect_rejects_non_controllers() {
    let mock = MockServer::start().await;
    let (dispatcher, dir, store) = stack_with_resources(&mock, "deny").await;

    let img = dir.join("x.jpg");
    std::fs::write(&img, b"fake").expect("写素材");
    let cmd = format!(
        "{{\"id\":\"IN_C3\",\"author\":{{\"member_openid\":\"U9\"}},\"content\":\"系统收录 不该有 {}\",\"group_openid\":\"GCOL2\"}}",
        img.display().to_string().replace('\\', "\\\\")
    );
    feed(&dispatcher, "GROUP_MESSAGE_CREATE", &cmd).await;

    let entries = store.keywords().await.expect("读关键词");
    assert!(
        !entries.iter().any(|e| e.keyword == "不该有"),
        "非控制者不该写进库: {entries:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// E5 日报：下载上游长图 → 转成 file_info → 以富媒体发送。
///
/// 这条链路此前**完全没有端到端覆盖**：`DailyConfig` 在测试配置里是 `None`，
/// 而 token 为空时插件根本不注册 —— 所以「日报」这个命令在测试里从未存在过。
/// 它偏偏是「涉及发送链路」的典型（下载 → 上传 → 发富媒体），
/// 正是 AGENTS.md §9 要求端到端覆盖的那一类。
#[tokio::test]
async fn daily_downloads_and_resends_the_image() {
    let mock = MockServer::start().await;
    let (dispatcher, _store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"DL_1","author":{"member_openid":"U1"},"content":"日报","group_openid":"GDL"}"#,
    )
    .await;

    // 必须真的去下载了那张长图 —— 只回一句文字不算。
    assert!(
        !mock.all(|h| h.path == "/daily-long.png").is_empty(),
        "应当下载上游图片"
    );

    let card = mock
        .all(|h| h.path == "/v2/groups/GDL/messages")
        .into_iter()
        .find(|h| {
            let body: serde_json::Value = serde_json::from_str(&h.body).unwrap_or_default();
            body["msg_type"] == 7
        })
        .expect("日报应当以富媒体发出");
    let body: serde_json::Value = serde_json::from_str(&card.body).unwrap();
    assert!(body["media"]["file_info"].is_string(), "应当走上传: {body}");
    // 文件名按扩展名推断：`.png` → `daily.png`。猜错会让平台拒收。
    assert!(!body["media"]["file_info"].as_str().unwrap_or_default().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 上游用 `code` 表达失败时要回话，不能静默。
#[tokio::test]
async fn daily_reports_upstream_failure() {
    let mock = MockServer::builder().daily_broken(true).start().await;
    let (dispatcher, _store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"DL_2","author":{"member_openid":"U1"},"content":"日报","group_openid":"GDL2"}"#,
    )
    .await;

    let send = mock
        .find(|h| h.path == "/v2/groups/GDL2/messages")
        .expect("失败也要回话，静默会让用户以为没收到命令");
    let body: serde_json::Value = serde_json::from_str(&send.body).unwrap();
    let text = body["content"].as_str().unwrap_or_default();
    assert!(text.contains("失败"), "要说清楚失败了: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 带 `三角洲` 前缀的四个别名也要能触发。
///
/// 它们是隐藏监听器（不进帮助），存在的意义是避免与其它插件的词撞车 ——
/// 所以「不显示」不等于「不工作」，这两件事都要有测试兜着。
#[tokio::test]
async fn delta_prefixed_aliases_work() {
    let mock = MockServer::start().await;
    let (dispatcher, _store, dir) = bili_stack(&mock).await;

    for (id, cmd, group) in [
        ("DFA_1", "三角洲集市", "GDFA"),
        ("DFA_2", "三角洲脑机", "GDFA2"),
        ("DFA_3", "三角洲密码", "GDFA3"),
        ("DFA_4", "三角洲一图流", "GDFA4"),
    ] {
        let msg = format!(
            r#"{{"id":"{id}","author":{{"member_openid":"U1"}},"content":"{cmd}","group_openid":"{group}"}}"#
        );
        feed(&dispatcher, "GROUP_MESSAGE_CREATE", &msg).await;

        let hit = mock
            .all(|h| h.path == format!("/v2/groups/{group}/messages"))
            .into_iter()
            .any(|h| {
                let body: serde_json::Value = serde_json::from_str(&h.body).unwrap_or_default();
                body["msg_type"] == 7
            });
        assert!(hit, "{cmd} 应当回一张卡片");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// B9–B15：三种地图写法 + 速查图，都应当把图发出来。
#[tokio::test]
async fn tarkov_images_accept_the_three_map_forms() {
    let mock = MockServer::start().await;
    let (dispatcher, _store, dir) = bili_stack(&mock).await;

    // cq-bot 的原样写法带空格，另两种是不带空格的变体。
    for (id, cmd, group) in [
        ("TI_1", "地图 海关", "GTI1"),
        ("TI_2", "地图海关", "GTI2"),
        ("TI_3", "海关地图", "GTI3"),
    ] {
        let msg = format!(
            r#"{{"id":"{id}","author":{{"member_openid":"U1"}},"content":"{cmd}","group_openid":"{group}"}}"#
        );
        feed(&dispatcher, "GROUP_MESSAGE_CREATE", &msg).await;

        let hit = mock
            .all(|h| h.path == format!("/v2/groups/{group}/messages"))
            .into_iter()
            .any(|h| {
                let body: serde_json::Value = serde_json::from_str(&h.body).unwrap_or_default();
                body["msg_type"] == 7
            });
        assert!(hit, "{cmd} 应当把图发出来");
    }

    // 7 个速查图走整串精确匹配。
    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"TI_4","author":{"member_openid":"U1"},"content":"任务流程图","group_openid":"GTI4"}"#,
    )
    .await;
    let hit = mock
        .all(|h| h.path == "/v2/groups/GTI4/messages")
        .into_iter()
        .any(|h| {
            let body: serde_json::Value = serde_json::from_str(&h.body).unwrap_or_default();
            body["msg_type"] == 7
        });
    assert!(hit, "速查图应当把图发出来");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 地图名单独出现**不该**发图 —— cq-bot 是子串匹配，那在群里太吵。
#[tokio::test]
async fn bare_map_name_does_not_send_an_image() {
    let mock = MockServer::start().await;
    let (dispatcher, _store, dir) = bili_stack(&mock).await;

    feed(
        &dispatcher,
        "GROUP_MESSAGE_CREATE",
        r#"{"id":"TI_5","author":{"member_openid":"U1"},"content":"今天海关真难打","group_openid":"GTI5"}"#,
    )
    .await;

    assert!(
        mock.all(|h| h.path == "/v2/groups/GTI5/messages").is_empty(),
        "闲聊不该触发静态图"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
