//! 日报（早报）插件。
//!
//! 上游返回的是一张包含当日全部新闻的**长图**，本插件把它下载后转成
//! `file_info` 再发送 —— 官方 API 不接受远程 URL 直发，也不能发本地路径。
//!
//! 缓存是刻意的：早报一天只更新一次，而用户可能在短时间内反复触发，
//! 每一次触发都会消耗一次上游配额。

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use serde::Deserialize;
use tokio::sync::Mutex;

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
    /// 只缓存一份结果。用 `Mutex` 而不是无锁结构：并发触发时只让一个请求
    /// 打到上游，其余在锁上等待后直接命中缓存。
    cache: Mutex<Option<Cached>>,
}

impl DailyPlugin {
    pub fn new(config: DailyConfig, http: reqwest::Client) -> Self {
        Self { config, http, cache: Mutex::new(None) }
    }

    /// 取早报图片，命中缓存则直接返回。
    async fn image(&self) -> Result<(Arc<Vec<u8>>, String), String> {
        let mut guard = self.cache.lock().await;
        if let Some(hit) = guard.as_ref()
            && hit.at.elapsed() < self.config.cache
        {
            return Ok((Arc::clone(&hit.bytes), hit.file_name.clone()));
        }

        let (bytes, file_name) = self.fetch().await?;
        let bytes = Arc::new(bytes);
        *guard = Some(Cached {
            bytes: Arc::clone(&bytes),
            file_name: file_name.clone(),
            at: Instant::now(),
        });
        Ok((bytes, file_name))
    }

    /// 拉取并解析上游数据，返回图片字节与文件名。
    async fn fetch(&self) -> Result<(Vec<u8>, String), String> {
        let url = with_token(&self.config.api_url, &self.config.token);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|err| format!("请求失败：{err}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("接口返回 HTTP {status}"));
        }
        let body = resp.text().await.map_err(|err| format!("读取响应失败：{err}"))?;
        let image_url = pick_image_url(&body)?;
        let file_name = file_name_from_url(&image_url);

        let img = self
            .http
            .get(&image_url)
            .send()
            .await
            .map_err(|err| format!("下载图片失败：{err}"))?;
        let img_status = img.status();
        if !img_status.is_success() {
            return Err(format!("下载图片返回 HTTP {img_status}"));
        }
        let bytes = img
            .bytes()
            .await
            .map_err(|err| format!("读取图片失败：{err}"))?
            .to_vec();
        if bytes.is_empty() {
            return Err("图片内容为空".to_string());
        }
        Ok((bytes, file_name))
    }
}

#[async_trait]
impl Handler for DailyPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        match self.image().await {
            Ok((bytes, file_name)) => {
                if let Err(err) = ctx.reply_image_named(&file_name, &bytes).await {
                    tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "日报发送失败");
                }
            }
            Err(reason) => {
                tracing::warn!(error = %reason, "日报获取失败");
                // 失败也要回话：静默失败会让用户以为机器人没收到命令。
                if let Err(err) = ctx.reply_text(format!("日报获取失败：{reason}")).await {
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
pub fn pick_image_url(body: &str) -> Result<String, String> {
    let parsed: DailyResponse =
        serde_json::from_str(body).map_err(|err| format!("响应解析失败：{err}"))?;

    // 上游失败时 HTTP 仍是 200，必须看 body 里的 code —— 与官方 API 同一个坑。
    if let Some(code) = parsed.code
        && code != 200
    {
        let msg = parsed.message.unwrap_or_else(|| "未知错误".to_string());
        return Err(format!("接口返回 code={code}：{msg}"));
    }

    let data = parsed.data.ok_or_else(|| "响应缺少 data 字段".to_string())?;
    let url = data.image.or(data.head_image).unwrap_or_default();
    let url = url.trim();
    if url.is_empty() {
        return Err("响应里没有图片地址".to_string());
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
        assert!(err.contains("code=400"), "{err}");
        assert!(err.contains("token 无效"), "{err}");
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
