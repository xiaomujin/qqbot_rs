//! 插件共用的 HTTP 客户端。
//!
//! 集中在一处，是为了让「所有对外请求必须有超时」**结构性地**成立 ——
//! 无超时的 `await` 是缺陷而不是风格问题，但它很容易在新插件里被忘掉。

use std::time::Duration;

/// 插件对外请求的统一超时。
///
/// 比纯 API 调用宽松：日报这类功能要把整张图片下载下来。
const TIMEOUT: Duration = Duration::from_secs(20);

/// 构建插件共享的 HTTP 客户端。
///
/// # Errors
///
/// TLS 后端初始化失败时返回错误。这是启动期的一次性失败，
/// 应当让进程直接起不来，而不是留一个「每次请求才报错」的机器人。
pub fn build_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder().timeout(TIMEOUT).build()
}
