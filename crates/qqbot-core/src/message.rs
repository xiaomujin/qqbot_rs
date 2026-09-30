use qqbot_api::{Keyboard, OutMessage, Target};

/// 待发送的消息内容。
#[derive(Debug, Clone)]
pub enum Body {
    Text(String),
    Markdown(String),
    /// 已上传的富媒体，携带 `file_info`。
    ///
    /// `text` 是**可选的说明文字**：官方 API 的富媒体消息（`msg_type=7`）
    /// 允许同时带 `content`，客户端会把图和文字放在同一个气泡里。
    /// 用不上时是 `None` —— 拆成两条消息会白占一次被动回复配额。
    Media { file_info: String, text: Option<String> },
}

impl Body {
    pub fn to_out_message(&self) -> OutMessage {
        match self {
            Body::Text(t) => OutMessage::text(t.clone()),
            Body::Markdown(m) => OutMessage::markdown(m.clone()),
            Body::Media { file_info, text } => match text {
                Some(t) => OutMessage::media(file_info.clone()).with_content(t.clone()),
                None => OutMessage::media(file_info.clone()),
            },
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Body::Text(_) => "text",
            Body::Markdown(_) => "markdown",
            Body::Media { .. } => "media",
        }
    }
}

/// 一次发送请求。
///
/// `reply_to` 存在即走**被动回复**（官方：单聊 60 分钟 / 4 次，群聊 5 分钟 / 5 次）；
/// 否则走**主动消息**，受单关系 20/min、1000/天 配额约束。
#[derive(Debug, Clone)]
pub struct SendRequest {
    pub target: Target,
    pub body: Body,
    /// 被动回复凭证（入站消息的 id）。
    pub reply_to: Option<String>,
    /// 响应事件（而非消息）时使用。
    pub event_id: Option<String>,
    /// 内嵌键盘（按钮）。`None` 表示普通消息。
    pub keyboard: Option<Keyboard>,
}

impl SendRequest {
    pub fn text(target: Target, text: impl Into<String>) -> Self {
        Self { target, body: Body::Text(text.into()), reply_to: None, event_id: None, keyboard: None }
    }

    pub fn markdown(target: Target, md: impl Into<String>) -> Self {
        Self { target, body: Body::Markdown(md.into()), reply_to: None, event_id: None, keyboard: None }
    }

    /// Markdown 正文 + 内嵌键盘（按钮）。
    ///
    /// 任务卡片走这条路：正文用 Markdown，翻页等操作靠回调按钮，
    /// 不需要渲染成图片再上传（省一次上传，也省一次被动回复配额）。
    pub fn markdown_with_keyboard(
        target: Target,
        md: impl Into<String>,
        keyboard: Keyboard,
    ) -> Self {
        Self {
            target,
            body: Body::Markdown(md.into()),
            reply_to: None,
            event_id: None,
            keyboard: Some(keyboard),
        }
    }

    pub fn media(target: Target, file_info: impl Into<String>) -> Self {
        Self {
            target,
            body: Body::Media { file_info: file_info.into(), text: None },
            reply_to: None,
            event_id: None,
            keyboard: None,
        }
    }

    /// 富媒体 + 一段说明文字，**同一条消息**发出。
    pub fn media_with_text(
        target: Target,
        file_info: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            target,
            body: Body::Media { file_info: file_info.into(), text: Some(text.into()) },
            reply_to: None,
            event_id: None,
            keyboard: None,
        }
    }

    pub fn replying_to(mut self, msg_id: impl Into<String>) -> Self {
        self.reply_to = Some(msg_id.into());
        self
    }

    pub fn responding_to_event(mut self, event_id: impl Into<String>) -> Self {
        self.event_id = Some(event_id.into());
        self
    }

    /// 挂上内嵌键盘（按钮）。
    pub fn with_keyboard(mut self, keyboard: Keyboard) -> Self {
        self.keyboard = Some(keyboard);
        self
    }

    /// 组装出站消息：正文 + 键盘。
    ///
    /// 键盘不是 [`Body`] 的一部分（富媒体正文和它互不排斥），所以只能在
    /// [`Body::to_out_message`] 之后再挂上去。
    pub fn to_out_message(&self) -> OutMessage {
        let message = self.body.to_out_message();
        match &self.keyboard {
            Some(kb) => message.with_keyboard(kb.clone()),
            None => message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qqbot_api::{Button, ButtonAction, KeyboardRow, RenderData};

    fn pager() -> Keyboard {
        Keyboard::rows(vec![KeyboardRow {
            buttons: vec![Button {
                id: Some("next".into()),
                render_data: Some(RenderData { label: "下一页".into(), visited_label: None, style: 1 }),
                action: Some(ButtonAction {
                    action_type: 1,
                    data: Some("task:x:page:2".into()),
                    ..ButtonAction::default()
                }),
                group_id: None,
            }],
        }])
    }

    /// 契约：Markdown + 键盘必须序列化成 msg_type=2 + markdown + keyboard 三件套。
    #[test]
    fn markdown_with_keyboard_serializes_all_three_parts() {
        let req = SendRequest::markdown_with_keyboard(Target::group("G1"), "## 任务详情", pager());
        let v: serde_json::Value = serde_json::to_value(req.to_out_message()).unwrap();

        assert_eq!(v["msg_type"], 2, "Markdown 的 msg_type 必须是 2");
        assert_eq!(v["markdown"]["content"], "## 任务详情");
        assert_eq!(
            v["keyboard"]["content"]["rows"][0]["buttons"][0]["action"]["data"],
            "task:x:page:2"
        );
    }

    /// 普通构造器不得凭空带上键盘 —— 否则每条文本消息都会多出一个空键盘字段。
    #[test]
    fn plain_constructors_carry_no_keyboard() {
        let req = SendRequest::markdown(Target::c2c("U1"), "hi");
        assert!(req.keyboard.is_none());
        let v: serde_json::Value = serde_json::to_value(req.to_out_message()).unwrap();
        assert!(v.get("keyboard").is_none());
    }
}
