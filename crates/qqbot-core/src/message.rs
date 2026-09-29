use qqbot_api::{OutMessage, Target};

/// 待发送的消息内容。
#[derive(Debug, Clone)]
pub enum Body {
    Text(String),
    Markdown(String),
    /// 已上传的富媒体，携带 `file_info`。
    Media { file_info: String },
}

impl Body {
    pub fn to_out_message(&self) -> OutMessage {
        match self {
            Body::Text(t) => OutMessage::text(t.clone()),
            Body::Markdown(m) => OutMessage::markdown(m.clone()),
            Body::Media { file_info } => OutMessage::media(file_info.clone()),
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
}

impl SendRequest {
    pub fn text(target: Target, text: impl Into<String>) -> Self {
        Self { target, body: Body::Text(text.into()), reply_to: None, event_id: None }
    }

    pub fn markdown(target: Target, md: impl Into<String>) -> Self {
        Self { target, body: Body::Markdown(md.into()), reply_to: None, event_id: None }
    }

    pub fn media(target: Target, file_info: impl Into<String>) -> Self {
        Self {
            target,
            body: Body::Media { file_info: file_info.into() },
            reply_to: None,
            event_id: None,
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
}
