//! 日报（早报）插件。
//!
//! 上游返回的是一张包含当日全部新闻的**长图**，本插件把它下载后转成
//! `file_info` 再发送 —— 官方 API 不接受远程 URL 直发，也不能发本地路径。
//!
//! 缓存是刻意的：早报一天只更新一次，而用户可能在短时间内反复触发，
//! 每一次触发都会消耗一次上游配额。
//!
//! 上游的 429 有两个来源，都得处理，而且它的 429 是**业务码**
//! （HTTP 仍是 200），只有读 body 才看得见：
//!
//! 1. **不带 `Accept-Encoding` 的请求一律 429**。这是 2026-11「日报一直提示
//!    请求次数过多」的真凶：同一个 token、同一个 IP，只要请求头里声明接受压缩
//!    就稳定 200。修复在共享客户端上（见 `http::build_client` 的文档），
//!    本文件不需要为此做任何事 —— 但要知道，否则会误以为是频率问题。
//! 2. **真实的频率限制**：连打两次必 429，隔 1 秒就恢复。见 `RATE_LIMIT_BACKOFF`。
//!
//! 两道保险都撞穿了也还有一层：重试用尽后**退回上一次成功的图**（见 `image`）。
//! 早报一天只更新一次，同一进程里那张在当天就是正确内容 ——
//! 一条限流不该把「已经拿到的今日早报」变成一句报错。

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use serde::Deserialize;
use tokio::sync::Mutex;

/// 上游用来表达「限流」的业务码。注意它是响应体里的 `code`，不是 HTTP 状态码。
const RATE_LIMITED_CODE: i64 = 429;

/// 撞上限流后的重试间隔。
///
/// 2026-11 实测：同一个 token 连打两次，第二次必 429，隔 1 秒就恢复；
/// 所以等 1 秒基本必中，3 秒是给「别人也在用同一个 token」留的余量。
/// 总等待 4 秒，远小于群聊被动窗口（5 分钟），用户感知不到。
const RATE_LIMIT_BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

/// 日报插件配置。`token` 为空时该插件**不会被注册**。
#[derive(Debug, Clone)]
pub struct DailyConfig {
    /// 接口地址（不含 token）。
    pub api_url: String,
    /// 接口令牌。
    pub token: String,
    /// 结果缓存时长。
    pub cache: Duration,
}

/// 上游响应。只声明需要的字段 —— 上游随时可能加字段，
/// 用 `deny_unknown_fields` 会让一次无害的上游升级变成机器人故障。
#[derive(Debug, Deserialize)]
struct DailyResponse {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    data: Option<DailyData>,
}

#[derive(Debug, Deserialize)]
struct DailyData {
    /// 整张早报长图。
    #[serde(default)]
    image: Option<String>,
    /// 头图，作为 `image` 缺失时的退路。
    #[serde(default)]
    head_image: Option<String>,
}

/// 接口调用失败的原因。
///
/// `message` 就是给用户看的那句话（与 `http::FetchError` 同一个契约）；
/// `rate_limited` 决定上层要不要退避重试 —— 只有限流值得等，
/// token 无效 / 没有图片地址这类错误重试多少次都一样。
#[derive(Debug)]
pub struct ApiFailure {
    /// 给用户看的那句话。
    pub message: String,
    /// 是不是上游限流 —— 决定上层要不要退避重试。
    pub rate_limited: bool,
}

/// 缓存下来的图片。
struct Cached {
    bytes: Arc<Vec<u8>>,
    file_name: String,
    at: Instant,
}

/// 日报插件。
pub struct DailyPlugin {
    config: DailyConfig,
    http: reqwest::Client,
    /// 限流重试的等待序列，生产固定为 [`RATE_LIMIT_BACKOFF`]。
    /// 做成字段只为让测试用 0 秒跑完重试路径，不必真的睡 4 秒。
    retry_delays: Vec<Duration>,
    /// 只缓存一份结果。用 `Mutex` 而不是无锁结构：并发触发时只让一个请求
    /// 打到上游，其余在锁上等待后直接命中缓存。
    cache: Mutex<Option<Cached>>,
}

impl DailyPlugin {
    pub fn new(config: DailyConfig, http: reqwest::Client) -> Self {
        Self {
            config,
            http,
            retry_delays: RATE_LIMIT_BACKOFF.to_vec(),
            cache: Mutex::new(None),
        }
    }

    /// 测试用：换掉限流退避序列（`&[]` 表示不重试）。
    #[cfg(test)]
    fn with_retry_delays(mut self, delays: &[Duration]) -> Self {
        self.retry_delays = delays.to_vec();
        self
    }

    /// 取早报图片，命中缓存则直接返回。
    ///
    /// 拉取失败时**退回上一次成功的那张**（哪怕已经过期）：早报一天只更新一次，
    /// 同一个进程里缓存的那张在当天就是正确内容，而这里失败的原因几乎都是
    /// 上游限流 —— 为了一条限流把「已经拿到的今天早报」换成一句报错是最差的选择。
    /// 只有进程起来后一次都没成功过，才会真的把错误交给用户。
    async fn image(&self) -> Result<(Arc<Vec<u8>>, String), String> {
        let mut guard = self.cache.lock().await;
        if let Some(hit) = guard.as_ref()
            && hit.at.elapsed() < self.config.cache
        {
            return Ok((Arc::clone(&hit.bytes), hit.file_name.clone()));
        }

        match self.fetch().await {
            Ok((bytes, file_name)) => {
                let bytes = Arc::new(bytes);
                *guard = Some(Cached {
                    bytes: Arc::clone(&bytes),
                    file_name: file_name.clone(),
                    at: Instant::now(),
                });
                Ok((bytes, file_name))
            }
            Err(reason) => match guard.as_ref() {
                Some(stale) => {
                    tracing::warn!(
                        error = %reason,
                        age_secs = stale.at.elapsed().as_secs(),
                        "日报拉取失败，退回上次成功的图"
                    );
                    Ok((Arc::clone(&stale.bytes), stale.file_name.clone()))
                }
                None => Err(reason),
            },
        }
    }

    /// 拉取并解析上游数据，返回图片字节与文件名。
    ///
    /// 接口这一步在限流时重试（见 [`RATE_LIMIT_BACKOFF`]）。重试期间仍然持有
    /// 缓存锁：这个窗口只有 1 秒级，并发触发的几条「日报」若各打一次上游，
    /// 只会把窗口撞得更满 —— 让它们排队，第二个开始就命中缓存。
    async fn fetch(&self) -> Result<(Vec<u8>, String), String> {
        let mut attempt = 0;
        let image_url = loop {
            match self.fetch_image_url().await {
                Ok(url) => break url,
                Err(failure) => {
                    let Some(delay) = retry_delay(&self.retry_delays, attempt, failure.rate_limited)
                    else {
                        return Err(failure.message);
                    };
                    tracing::info!(
                        attempt = attempt + 1,
                        delay_ms = delay.as_millis() as u64,
                        error = %failure.message,
                        "日报接口限流，退避后重试"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        };
        let file_name = file_name_from_url(&image_url);

        // 图片与接口不是同一台主机、也不带 token，限流与它无关；
        // 早报是长图，用默认的图片上限兜底。
        let bytes =
            crate::http::fetch_bytes(&self.http, &image_url, crate::http::DEFAULT_IMAGE_MAX_BYTES)
                .await
                .map_err(|err| err.to_string())?;
        Ok((bytes, file_name))
    }

    /// 一次接口调用：拿图片地址。失败原因里带「是否限流」，供上层决定重试。
    async fn fetch_image_url(&self) -> Result<String, ApiFailure> {
        let url = with_token(&self.config.api_url, &self.config.token);
        let body = crate::http::fetch_text(&self.http, &url).await.map_err(|err| {
            // 上游偶尔也用 HTTP 429 表达同一件事，一并归到「可重试」。
            let rate_limited = matches!(err, crate::http::FetchError::Status(429));
            ApiFailure { message: err.to_string(), rate_limited }
        })?;
        pick_image_url(&body)
    }
}

/// 第 `attempt` 次失败后应当等多久再试；`None` 表示不再重试。
///
/// 抽成纯函数是为了让「只有限流才重试」「重试次数有上界」这两条能脱离网络单测。
fn retry_delay(delays: &[Duration], attempt: usize, rate_limited: bool) -> Option<Duration> {
    if !rate_limited {
        return None;
    }
    delays.get(attempt).copied()
}

#[async_trait]
impl Handler for DailyPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        match self.image().await {
            Ok((bytes, file_name)) => {
                if let Err(err) = ctx.reply.image_named(&file_name, &bytes).await {
                    tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "日报发送失败");
                }
            }
            Err(reason) => {
                tracing::warn!(error = %reason, "日报获取失败");
                // 失败也要回话：静默失败会让用户以为机器人没收到命令。
                if let Err(err) = ctx.reply.text(format!("日报获取失败：{reason}")).await {
                    tracing::warn!(error = %err, "日报错误提示发送失败");
                }
            }
        }
        Handled::Consumed
    }

    fn name(&self) -> &'static str {
        "日报"
    }
}

/// 把 token 拼到接口地址上。
///
/// 地址里已经有 `?` 时用 `&` —— 写死 `?token=` 会让形如
/// `...?format=json` 的地址拼出两个 `?`，服务端直接当成参数名的一部分。
pub fn with_token(api_url: &str, token: &str) -> String {
    let sep = if api_url.contains('?') { '&' } else { '?' };
    format!("{api_url}{sep}token={token}")
}

/// 从响应体里挑出早报图片地址。
///
/// 优先 `data.image`（整张早报长图），退回 `data.head_image`（头图）。
pub fn pick_image_url(body: &str) -> Result<String, ApiFailure> {
    let not_retryable = |message: String| ApiFailure { message, rate_limited: false };

    let parsed: DailyResponse = serde_json::from_str(body)
        .map_err(|err| not_retryable(format!("响应解析失败：{err}")))?;

    // 上游失败时 HTTP 仍是 200，必须看 body 里的 code —— 与官方 API 同一个坑。
    if let Some(code) = parsed.code
        && code != 200
    {
        let msg = parsed.message.unwrap_or_else(|| "未知错误".to_string());
        // 429 是限流，值得等一秒再试；其余业务码重试没有意义。
        return Err(ApiFailure {
            message: format!("接口返回 code={code}：{msg}"),
            rate_limited: code == RATE_LIMITED_CODE,
        });
    }

    let data = parsed.data.ok_or_else(|| not_retryable("响应缺少 data 字段".to_string()))?;
    let url = data.image.or(data.head_image).unwrap_or_default();
    let url = url.trim();
    if url.is_empty() {
        return Err(not_retryable("响应里没有图片地址".to_string()));
    }
    Ok(url.to_string())
}

/// 从 URL 推断文件名。
///
/// 平台按文件名判定图片格式，猜错会让图片发不出去，所以只认已知扩展名，
/// 其余一律退回 PNG。
pub fn file_name_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let ext = path.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "daily.jpg".to_string(),
        "webp" => "daily.webp".to_string(),
        "gif" => "daily.gif".to_string(),
        _ => "daily.png".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实响应的裁剪版（2026-09-29 实测）。
    const FIXTURE: &str = r#"{"request_id":"963459927030038529","success":true,"message":"success","code":200,"data":{"date":"2026-09-29","news":["1、xxx"],"weiyu":"【微语】xxx","image":"https:\/\/file.alapi.cn\/60s\/202609281790613210.png","audio":"https:\/\/file.alapi.cn\/60s\/audio\/x.mp3","head_image":"https:\/\/file.alapi.cn\/60s\/202609281790613210_head.png"},"time":1790666746,"usage":0}"#;

    #[test]
    fn appends_token_with_correct_separator() {
        assert_eq!(with_token("https://a/b", "T"), "https://a/b?token=T");
        assert_eq!(with_token("https://a/b?format=json", "T"), "https://a/b?format=json&token=T");
    }

    #[test]
    fn picks_image_from_real_response() {
        let url = pick_image_url(FIXTURE).unwrap();
        assert_eq!(url, "https://file.alapi.cn/60s/202609281790613210.png");
    }

    #[test]
    fn falls_back_to_head_image() {
        let body = r#"{"code":200,"data":{"head_image":"https://x/h.png"}}"#;
        assert_eq!(pick_image_url(body).unwrap(), "https://x/h.png");
    }

    #[test]
    fn reports_upstream_business_error() {
        // HTTP 200 但业务失败：必须报错，不能当成成功。
        let body = r#"{"code":400,"message":"token 无效","data":null}"#;
        let err = pick_image_url(body).unwrap_err();
        assert!(err.message.contains("code=400"), "{}", err.message);
        assert!(err.message.contains("token 无效"), "{}", err.message);
    }

    /// 429 是唯一值得重试的业务码 —— 上游的限流窗口只有 1 秒级。
    #[test]
    fn only_rate_limiting_is_retryable() {
        let limited = pick_image_url(r#"{"code":429,"message":"请求次数过多，请稍后再试"}"#)
            .unwrap_err();
        assert!(limited.rate_limited, "{limited:?}");
        // 文案是契约：用户看到的那句话不能因为内部重组而改变。
        assert_eq!(limited.message, "接口返回 code=429：请求次数过多，请稍后再试");

        for body in [
            r#"{"code":400,"message":"token 无效"}"#,
            r#"{"code":500,"message":"服务异常"}"#,
            r#"{"code":200,"data":{}}"#,
            r#"{"code":200}"#,
            "not json",
        ] {
            let err = pick_image_url(body).unwrap_err();
            assert!(!err.rate_limited, "只有 429 可以重试：{body} → {err:?}");
        }
    }

    /// 重试次数有上界：退避序列用尽就必须把错误交出去，不能无限重试。
    #[test]
    fn retry_budget_is_bounded() {
        let delays = RATE_LIMIT_BACKOFF;
        for attempt in 0..delays.len() {
            assert_eq!(
                retry_delay(&delays, attempt, true),
                Some(delays[attempt]),
                "第 {attempt} 次失败应当按序列退避"
            );
        }
        assert_eq!(retry_delay(&delays, delays.len(), true), None, "用尽后不再重试");
        // 非限流错误一次都不重试。
        for attempt in 0..delays.len() {
            assert_eq!(retry_delay(&delays, attempt, false), None);
        }
    }

    #[test]
    fn rejects_missing_image() {
        assert!(pick_image_url(r#"{"code":200,"data":{"date":"x"}}"#).is_err());
        assert!(pick_image_url(r#"{"code":200}"#).is_err());
        assert!(pick_image_url("not json").is_err());
    }

    #[test]
    fn infers_file_name_from_extension() {
        assert_eq!(file_name_from_url("https://x/a.png"), "daily.png");
        assert_eq!(file_name_from_url("https://x/a.JPG"), "daily.jpg");
        assert_eq!(file_name_from_url("https://x/a.webp?sign=1"), "daily.webp");
        assert_eq!(file_name_from_url("https://x/a.gif#f"), "daily.gif");
        // 没有扩展名 / 只有主机名里的点 → 退回 PNG
        assert_eq!(file_name_from_url("https://x.y/z"), "daily.png");
        assert_eq!(file_name_from_url(""), "daily.png");
    }
    /// 起一个回环服务器：每个连接回一条响应体，回完立刻断开。
    ///
    /// 刻意带 `Connection: close` —— 这里每次都是「用完即断」，
    /// 留着 keep-alive 会让第二个请求落到已经关闭的 socket 上。
    async fn scripted_server(
        build: impl FnOnce(&str) -> Vec<String> + Send + 'static,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("绑定回环端口");
        let addr = listener.local_addr().expect("取本地地址");
        let base = format!("http://{addr}");
        let replies = build(&base);
        let handle = tokio::spawn(async move {
            for reply in replies {
                let (mut socket, _) = listener.accept().await.expect("接受连接");
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf)).await {
                        Ok(Ok(0)) | Err(_) => break,
                        Ok(Ok(n)) => head.extend_from_slice(&buf[..n]),
                        Ok(Err(err)) => panic!("读请求失败：{err}"),
                    }
                }
                let raw = format!(
                    "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{reply}",
                    reply.len()
                );
                socket.write_all(raw.as_bytes()).await.expect("写响应");
                socket.flush().await.expect("刷响应");
            }
        });
        (base, handle)
    }

    /// 退避路径的端到端覆盖：第一次 429、第二次 200、第三次取图 —— 应当成功。
    ///
    /// 退避序列传 0 秒：真睡 1 秒的重试没人愿意在测试里等，
    /// 而这里要盯的是「重试真的发生了」而不是「等得准不准」。
    #[tokio::test]
    async fn retries_after_rate_limiting() {
        let (base, handle) = scripted_server(|base| {
            vec![
                // 上游用业务码表达限流，HTTP 仍是 200 —— 这正是线上的那个 429。
                r#"{"code":429,"message":"请求次数过多，请稍后再试"}"#.to_string(),
                format!(r#"{{"code":200,"data":{{"image":"{base}/daily.png"}}}}"#),
                "FAKE-DAILY-LONG-IMAGE".to_string(),
            ]
        })
        .await;

        let plugin = DailyPlugin::new(
            DailyConfig {
                api_url: format!("{base}/api/zaobao"),
                token: "test-token".to_string(),
                cache: Duration::from_secs(60),
            },
            crate::http::build_client().expect("构建 HTTP 客户端"),
        )
        .with_retry_delays(&[Duration::ZERO]);

        let (bytes, name) = plugin.fetch().await.expect("限流之后应当重试成功");
        assert_eq!(bytes, b"FAKE-DAILY-LONG-IMAGE");
        assert_eq!(name, "daily.png");
        handle.await.expect("mock 服务器任务不应 panic");
    }

    /// 重试用尽后退回上一次成功的图 —— 用户不该因为一条限流就拿不到早报。
    ///
    /// `cache` 传 0 秒：每次调用都会判定过期、必须去刷新，
    /// 这样才能把「刷新失败 → 退回旧图」这段单独逼出来。
    #[tokio::test]
    async fn serves_the_stale_image_when_refreshing_fails() {
        let limited = r#"{"code":429,"message":"请求次数过多，请稍后再试"}"#.to_string();
        let (base, handle) = scripted_server(move |base| {
            vec![
                // 第一次：正常拿到图。
                format!(r#"{{"code":200,"data":{{"image":"{base}/a.png"}}}}"#),
                "FIRST-IMAGE".to_string(),
                // 第二次：接口限流，重试一次仍是限流。
                limited.clone(),
                limited.clone(),
            ]
        })
        .await;

        let plugin = DailyPlugin::new(
            DailyConfig {
                api_url: format!("{base}/api/zaobao"),
                token: "test-token".to_string(),
                cache: Duration::ZERO,
            },
            crate::http::build_client().expect("构建 HTTP 客户端"),
        )
        .with_retry_delays(&[Duration::ZERO]);

        let (first, name) = plugin.image().await.expect("第一次应当成功");
        assert_eq!(first.as_slice(), b"FIRST-IMAGE");
        assert_eq!(name, "daily.png");

        let (stale, name) = plugin.image().await.expect("刷新失败也要把上次的图发出去");
        assert_eq!(stale.as_slice(), b"FIRST-IMAGE");
        assert_eq!(name, "daily.png");
        handle.await.expect("mock 服务器任务不应 panic");
    }

    /// 非限流错误不许重试：token 无效重试一万次也还是无效。
    ///
    /// 服务器只准备了**一条**响应 —— 真重试了就会连到已关闭的端口，
    /// 错误文案会变成「请求失败：…」而不是上游那句话。
    #[tokio::test]
    async fn does_not_retry_business_errors() {
        let (base, handle) =
            scripted_server(|_| vec![r#"{"code":400,"message":"token 无效"}"#.to_string()]).await;

        let plugin = DailyPlugin::new(
            DailyConfig {
                api_url: format!("{base}/api/zaobao"),
                token: "bad".to_string(),
                cache: Duration::from_secs(60),
            },
            crate::http::build_client().expect("构建 HTTP 客户端"),
        )
        .with_retry_delays(&[Duration::ZERO, Duration::ZERO]);

        let err = plugin.fetch().await.unwrap_err();
        assert!(err.contains("code=400"), "应当保留上游错误：{err}");
        handle.await.expect("mock 服务器任务不应 panic");
    }

    /// 重试用尽后仍要把上游那句话交给用户 —— 不能变成一句「未知错误」。
    #[tokio::test]
    async fn gives_up_after_retries_are_exhausted() {
        let limited = r#"{"code":429,"message":"请求次数过多，请稍后再试"}"#.to_string();
        let (base, handle) =
            scripted_server(move |_| vec![limited.clone(), limited.clone(), limited]).await;

        let plugin = DailyPlugin::new(
            DailyConfig {
                api_url: format!("{base}/api/zaobao"),
                token: "test-token".to_string(),
                cache: Duration::from_secs(60),
            },
            crate::http::build_client().expect("构建 HTTP 客户端"),
        )
        .with_retry_delays(&[Duration::ZERO, Duration::ZERO]);

        let err = plugin.fetch().await.unwrap_err();
        assert!(err.contains("code=429"), "应当把上游文案交出去：{err}");
        handle.await.expect("mock 服务器任务不应 panic");
    }

    /// 真实网络冒烟（限流重试版）：连续拉两次必须**都**成功。
    ///
    /// 上游的免费 token 是 1 秒级限流（见模块文档），第二次请求必然先吃 429 ——
    /// 所以这条测试真正盯的是退避重试：没有它，第二次必定失败。
    /// 这也是唯一能在真实上游上验证「重试确实发生了」的地方。
    ///
    /// ```text
    /// QQBOT_DAILY_TOKEN=xxx cargo test -p qqbot-plugins live -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "需要外网与真实 token"]
    async fn live_retry_survives_rate_limiting() {
        let token = std::env::var("QQBOT_DAILY_TOKEN").expect("需要 QQBOT_DAILY_TOKEN");
        let plugin = DailyPlugin::new(
            DailyConfig {
                api_url: "https://v2.alapi.cn/api/zaobao?format=json".to_string(),
                token,
                cache: Duration::from_secs(60),
            },
            crate::http::build_client().expect("构建 HTTP 客户端"),
        );
        // 直接调 fetch（绕过缓存）：要的就是两次真实请求。
        for attempt in 1..=2 {
            match plugin.fetch().await {
                Ok((bytes, name)) => {
                    println!("第 {attempt} 次成功：{name}，{} 字节", bytes.len());
                    assert!(bytes.len() > 1024, "图片太小，多半不是真图");
                }
                Err(err) => panic!("第 {attempt} 次拉取失败（重试没生效？）：{err}"),
            }
        }
    }

    /// 真实网络冒烟测试：确认「接口 → JSON 解析 → 图片下载」这条链路真的通。
    ///
    /// 默认忽略（需要外网与真实 token）：
    /// `QQBOT_DAILY_TOKEN=xxx cargo test -p qqbot-plugins live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "需要外网与真实 token"]
    async fn live_fetch_downloads_an_image() {
        let token = std::env::var("QQBOT_DAILY_TOKEN").expect("需要 QQBOT_DAILY_TOKEN");
        let plugin = DailyPlugin::new(
            DailyConfig {
                api_url: "https://v2.alapi.cn/api/zaobao?format=json".to_string(),
                token,
                cache: Duration::from_secs(60),
            },
            crate::http::build_client().expect("构建 HTTP 客户端"),
        );
        let (bytes, name) = plugin.fetch().await.expect("拉取早报失败");
        println!("下载成功：{name}，{} 字节", bytes.len());
        assert!(bytes.len() > 1024, "图片太小，多半不是真图");
    }
}
