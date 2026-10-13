//! 回复：把「这次发送属于哪条入站消息 / 哪个事件」收进一个类型。
//!
//! 被动凭证是发送链路上唯一「漏掉也不报错」的东西：漏挂既不会编译失败，
//! 也不会运行失败 —— 只会静默降级成主动消息、吃掉配额（群聊一条入站消息
//! 一共只有 5 次）。所以凭证在**构造** [`Reply`] 时就确定，并且只在
//! `attach` 这一处挂到 [`SendRequest`] 上。

use std::sync::Arc;

use qqbot_api::{Keyboard, SendResult, Target};
use qqbot_media::FileType;
use qqbot_render::Card;

use crate::ctx::Services;
use crate::error::CoreError;
use crate::message::SendRequest;

/// 被动回复凭证：一次发送属于哪条入站消息 / 哪个事件。
enum Credential {
    /// 消息凭证：`msg_id`。群聊被动窗口 5 分钟 / 5 次（单聊 60 分钟 / 4 次）。
    Message(String),
    /// 事件凭证：`event_id`。互动事件必须 3 秒内回应。
    Event(String),
    /// 没有凭证，只能发主动消息。
    None,
}

/// 一次发送的目标 + 凭证。
///
/// 语义方法（`text` / `markdown` / `image` / ...）覆盖插件全部出站需求；
/// **刻意不留 `send(SendRequest)` 逃生口** —— 真需要时再加方法，
/// 比现在留一个能绕过凭证的口子安全（ADR 0001 第 6 条）。
pub struct Reply {
    services: Arc<Services>,
    target: Target,
    credential: Credential,
}

impl Reply {
    /// 消息凭证：回复某条入站消息。
    pub fn for_message(services: Arc<Services>, target: Target, msg_id: impl Into<String>) -> Self {
        Self { services, target, credential: Credential::Message(msg_id.into()) }
    }

    /// 事件凭证：回应某个互动事件；`None` 表示拿不到 event_id，只能发主动消息。
    pub fn for_event(services: Arc<Services>, target: Target, event_id: Option<String>) -> Self {
        let credential = match event_id {
            Some(id) => Credential::Event(id),
            None => Credential::None,
        };
        Self { services, target, credential }
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    pub async fn text(&self, text: impl Into<String>) -> Result<SendResult, CoreError> {
        let request = SendRequest::text(self.target.clone(), text);
        self.send(request).await
    }

    pub async fn markdown(&self, md: impl Into<String>) -> Result<SendResult, CoreError> {
        let request = SendRequest::markdown(self.target.clone(), md);
        self.send(request).await
    }

    /// Markdown 正文 + 内嵌键盘（按钮）回复。
    ///
    /// 正文与按钮同一条消息发出，省一次被动回复配额（群聊一共只有 5 次）。
    pub async fn markdown_with_keyboard(
        &self,
        md: impl Into<String>,
        keyboard: Keyboard,
    ) -> Result<SendResult, CoreError> {
        let request = SendRequest::markdown_with_keyboard(self.target.clone(), md, keyboard);
        self.send(request).await
    }

    /// 直接发送 PNG 字节（内部完成上传 + 发送）。
    pub async fn image(&self, png: &[u8]) -> Result<SendResult, CoreError> {
        let file_info = self.services.upload_png(&self.target, png).await?;
        let request = SendRequest::media(self.target.clone(), file_info);
        self.send(request).await
    }

    /// 发送任意格式的图片（`file_name` 决定格式，如 `daily.jpg`）。
    pub async fn image_named(
        &self,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<SendResult, CoreError> {
        let file_info = self.services.upload_image(&self.target, file_name, bytes).await?;
        let request = SendRequest::media(self.target.clone(), file_info);
        self.send(request).await
    }

    /// 发送任意类型的素材（图片 / 视频 / 语音 / 文件）。
    ///
    /// file_type 与 file_name 都要对：平台按文件名判格式，
    /// 而 file_type 决定走哪条上传通道。
    pub async fn media(
        &self,
        file_type: FileType,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<SendResult, CoreError> {
        let file_info =
            self.services.media.upload_bytes(&self.target, file_type, file_name, bytes).await?;
        let request = SendRequest::media(self.target.clone(), file_info);
        self.send(request).await
    }

    /// 发送素材并附上一段说明文字。
    ///
    /// 官方 API 的富媒体消息**可以**带 `content`，所以图和说明能一条发完，
    /// 不必拆成两条 —— 那会白占一次被动回复配额（群聊一共只有 5 次）。
    pub async fn media_with_text(
        &self,
        file_type: FileType,
        file_name: &str,
        bytes: &[u8],
        text: &str,
    ) -> Result<SendResult, CoreError> {
        let file_info =
            self.services.media.upload_bytes(&self.target, file_type, file_name, bytes).await?;
        let request = SendRequest::media_with_text(self.target.clone(), file_info, text);
        self.send(request).await
    }

    /// 渲染一张卡片并作为图片回复。
    ///
    /// 调用方只管内容（标题 / 行 / 页脚 / 宽度），
    /// 用哪个模板、高度多少都由 `Card` 决定。
    pub async fn card(&self, card: &Card) -> Result<SendResult, CoreError> {
        let png = self.services.render_card_png(card).await?;
        self.image(&png).await
    }

    /// 渲染 SVG 源码并作为图片回复。
    pub async fn svg(&self, svg: String) -> Result<SendResult, CoreError> {
        let png = self.services.render_svg_png(svg).await?;
        self.image(&png).await
    }

    /// 挂凭证 → 交给会话 actor 分配 `msg_seq` / 校验配额。
    async fn send(&self, request: SendRequest) -> Result<SendResult, CoreError> {
        self.services.send(attach(request, &self.credential)).await
    }
}

/// 把凭证挂到请求上。
///
/// **唯一**一处决定「这次发送带不带被动凭证」的地方：每个语义方法都走它，
/// 所以不存在「某个方法忘了挂」这种静默降级。
fn attach(request: SendRequest, credential: &Credential) -> SendRequest {
    match credential {
        Credential::Message(msg_id) => request.replying_to(msg_id.clone()),
        Credential::Event(event_id) => request.responding_to_event(event_id.clone()),
        Credential::None => request,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> SendRequest {
        SendRequest::text(Target::group("G1"), "hi")
    }

    /// 消息凭证落到 `reply_to`（即 `msg_id`），且不碰 `event_id`。
    #[test]
    fn message_credential_sets_reply_to() {
        let req = attach(request(), &Credential::Message("M1".into()));
        assert_eq!(req.reply_to.as_deref(), Some("M1"));
        assert!(req.event_id.is_none(), "消息凭证不应带 event_id");
    }

    /// 事件凭证落到 `event_id`，且不碰 `reply_to`。
    #[test]
    fn event_credential_sets_event_id() {
        let req = attach(request(), &Credential::Event("EV".into()));
        assert_eq!(req.event_id.as_deref(), Some("EV"));
        assert!(req.reply_to.is_none(), "事件凭证不应带 msg_id");
    }

    /// 没有凭证时两个字段都不能有 —— 带任何一个都会被官方当成被动回复。
    #[test]
    fn no_credential_leaves_request_untouched() {
        let req = attach(request(), &Credential::None);
        assert!(req.reply_to.is_none());
        assert!(req.event_id.is_none());
    }
}
