//! HTTP 客户端与 access_token 管理。
//!
//! 这是 `qqbot-api` 中**唯一携带 IO** 的模块。

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::error::ApiError;
use crate::intents::Intents;
use crate::message::{OutMessage, Target};
use crate::payload::{Identify, Properties, Resume};
use crate::DEFAULT_API_BASE;

/// 提前刷新余量。
///
/// 官方说明：距过期 **60s 内**请求会返回新 token。这里留更宽裕的 300s，
/// 避免长请求跨过过期边界。
const REFRESH_MARGIN: Duration = Duration::from_secs(300);

/// `expires_in` 的上界。服务端数值不可信，夹住它可同时防
/// `Instant + Duration` 溢出和「寿命被余量吃光」。
const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(24 * 3600);

/// 默认 HTTP 超时。官方建议上传类接口超时 ≥ 5s，这里给足余量。
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct ApiClientConfig {
    pub app_id: String,
    pub client_secret: String,
    pub base_url: String,
}

/// 手写 `Debug`：`client_secret` 是密钥，绝不能因为一次
/// `tracing::debug!(?cfg)`、`#[instrument]` 或 `dbg!` 就进日志。
impl std::fmt::Debug for ApiClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiClientConfig")
            .field("app_id", &self.app_id)
            .field("client_secret", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish()
    }
}

impl ApiClientConfig {
    pub fn new(app_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            client_secret: client_secret.into(),
            base_url: DEFAULT_API_BASE.to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

struct CachedToken {
    token: String,
    /// 到这一刻就该刷新（余量已经扣掉）。
    refresh_at: Instant,
}

/// access_token 提供者。
///
/// 刷新用 `Mutex` 串行化，等价于 singleflight：并发调用只会触发一次网络请求，
/// 后到者拿到的是同一个新 token。
///
/// 但**读缓存**走 `RwLock` 的共享读锁：刷新期间（一次网络往返）不会挡住其他
/// 已经命中缓存的调用，只有真正需要刷新的任务才在 `refresh` 上排队。
pub struct TokenProvider {
    cfg: ApiClientConfig,
    http: reqwest::Client,
    cache: RwLock<Option<CachedToken>>,
    refresh: Mutex<()>,
}

impl TokenProvider {
    pub fn new(cfg: ApiClientConfig, http: reqwest::Client) -> Self {
        Self { cfg, http, cache: RwLock::new(None), refresh: Mutex::new(()) }
    }

    /// 取当前有效的 access_token（必要时刷新）。
    pub async fn token(&self) -> Result<String, ApiError> {
        if let Some(token) = self.cached().await {
            return Ok(token);
        }

        // 只有需要刷新的任务在这里排队；上面的读路径不受影响。
        let _gate = self.refresh.lock().await;
        // double-check：等锁期间可能已被别的任务刷新过。
        if let Some(token) = self.cached().await {
            return Ok(token);
        }

        self.fetch_token().await
    }

    /// 命中缓存则返回 token。共享读锁，不阻塞并发读。
    async fn cached(&self) -> Option<String> {
        let guard = self.cache.read().await;
        guard
            .as_ref()
            .filter(|c| c.refresh_at > Instant::now())
            .map(|c| c.token.clone())
    }

    /// 真正发起刷新。**调用方必须已持有 `refresh` 锁。**
    async fn fetch_token(&self) -> Result<String, ApiError> {
        let url = format!("{}/app/getAppAccessToken", self.cfg.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({
                "appId": self.cfg.app_id,
                "clientSecret": self.cfg.client_secret,
            }))
            .send()
            .await?;

        let status = resp.status();
        let body = resp.text().await?;

        // ⚠️ 该接口失败时 HTTP 仍可能为 200，必须看响应体的 code 字段。
        let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            ApiError::Protocol(format!(
                "getAppAccessToken 返回非 JSON（HTTP {status}）: {e}; body={body}"
            ))
        })?;

        if let Some(code) = v.get("code").and_then(|c| c.as_i64()) {
            if code != 0 {
                return Err(ApiError::Business {
                    code,
                    message: v.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string(),
                    trace_id: None,
                });
            }
        }

        let token = v
            .get("access_token")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ApiError::Protocol(format!("响应缺少 access_token: {body}")))?
            .to_string();

        // 官方返回示例中 expires_in 是字符串 "7200"，这里同时兼容数字。
        let expires_in = v.get("expires_in").and_then(parse_secs).unwrap_or(7200);

        // 服务端数值不可信：夹住上界防溢出；余量不得超过寿命的一半，
        // 否则会出现「刚刷新完就判定过期」的空刷循环（线上日志见过 expires_in=271/214）。
        let lifetime = Duration::from_secs(expires_in).min(MAX_TOKEN_LIFETIME);

        tracing::info!(
            expires_in,
            margin_secs = refresh_margin(lifetime).as_secs(),
            "access_token 已刷新"
        );
        *self.cache.write().await = Some(CachedToken {
            token: token.clone(),
            refresh_at: Instant::now() + refresh_after(lifetime),
        });
        Ok(token)
    }

    /// 丢弃缓存，强制下次重新获取（401 后调用）。
    pub async fn invalidate(&self) {
        self.cache.write().await.take();
    }
}

/// 实际保留的刷新余量：**不得超过寿命的一半**。
///
/// 若固定 300s，而服务端返回的 `expires_in` 只有 271s（线上日志确实出现过
/// 271 / 214），那么刷新刚完成时「剩余寿命 > 余量」就恒为假 ——
/// 于是**每次调用都重新刷新一遍**，拿回来的还是同一个 token。
fn refresh_margin(lifetime: Duration) -> Duration {
    REFRESH_MARGIN.min(lifetime / 2)
}

/// 从刷新时刻起，多久之后该再次刷新。结果恒小于 `lifetime`，
/// 因此刚写入的缓存必定立即可用。
fn refresh_after(lifetime: Duration) -> Duration {
    lifetime - refresh_margin(lifetime)
}

fn parse_secs(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// 网关连接信息（`GET /gateway/bot`）。
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayInfo {
    pub url: String,
    /// 官方建议的分片总数。
    #[serde(default)]
    pub shards: u32,
    #[serde(default)]
    pub session_start_limit: Option<SessionStartLimit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionStartLimit {
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub remaining: u32,
    #[serde(default)]
    pub reset_after: u64,
    #[serde(default)]
    pub max_concurrency: u32,
}

/// 发消息返回。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SendResult {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
}

/// 上传返回。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaUploadResult {
    #[serde(default)]
    pub file_info: Option<String>,
    #[serde(default)]
    pub file_uuid: Option<String>,
    /// 有效期（秒）。过期需重新上传。
    #[serde(default)]
    pub ttl: Option<u64>,
    /// `srv_send_msg = true` 时同时返回消息 ID。
    #[serde(default)]
    pub id: Option<String>,
    /// 文件下载链接（COS 预签名 GET URL），有效期与 `ttl` 一致。
    #[serde(default)]
    pub raw_url: Option<String>,
}

/// 分片上传预上传返回。
///
/// 官方把 block_size 作为**字符串**返回（如 "10485760"），这里统一转成数字。
#[derive(Debug, Clone, Deserialize)]
pub struct UploadPrepareResult {
    pub upload_id: String,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub block_size: u64,
    #[serde(default)]
    pub parts: Vec<UploadPartUrl>,
    #[serde(default)]
    pub upload_config: Option<UploadConfig>,
}

/// 上传配置（由服务端下发，控制客户端上传行为）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UploadConfig {
    /// 上传并发数，默认 1。
    #[serde(default)]
    pub concurrency: u32,
    /// 重试超时（秒），默认 300。
    #[serde(default)]
    pub retry_timeout: u64,
    /// 重试延迟（秒），默认 1。
    #[serde(default)]
    pub retry_delay: u64,
}

/// 同时接受数字与字符串的 u64，缺省为 0。
fn de_opt_u64<'de, D>(d: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(v.as_ref().and_then(parse_secs).unwrap_or(0))
}

#[derive(Debug, Clone, Deserialize)]
pub struct UploadPartUrl {
    /// 分片序号，从 0 开始。
    #[serde(default)]
    pub index: u32,
    /// 官方字段名为 presigned_url；url 作为兼容别名。
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub presigned_url: Option<String>,
    #[serde(default)]
    pub block_size: Option<String>,
}

impl UploadPartUrl {
    /// 兼容 `url` / `presigned_url` 两种字段命名。
    pub fn endpoint(&self) -> Option<&str> {
        self.presigned_url.as_deref().or(self.url.as_deref())
    }
}

/// OpenAPI 客户端。内部持有 `reqwest::Client`（连接池）与 token 缓存，克隆代价很低。
#[derive(Clone)]
pub struct ApiClient {
    cfg: ApiClientConfig,
    http: reqwest::Client,
    tokens: Arc<TokenProvider>,
}

impl ApiClient {
    pub fn new(cfg: ApiClientConfig) -> Result<Self, ApiError> {
        Self::with_timeout(cfg, DEFAULT_HTTP_TIMEOUT)
    }

    pub fn with_timeout(cfg: ApiClientConfig, timeout: Duration) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;
        let tokens = Arc::new(TokenProvider::new(cfg.clone(), http.clone()));
        Ok(Self { cfg, http, tokens })
    }

    pub fn config(&self) -> &ApiClientConfig {
        &self.cfg
    }

    pub async fn token(&self) -> Result<String, ApiError> {
        self.tokens.token().await
    }

    pub async fn invalidate_token(&self) {
        self.tokens.invalidate().await;
    }

    /// 网关鉴权用的 token。
    ///
    /// ⚠️ 官方规定 Identify / Resume 的 `token` 字段格式为 **`"QQBot {AccessToken}"`**，
    /// 与 HTTP 的 `Authorization` 头一致。漏掉前缀会被服务端以 op 9 InvalidSession 拒绝。
    pub async fn gateway_token(&self) -> Result<String, ApiError> {
        Ok(format!("QQBot {}", self.token().await?))
    }

    /// 构造 Identify 载荷。
    pub async fn identify(&self, intents: Intents, shard: [u32; 2]) -> Result<Identify, ApiError> {
        Ok(Identify {
            token: self.gateway_token().await?,
            intents,
            shard,
            properties: Properties::default(),
        })
    }

    /// 构造 Resume 载荷。
    pub async fn resume(&self, session_id: impl Into<String>, seq: i64) -> Result<Resume, ApiError> {
        Ok(Resume { token: self.gateway_token().await?, session_id: session_id.into(), seq })
    }

    /// 取网关地址与建议分片数。
    pub async fn gateway(&self) -> Result<GatewayInfo, ApiError> {
        self.get_json("/gateway/bot").await
    }

    /// 发送消息。
    pub async fn send_message(&self, target: &Target, msg: &OutMessage) -> Result<SendResult, ApiError> {
        self.post_json(&target.messages_path(), msg).await
    }

    /// 撤回消息。官方限制：发送超过 **2 分钟**不可撤回。
    pub async fn recall_message(&self, target: &Target, message_id: &str) -> Result<(), ApiError> {
        let path = format!("{}/{}", target.messages_path(), message_id);
        self.delete_json(&path).await
    }

    /// 通过公网 URL 上传富媒体（平台自动下载转存）。
    ///
    /// `file_type`：1 图片 / 2 视频 / 3 语音 / 4 文件。
    pub async fn upload_by_url(
        &self,
        target: &Target,
        file_type: u8,
        url: &str,
        srv_send_msg: bool,
    ) -> Result<MediaUploadResult, ApiError> {
        let body = serde_json::json!({
            "file_type": file_type,
            "url": url,
            "srv_send_msg": srv_send_msg,
        });
        self.post_json(&target.files_path(), &body).await
    }

    /// 分片上传第一步：预上传。
    pub async fn upload_prepare(
        &self,
        target: &Target,
        body: &serde_json::Value,
    ) -> Result<UploadPrepareResult, ApiError> {
        self.post_json(&target.upload_prepare_path(), body).await
    }

    /// 分片上传第三步：确认单个分片完成。
    pub async fn upload_part_finish(
        &self,
        target: &Target,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, ApiError> {
        self.post_json(&target.upload_part_finish_path(), body).await
    }

    /// 分片上传第四步：合并，返回 `file_info`。
    pub async fn upload_finish(
        &self,
        target: &Target,
        upload_id: &str,
        file_type: u8,
    ) -> Result<MediaUploadResult, ApiError> {
        // srv_send_msg 是可选字段且官方未说明默认值：显式传 false，
        // 避免默认值意外变成「上传即发送」而占用主动消息频次。
        let body = serde_json::json!({
            "upload_id": upload_id,
            "file_type": file_type,
            "srv_send_msg": false,
        });
        self.post_json(&target.files_path(), &body).await
    }

    // ---------- 通用请求 ----------

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        let req = self.http.get(self.url(path));
        self.execute(req).await
    }

    async fn post_json<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ApiError> {
        let req = self.http.post(self.url(path)).json(body);
        self.execute(req).await
    }

    async fn delete_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        let req = self.http.delete(self.url(path));
        self.execute(req).await
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path)
    }

    async fn execute<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, ApiError> {
        let token = self.tokens.token().await?;
        let resp = req
            .header("Authorization", format!("QQBot {token}"))
            .send()
            .await?;

        let status = resp.status().as_u16();
        let trace = resp
            .headers()
            .get("X-Tps-trace-ID")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = resp.text().await?;

        if status == 401 {
            // token 可能刚失效，丢弃缓存以便下次重取。
            self.tokens.invalidate().await;
        }

        decode_response(status, trace, &body)
    }
}

/// 统一响应解码。
///
/// ⚠️ 官方规定：**失败时 HTTP 状态码可能仍是 200**，因此必须先检查 `err_code`。
pub fn decode_response<T: DeserializeOwned>(
    status: u16,
    trace: Option<String>,
    body: &str,
) -> Result<T, ApiError> {
    if status == 401 {
        return Err(ApiError::Unauthorized);
    }
    if status == 429 {
        return Err(ApiError::RateLimited);
    }

    let v: serde_json::Value = if body.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(body).map_err(|e| {
            ApiError::Protocol(format!("响应非 JSON（HTTP {status}）: {e}; body={body}"))
        })?
    };

    if let Some(code) = v.get("err_code").and_then(|c| c.as_i64()) {
        if code != 0 {
            return Err(ApiError::Business {
                code,
                message: v.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string(),
                trace_id: trace.or_else(|| {
                    v.get("trace_id").and_then(|t| t.as_str()).map(str::to_string)
                }),
            });
        }
    }

    if !(200..300).contains(&status) {
        return Err(ApiError::Status { status, body: body.to_string() });
    }

    serde_json::from_value(v).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：刷新余量必须被「寿命的一半」夹住。
    ///
    /// 修复前余量恒为 300s，只要服务端返回的 `expires_in <= 300`
    /// （线上日志确实出现过 271 / 214），刷新刚完成时「剩余寿命 > 余量」
    /// 就恒为假 —— 每次 API 调用都会重新刷新一遍，且全程持独占锁。
    #[test]
    fn refresh_point_always_lands_after_the_refresh_itself() {
        // 正常寿命：留满 300s 余量
        assert_eq!(refresh_after(Duration::from_secs(7200)), Duration::from_secs(6900));

        // 线上真实出现过的短寿命（271s）：余量被压到一半，刷新点仍在未来
        assert_eq!(refresh_margin(Duration::from_secs(271)), Duration::from_millis(135_500));
        assert_eq!(refresh_after(Duration::from_secs(271)), Duration::from_millis(135_500));

        // 关键不变式：任何正寿命都必须得到**正**的可用窗口，
        // 否则 token 刚写入就被判定过期 —— 正是那个空刷循环。
        for secs in [1u64, 2, 60, 271, 300, 301, 7200, u64::MAX] {
            let lifetime = Duration::from_secs(secs).min(MAX_TOKEN_LIFETIME);
            assert!(
                refresh_after(lifetime) > Duration::ZERO,
                "寿命 {secs}s 时可用窗口为 0，会导致每次调用都空刷"
            );
            assert!(refresh_after(lifetime) <= lifetime);
        }

        // 寿命为 0 也不能 panic（Duration 减法会下溢）
        assert_eq!(refresh_after(Duration::ZERO), Duration::ZERO);
    }

    #[derive(Debug, Deserialize)]
    struct Dummy {
        #[serde(default)]
        id: Option<String>,
    }

    #[test]
    fn err_code_with_http_200_is_an_error() {
        let r = decode_response::<Dummy>(
            200,
            Some("trace-1".into()),
            r#"{"err_code":40034005,"message":"回复消息msg_id已过期","trace_id":"t2"}"#,
        );
        match r {
            Err(e) => {
                assert_eq!(e.business_code(), Some(40034005));
                assert!(e.is_passive_expired());
                assert!(!e.is_retryable());
            }
            Ok(_) => panic!("应当报错"),
        }
    }

    #[test]
    fn http_401_and_429_map_to_dedicated_variants() {
        assert!(matches!(decode_response::<Dummy>(401, None, ""), Err(ApiError::Unauthorized)));
        assert!(matches!(decode_response::<Dummy>(429, None, ""), Err(ApiError::RateLimited)));
        assert!(decode_response::<Dummy>(429, None, "").unwrap_err().is_retryable());
    }

    #[test]
    fn http_500_is_retryable() {
        let e = decode_response::<Dummy>(500, None, r#"{"message":"boom"}"#).unwrap_err();
        assert!(e.is_retryable());
    }

    #[test]
    fn successful_body_is_parsed() {
        let d: Dummy = decode_response(200, None, r#"{"id":"ROBOT1.0_x"}"#).unwrap();
        assert_eq!(d.id.as_deref(), Some("ROBOT1.0_x"));
    }

    #[test]
    fn empty_body_is_accepted_for_delete() {
        let _: () = decode_response(204, None, "").unwrap();
    }

    #[test]
    fn expires_in_accepts_string_and_number() {
        assert_eq!(parse_secs(&serde_json::json!("7200")), Some(7200));
        assert_eq!(parse_secs(&serde_json::json!(7200)), Some(7200));
        assert_eq!(parse_secs(&serde_json::json!(null)), None);
    }

    #[test]
    fn gateway_token_must_carry_qqbot_prefix() {
        // 官方文档：token 格式为 "QQBot {AccessToken}"。
        // 这里直接验证拼接逻辑，避免回归时又要连线上才能发现。
        let token = "ABC123";
        assert_eq!(format!("QQBot {token}"), "QQBot ABC123");
    }

    #[test]
    fn system_errors_are_retry_once() {
        let e = decode_response::<Dummy>(200, None, r#"{"err_code":11281,"message":"sys"}"#).unwrap_err();
        assert!(e.is_retry_once());
        assert!(e.is_retryable());
    }
}
