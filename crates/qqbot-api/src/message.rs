use serde::{Deserialize, Serialize};

/// 消息内容格式（`msg_type`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "u8", from = "u8")]
pub enum MsgType {
    /// 0 — 文本。
    Text,
    /// 2 — Markdown。
    Markdown,
    /// 3 — Ark 卡片。
    Ark,
    /// 4 — Embed。
    Embed,
    /// 7 — 富媒体（需先上传获取 `file_info`）。
    Media,
}

impl MsgType {
    pub const fn as_u8(self) -> u8 {
        match self {
            MsgType::Text => 0,
            MsgType::Markdown => 2,
            MsgType::Ark => 3,
            MsgType::Embed => 4,
            MsgType::Media => 7,
        }
    }
}

impl From<MsgType> for u8 {
    fn from(m: MsgType) -> u8 {
        m.as_u8()
    }
}

impl From<u8> for MsgType {
    fn from(v: u8) -> Self {
        match v {
            2 => MsgType::Markdown,
            3 => MsgType::Ark,
            4 => MsgType::Embed,
            7 => MsgType::Media,
            _ => MsgType::Text,
        }
    }
}

/// 发送目标。单聊与群聊的**上传接口互不相通**，因此该类型同时承担场景标识职责。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    C2c { user_openid: String },
    Group { group_openid: String },
}

impl Target {
    pub fn c2c(user_openid: impl Into<String>) -> Self {
        Target::C2c { user_openid: user_openid.into() }
    }

    pub fn group(group_openid: impl Into<String>) -> Self {
        Target::Group { group_openid: group_openid.into() }
    }

    /// 场景标识：`c2c` 或 `group`。用于缓存 key，避免单聊/群聊 `file_info` 串用。
    pub const fn scene(&self) -> &'static str {
        match self {
            Target::C2c { .. } => "c2c",
            Target::Group { .. } => "group",
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Target::C2c { user_openid } => user_openid,
            Target::Group { group_openid } => group_openid,
        }
    }

    pub const fn is_group(&self) -> bool {
        matches!(self, Target::Group { .. })
    }

    /// 会话路由 key（会话状态 actor 按此哈希分片）。
    pub fn key(&self) -> String {
        format!("{}:{}", self.scene(), self.id())
    }

    pub fn messages_path(&self) -> String {
        match self {
            Target::C2c { user_openid } => format!("/v2/users/{user_openid}/messages"),
            Target::Group { group_openid } => format!("/v2/groups/{group_openid}/messages"),
        }
    }

    pub fn files_path(&self) -> String {
        match self {
            Target::C2c { user_openid } => format!("/v2/users/{user_openid}/files"),
            Target::Group { group_openid } => format!("/v2/groups/{group_openid}/files"),
        }
    }

    pub fn upload_prepare_path(&self) -> String {
        match self {
            Target::C2c { user_openid } => format!("/v2/users/{user_openid}/upload_prepare"),
            Target::Group { group_openid } => format!("/v2/groups/{group_openid}/upload_prepare"),
        }
    }

    pub fn upload_part_finish_path(&self) -> String {
        match self {
            Target::C2c { user_openid } => format!("/v2/users/{user_openid}/upload_part_finish"),
            Target::Group { group_openid } => format!("/v2/groups/{group_openid}/upload_part_finish"),
        }
    }
}

/// 发送消息请求体。
///
/// 被动回复必须携带 `msg_id`；同一 `msg_id` 下 `msg_seq` **必须递增**，
/// 否则重复发送会被服务端拒绝（官方文档「消息去重」）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutMessage {
    /// 用 `MsgType` 而不是裸 `u8`：`MsgType` 已经实现 `into/from u8`，
    /// 序列化结果完全一致，但能挡住 `msg_type: 42` 这种能编译却会被服务端拒绝的值。
    pub msg_type: MsgType,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<Markdown>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard: Option<Keyboard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<Media>,

    /// 被动回复凭证。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
    /// 同一 `msg_id` 下的回复序号，必须递增。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_seq: Option<u32>,
    /// 响应事件（而非响应消息）时使用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// 互动召回消息标记。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_wakeup: Option<bool>,
}

impl OutMessage {
    pub fn text(content: impl Into<String>) -> Self {
        Self::bare(MsgType::Text).with_content(content)
    }

    pub fn markdown(content: impl Into<String>) -> Self {
        let mut m = Self::bare(MsgType::Markdown);
        m.markdown = Some(Markdown { content: content.into() });
        m
    }

    /// 富媒体消息（图片 / 视频 / 语音 / 文件），需先上传拿到 `file_info`。
    pub fn media(file_info: impl Into<String>) -> Self {
        let mut m = Self::bare(MsgType::Media);
        m.media = Some(Media { file_info: file_info.into() });
        m
    }

    fn bare(msg_type: MsgType) -> Self {
        Self {
            msg_type,
            content: None,
            markdown: None,
            keyboard: None,
            media: None,
            msg_id: None,
            msg_seq: None,
            event_id: None,
            is_wakeup: None,
        }
    }

    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }

    pub fn with_msg_id(mut self, msg_id: impl Into<String>) -> Self {
        self.msg_id = Some(msg_id.into());
        self
    }

    pub fn with_msg_seq(mut self, msg_seq: u32) -> Self {
        self.msg_seq = Some(msg_seq);
        self
    }

    pub fn with_event_id(mut self, event_id: impl Into<String>) -> Self {
        self.event_id = Some(event_id.into());
        self
    }

    pub fn with_keyboard(mut self, keyboard: Keyboard) -> Self {
        self.keyboard = Some(keyboard);
        self
    }

    pub fn with_wakeup(mut self, is_wakeup: bool) -> Self {
        self.is_wakeup = Some(is_wakeup);
        self
    }

    /// 是否为被动回复（携带 `msg_id`）。
    pub fn is_passive(&self) -> bool {
        self.msg_id.is_some()
    }
}

/// Markdown 消息体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Markdown {
    pub content: String,
}

/// 富媒体消息体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Media {
    pub file_info: String,
}

/// 按钮键盘。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keyboard {
    pub content: KeyboardContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyboardContent {
    pub rows: Vec<KeyboardRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyboardRow {
    pub buttons: Vec<Button>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Button {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_data: Option<RenderData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ButtonAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderData {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visited_label: Option<String>,
    /// 0 = 灰色线框，1 = 蓝色线框。
    #[serde(default)]
    pub style: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ButtonAction {
    /// 0 = 跳转按钮，1 = 回调按钮，2 = 指令按钮。
    #[serde(rename = "type")]
    pub action_type: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupport_tips: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msg_type_values_match_docs() {
        assert_eq!(MsgType::Text.as_u8(), 0);
        assert_eq!(MsgType::Markdown.as_u8(), 2);
        assert_eq!(MsgType::Media.as_u8(), 7);
    }

    #[test]
    fn target_paths_are_scene_isolated() {
        let g = Target::group("G1");
        let c = Target::c2c("U1");
        assert_eq!(g.messages_path(), "/v2/groups/G1/messages");
        assert_eq!(c.messages_path(), "/v2/users/U1/messages");
        assert_eq!(g.files_path(), "/v2/groups/G1/files");
        assert_eq!(c.files_path(), "/v2/users/U1/files");
        assert_ne!(g.key(), c.key());
    }

    #[test]
    fn text_message_omits_none_fields() {
        let m = OutMessage::text("hi").with_msg_id("M1").with_msg_seq(2);
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"msg_type":0,"content":"hi","msg_id":"M1","msg_seq":2}"#);
    }

    #[test]
    fn media_message_shape() {
        let m = OutMessage::media("FILEINFO").with_msg_id("M1").with_msg_seq(1);
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"msg_type":7,"media":{"file_info":"FILEINFO"},"msg_id":"M1","msg_seq":1}"#);
    }
}
