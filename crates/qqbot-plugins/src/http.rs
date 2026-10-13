//! 插件共用的 HTTP 客户端。
//!
//! 集中在一处，是为了让「所有对外请求必须有超时」**结构性地**成立 ——
//! 无超时的 `await` 是缺陷而不是风格问题，但它很容易在新插件里被忘掉。
//!
//! 它同时是插件**唯一**的对外 HTTP 入口：UA、非 2xx、空体、大小上限与
//! 传输层失败都收在内部，各插件只保留自己的语义前缀（`下载失败：` /
//! `查询失败：` …）。收益是双份的 —— 新插件不必各自复制一遍状态码判断，
//! 而这四类失败终于有进程内测试可打（见本文件 `tests`：本机回环服务器，
//! 不依赖外网）。

use std::time::Duration;

use serde::de::DeserializeOwned;

/// 插件对外请求的统一超时。
///
/// 比纯 API 调用宽松：日报这类功能要把整张图片下载下来。
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// 插件对外请求的统一 UA。
///
/// 由 [`build_client`] 装在 client 上，所以所有请求都自动携带 ——
/// 调用点**不要**再逐个手写 `.header("User-Agent", …)`：漏一处就会被上游按
/// 默认 UA 拦掉（tarkov 状态站就按 UA 拦）。
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) qqbot-rs";

/// 图片 / 附件的默认大小上限（32 MiB）。
///
/// 实测常见图 2.3MB，留足余量但挡住异常大文件。原来这个数字在 `ba.rs` 与
/// `caption.rs` 各写了一遍、语义完全相同，所以收在这里。
pub const DEFAULT_IMAGE_MAX_BYTES: usize = 32 * 1024 * 1024;

/// 插件对外 HTTP 的失败原因。
///
/// `Display` 就是给用户看的那句话（各插件在它前面再拼自己的语义前缀），
/// 所以文案是契约的一部分：改它会改变用户可见行为。
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// 传输层失败：连不上、超时、TLS 握手失败等。
    #[error("请求失败：{0}")]
    Request(reqwest::Error),
    /// 上游返回非 2xx。把上游状态码丢给用户没有意义，只报数字。
    #[error("返回 HTTP {0}")]
    Status(u16),
    /// 响应体读取失败（连接中途断开、gzip 解码失败等）。
    #[error("读取响应失败：{0}")]
    Body(reqwest::Error),
    /// 下载到 0 字节。图片插件必须挡住它，否则会发一张空图。
    #[error("下载内容为空")]
    Empty,
    /// 下载内容超过调用方给的 limit。
    #[error("下载内容过大：{size} 字节，上限 {limit} 字节")]
    TooLarge { size: usize, limit: usize },
    /// 响应读到了、但反序列化失败。
    ///
    /// 不能并进 [`FetchError::Empty`]：上游改了字段名是最需要被诊断出来的一类
    /// 故障，伪装成「下载内容为空」会让人往网络方向查。
    #[error("响应内容无法解析：{0}")]
    Parse(#[from] serde_json::Error),
}

/// 构建插件共享的 HTTP 客户端。
///
/// # Errors
///
/// TLS 后端初始化失败时返回错误。这是启动期的一次性失败，
/// 应当让进程直接起不来，而不是留一个「每次请求才报错」的机器人。
pub fn build_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder().timeout(TIMEOUT).user_agent(USER_AGENT).build()
}

/// 发送请求，并把「传输失败」与「非 2xx」归一成 [`FetchError`]。
///
/// 需要原始 [`reqwest::Response`] 的两个调用点走这里：`delta` 要读 `Set-Cookie`
/// 维护会话，`bili::resolve_short` 要拿重定向后的 `res.url()`。其余调用点用
/// [`fetch_text`] / [`fetch_json`] / [`fetch_bytes`]。
///
/// # Errors
///
/// - [`FetchError::Request`]：请求没发出去或中途失败；
/// - [`FetchError::Status`]：上游返回非 2xx（3xx 也算 —— client 默认跟重定向，
///   只有跟不动或超过上限时才会以 3xx 出现在这里）。
pub async fn send(req: reqwest::RequestBuilder) -> Result<reqwest::Response, FetchError> {
    let res = req.send().await.map_err(FetchError::Request)?;
    let status = res.status();
    if !status.is_success() {
        return Err(FetchError::Status(status.as_u16()));
    }
    Ok(res)
}

/// GET 一个地址并读成文本。
///
/// # Errors
///
/// 见 [`send`]，外加 [`FetchError::Body`]（读响应体失败）。
pub async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String, FetchError> {
    let res = send(client.get(url)).await?;
    res.text().await.map_err(FetchError::Body)
}

/// GET 一个地址并把 JSON 反序列化成 `T`。
///
/// # Errors
///
/// 见 [`fetch_text`]，外加 [`FetchError::Parse`]（响应读到了但不是合法 JSON，
/// 或者结构与 `T` 对不上）。
pub async fn fetch_json<T: DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, FetchError> {
    let body = fetch_text(client, url).await?;
    Ok(serde_json::from_str(&body)?)
}

/// GET 一个地址并读成字节，带空体与大小上限守卫。
///
/// `limit` 由调用方给：资源收录的预检用的是附件**声明**的大小，那只有调用方
/// 知道；下载后的兜底检查才在这里。
///
/// # Errors
///
/// 见 [`send`]，外加 [`FetchError::Body`]、[`FetchError::Empty`]、
/// [`FetchError::TooLarge`]。
pub async fn fetch_bytes(
    client: &reqwest::Client,
    url: &str,
    limit: usize,
) -> Result<Vec<u8>, FetchError> {
    let res = send(client.get(url)).await?;
    let bytes = res.bytes().await.map_err(FetchError::Body)?;
    if bytes.is_empty() {
        return Err(FetchError::Empty);
    }
    if bytes.len() > limit {
        return Err(FetchError::TooLarge { size: bytes.len(), limit });
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    //! 传输层从零覆盖变成有覆盖。
    //!
    //! 全部打**本机回环**：起一个只回一次响应的最小 TCP 服务器，
    //! 因此不需要外网、不需要新依赖，也不会偶发失败。

    use std::time::Duration;

    use serde::Deserialize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// 拼一个固定响应的原始 HTTP 报文。
    fn response(status: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
        let mut raw = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
        for (name, value) in headers {
            raw.push_str(&format!("{name}: {value}\r\n"));
        }
        raw.push_str("\r\n");
        raw.push_str(body);
        raw.into_bytes()
    }

    /// 起一个回环服务器，接受**一次**连接，把 `reply` 发回去。
    ///
    /// 返回 `(base_url, handle)`：handle 用 `await` 收尾，`expect` 一下才能
    /// 让闭包里的断言失败冒出来（spawn 出去的 panic 不会自己传播）。
    async fn one_shot(
        reply: impl FnOnce(&str) -> Vec<u8> + Send + 'static,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定回环端口");
        let addr = listener.local_addr().expect("取本地地址");
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("接受连接");
            // 读请求头：读到空行就够了，测试里没有请求体。
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf)).await {
                    Ok(Ok(0)) | Err(_) => break,
                    Ok(Ok(n)) => head.extend_from_slice(&buf[..n]),
                    Ok(Err(err)) => panic!("读请求失败：{err}"),
                }
            }
            let text = String::from_utf8_lossy(&head).into_owned();
            socket.write_all(&reply(&text)).await.expect("写响应");
            socket.flush().await.expect("刷响应");
        });
        (format!("http://{addr}"), handle)
    }

    /// 只回一个固定响应的地址。
    async fn serve(status: &str, body: &str) -> (String, tokio::task::JoinHandle<()>) {
        let status = status.to_string();
        let body = body.to_string();
        one_shot(move |_| response(&status, &[], &body)).await
    }

    fn client() -> reqwest::Client {
        build_client().expect("构建测试客户端")
    }

    #[test]
    fn error_display_is_the_user_facing_sentence() {
        // 文案是契约：各插件在它前面拼自己的语义前缀，所以形状必须固定。
        assert_eq!(FetchError::Status(404).to_string(), "返回 HTTP 404");
        assert_eq!(FetchError::Empty.to_string(), "下载内容为空");
        assert_eq!(
            FetchError::TooLarge { size: 40, limit: 32 }.to_string(),
            "下载内容过大：40 字节，上限 32 字节"
        );
        let parse = FetchError::from(serde_json::from_str::<i32>("不是 JSON").unwrap_err());
        assert!(parse.to_string().starts_with("响应内容无法解析："), "{parse}");
    }

    #[tokio::test]
    async fn non_success_status_is_a_status_error() {
        let (url, handle) = serve("404 Not Found", "没有这个页面").await;
        let err = fetch_text(&client(), &url).await.expect_err("404 必须报错");
        assert!(matches!(err, FetchError::Status(404)), "{err:?}");
        assert_eq!(err.to_string(), "返回 HTTP 404");
        handle.await.expect("mock 服务器任务不应 panic");
    }

    #[tokio::test]
    async fn transport_failure_is_a_request_error() {
        // 1 号端口不会有服务在听，连接会被立刻拒绝 —— 不依赖外网也能稳定复现。
        let err = fetch_text(&client(), "http://127.0.0.1:1")
            .await
            .expect_err("连不上必须报错");
        assert!(matches!(err, FetchError::Request(_)), "{err:?}");
        assert!(err.to_string().starts_with("请求失败："), "{err}");
    }

    #[tokio::test]
    async fn empty_body_is_an_empty_error() {
        let (url, handle) = serve("200 OK", "").await;
        let err = fetch_bytes(&client(), &url, 32)
            .await
            .expect_err("0 字节必须报错");
        assert!(matches!(err, FetchError::Empty), "{err:?}");
        assert_eq!(err.to_string(), "下载内容为空");
        handle.await.expect("mock 服务器任务不应 panic");
    }

    #[tokio::test]
    async fn body_over_limit_is_a_too_large_error() {
        let (url, handle) = serve("200 OK", "0123456789").await;
        let err = fetch_bytes(&client(), &url, 4)
            .await
            .expect_err("超过上限必须报错");
        match err {
            FetchError::TooLarge { size, limit } => {
                assert_eq!((size, limit), (10, 4));
            }
            other => panic!("应当是 TooLarge，实际 {other:?}"),
        }
        handle.await.expect("mock 服务器任务不应 panic");
    }

    #[tokio::test]
    async fn text_body_comes_back_verbatim() {
        let (url, handle) = serve("200 OK", "更新于 2026-09-29").await;
        let body = fetch_text(&client(), &url).await.expect("应当读到正文");
        assert_eq!(body, "更新于 2026-09-29");
        handle.await.expect("mock 服务器任务不应 panic");
    }

    #[tokio::test]
    async fn json_body_deserializes() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Payload {
            code: i64,
            #[serde(default)]
            name: String,
        }
        let (url, handle) = serve("200 OK", r#"{"code":200,"name":"爱丽丝"}"#).await;
        let parsed: Payload = fetch_json(&client(), &url).await.expect("应当反序列化成功");
        assert_eq!(parsed, Payload { code: 200, name: "爱丽丝".to_string() });
        handle.await.expect("mock 服务器任务不应 panic");
    }

    #[tokio::test]
    async fn json_that_does_not_match_is_a_parse_error() {
        // 上游改了字段名 / 少了字段时，必须说「解析不了」，不能说「内容为空」——
        // 后者会把人往网络方向带。
        #[derive(Debug, Deserialize)]
        struct Payload {
            #[allow(dead_code)]
            name: String,
        }
        let (url, handle) = serve("200 OK", r#"{"code":200}"#).await;
        let err = fetch_json::<Payload>(&client(), &url).await.expect_err("缺字段必须报错");
        assert!(matches!(err, FetchError::Parse(_)), "{err:?}");
        assert!(err.to_string().starts_with("响应内容无法解析："), "{err}");
        handle.await.expect("mock 服务器任务不应 panic");
    }

    #[tokio::test]
    async fn bytes_under_limit_come_back() {
        let (url, handle) = serve("200 OK", "abcd").await;
        let bytes = fetch_bytes(&client(), &url, 4).await.expect("等于上限应当放行");
        assert_eq!(bytes, b"abcd");
        handle.await.expect("mock 服务器任务不应 panic");
    }

    #[tokio::test]
    async fn send_keeps_the_response_headers() {
        // delta 靠这条读 Set-Cookie，bili::resolve_short 靠 res.url() 拿重定向结果。
        let (url, handle) = one_shot(|_| {
            response("200 OK", &[("Set-Cookie", "PHPSESSID=abc; Path=/")], "ok")
        })
        .await;
        let res = send(client().get(&url)).await.expect("应当拿到响应");
        let cookie = res
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(cookie.starts_with("PHPSESSID=abc"), "{cookie}");
        assert_eq!(res.text().await.expect("读正文"), "ok");
        handle.await.expect("mock 服务器任务不应 panic");
    }

    #[tokio::test]
    async fn client_carries_the_shared_user_agent() {
        // UA 现在只装一次（在 client 上）。这条盯住「别再退回逐请求手写」。
        let (url, handle) = one_shot(|head| {
            let ua = head
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("user-agent:"))
                .unwrap_or_default()
                .to_string();
            assert!(ua.contains(USER_AGENT), "请求里应当带上统一 UA：{ua}");
            response("200 OK", &[], "ok")
        })
        .await;
        let _ = fetch_text(&client(), &url).await.expect("应当读到正文");
        handle.await.expect("mock 服务器任务不应 panic");
    }
}