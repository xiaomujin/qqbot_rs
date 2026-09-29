use std::sync::{Arc, RwLock};

use qqbot_api::{ApiClient, MessageEvent, SendResult, Target};
use qqbot_media::MediaUploader;
use qqbot_render::RenderService;
use qqbot_store::MessageStore;

use crate::error::CoreError;
use crate::message::SendRequest;
use crate::session::SessionRegistry;

/// 共享服务集合。插件通过 [`Ctx`] 访问它们。
pub struct Services {
    pub api: ApiClient,
    pub media: MediaUploader,
    pub render: RenderService,
    pub sessions: Arc<SessionRegistry>,
    /// 消息持久化。`None` 表示未启用（此时词云回退到内存语料）。
    pub store: Option<Arc<MessageStore>>,
    bot_name: RwLock<String>,
}

impl Services {
    pub fn new(
        api: ApiClient,
        media: MediaUploader,
        render: RenderService,
        sessions: Arc<SessionRegistry>,
    ) -> Self {
        Self {
            api,
            media,
            render,
            sessions,
            store: None,
            bot_name: RwLock::new(String::from("qqbot")),
        }
    }

    /// 挂上消息存储。
    pub fn with_store(mut self, store: Arc<MessageStore>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn bot_name(&self) -> String {
        self.bot_name.read().map(|n| n.clone()).unwrap_or_else(|_| "qqbot".to_string())
    }

    pub fn set_bot_name(&self, name: impl Into<String>) {
        if let Ok(mut guard) = self.bot_name.write() {
            *guard = name.into();
        }
    }

    /// 发送（走会话 actor 分配 `msg_seq` 与配额校验）。
    pub async fn send(&self, request: SendRequest) -> Result<SendResult, CoreError> {
        self.sessions.send(request).await
    }

    /// 渲染模板为 PNG 字节。
    pub async fn render_template_png(
        &self,
        template: &str,
        data: serde_json::Value,
    ) -> Result<Vec<u8>, CoreError> {
        Ok(self.render.render_template(template, data).await?.png)
    }

    /// 渲染 SVG 源码为 PNG 字节。
    pub async fn render_svg_png(&self, svg: String) -> Result<Vec<u8>, CoreError> {
        Ok(self.render.render_svg(svg).await?.png)
    }

    /// 上传 PNG 并返回 `file_info`（带秒传缓存）。
    pub async fn upload_png(&self, target: &Target, png: &[u8]) -> Result<String, CoreError> {
        Ok(self.media.upload_png(target, png).await?)
    }
}

/// 单条消息的处理上下文。
pub struct Ctx {
    pub services: Arc<Services>,
    pub message: Arc<MessageEvent>,
    pub target: Target,
    content: String,
    args: Vec<String>,
}

impl Ctx {
    /// 由入站消息构造；无法推导发送目标时返回 `None`。
    pub fn new(services: Arc<Services>, message: Arc<MessageEvent>) -> Option<Self> {
        let target = message.target()?;
        let content = message.trimmed().to_string();
        let args = content.split_whitespace().skip(1).map(str::to_string).collect();
        Some(Self { services, message, target, content, args })
    }

    /// 去掉首尾空白后的全文（路由匹配依据）。
    pub fn content(&self) -> &str {
        &self.content
    }

    /// 命令之后的参数列表（按空白切分）。
    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn arg(&self, index: usize) -> Option<&str> {
        self.args.get(index).map(String::as_str)
    }

    /// 命令之后的原始文本（保留空格）。
    pub fn rest(&self) -> String {
        let cmd = self.content.split_whitespace().next().unwrap_or("");
        self.content
            .strip_prefix(cmd)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    pub fn is_group(&self) -> bool {
        self.target.is_group()
    }

    pub fn is_admin(&self) -> bool {
        self.message.is_admin()
    }

    pub fn sender_openid(&self) -> Option<&str> {
        self.message.sender_openid()
    }

    pub fn sender_name(&self) -> &str {
        self.message.author.username.as_deref().unwrap_or("未知用户")
    }

    /// 入站消息 id（被动回复凭证）。
    pub fn message_id(&self) -> &str {
        &self.message.id
    }

    // ---------- 回复便捷方法 ----------

    pub async fn reply_text(&self, text: impl Into<String>) -> Result<SendResult, CoreError> {
        self.services
            .send(SendRequest::text(self.target.clone(), text).replying_to(self.message.id.clone()))
            .await
    }

    pub async fn reply_markdown(&self, md: impl Into<String>) -> Result<SendResult, CoreError> {
        self.services
            .send(SendRequest::markdown(self.target.clone(), md).replying_to(self.message.id.clone()))
            .await
    }

    /// 直接发送 PNG 字节（内部完成上传 + 发送）。
    pub async fn reply_image(&self, png: &[u8]) -> Result<SendResult, CoreError> {
        let file_info = self.services.upload_png(&self.target, png).await?;
        self.services
            .send(SendRequest::media(self.target.clone(), file_info).replying_to(self.message.id.clone()))
            .await
    }

    /// 渲染模板并作为图片回复。
    pub async fn reply_template(
        &self,
        template: &str,
        data: serde_json::Value,
    ) -> Result<SendResult, CoreError> {
        let png = self.services.render_template_png(template, data).await?;
        self.reply_image(&png).await
    }

    /// 渲染 SVG 源码并作为图片回复。
    pub async fn reply_svg(&self, svg: String) -> Result<SendResult, CoreError> {
        let png = self.services.render_svg_png(svg).await?;
        self.reply_image(&png).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qqbot_api::MessageEvent;

    fn services() -> Arc<Services> {
        use qqbot_api::ApiClientConfig;
        use qqbot_render::RenderConfig;
        use std::time::Duration;

        let api = ApiClient::new(ApiClientConfig::new("test-app", "test-secret")).unwrap();
        let media = MediaUploader::new(api.clone());
        let render = RenderService::new(RenderConfig::default());
        let sessions = Arc::new(SessionRegistry::new(api.clone(), 2, 16, Duration::from_secs(5)));
        Arc::new(Services::new(api, media, render, sessions))
    }

    fn message(raw: &str) -> Arc<MessageEvent> {
        Arc::new(serde_json::from_str(raw).unwrap())
    }

    #[tokio::test]
    async fn parses_command_and_args() {
        let m = message(r#"{"id":"M1","author":{"member_openid":"U1"},"content":"  塔科夫 价格 显卡  ","group_openid":"G1"}"#);
        let ctx = Ctx::new(services(), m).unwrap();
        assert_eq!(ctx.content(), "塔科夫 价格 显卡");
        assert_eq!(ctx.arg(0), Some("价格"));
        assert_eq!(ctx.arg(1), Some("显卡"));
        assert_eq!(ctx.rest(), "价格 显卡");
        assert!(ctx.is_group());
        assert_eq!(ctx.target, Target::group("G1"));
    }

    #[tokio::test]
    async fn c2c_message_has_no_group() {
        let m = message(r#"{"id":"M2","author":{"user_openid":"U9","username":"小明"},"content":"你好"}"#);
        let ctx = Ctx::new(services(), m).unwrap();
        assert!(!ctx.is_group());
        assert_eq!(ctx.sender_name(), "小明");
        assert!(ctx.args().is_empty());
        assert_eq!(ctx.rest(), "");
    }

    #[tokio::test]
    async fn missing_target_yields_none() {
        let m = message(r#"{"id":"M3","author":{},"content":"x"}"#);
        assert!(Ctx::new(services(), m).is_none());
    }

    #[tokio::test]
    async fn bot_name_roundtrip() {
        let s = services();
        assert_eq!(s.bot_name(), "qqbot");
        s.set_bot_name("黑猫Bot");
        assert_eq!(s.bot_name(), "黑猫Bot");
    }
}
