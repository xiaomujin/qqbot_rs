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

    /// 流式消息路径。
    ///
    /// ⚠️ 官方明确「**群消息不支持流式参数**」，因此群聊返回 `None`。
    /// 返回 `Option` 而不是硬拼一个必定被服务端拒绝的路径。
    pub fn stream_messages_path(&self) -> Option<String> {
        match self {
            Target::C2c { user_openid } => Some(format!("/v2/users/{user_openid}/stream_messages")),
            Target::Group { .. } => None,
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
    /// 引用回复。填写后以引用形式展示，关联上下文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_reference: Option<MessageReference>,
}

impl OutMessage {
    pub fn text(content: impl Into<String>) -> Self {
        Self::bare(MsgType::Text).with_content(content)
    }

    pub fn markdown(content: impl Into<String>) -> Self {
        let mut m = Self::bare(MsgType::Markdown);
        m.markdown = Some(Markdown::new(content));
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
            message_reference: None,
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

    /// 引用某条消息（引用回复）。
    pub fn with_message_reference(mut self, message_id: impl Into<String>) -> Self {
        self.message_reference = Some(MessageReference { message_id: message_id.into() });
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
    /// 是否校验图片转存结果。`true` 时图片转存失败会直接报错、**消息不发送**；默认 `false`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_verify_image_resource: Option<bool>,
}

impl Markdown {
    pub fn new(content: impl Into<String>) -> Self {
        Self { content: content.into(), force_verify_image_resource: None }
    }
}

/// 富媒体消息体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Media {
    pub file_info: String,
}

/// 内嵌键盘。
///
/// 两种形态**互斥**：
/// - 短形式：只传 `id`，使用平台预设键盘模板；
/// - 长形式：传 `content.rows`，自定义按钮布局。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Keyboard {
    /// 平台预设键盘模板 ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 自定义键盘布局。与 `id` 互斥。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<KeyboardContent>,
}

impl Keyboard {
    /// 长形式：自定义按钮布局。
    pub fn rows(rows: Vec<KeyboardRow>) -> Self {
        Self { id: None, content: Some(KeyboardContent { rows }) }
    }

    /// 短形式：使用平台预设模板。
    pub fn template(id: impl Into<String>) -> Self {
        Self { id: Some(id.into()), content: None }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeyboardContent {
    #[serde(default)]
    pub rows: Vec<KeyboardRow>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeyboardRow {
    #[serde(default)]
    pub buttons: Vec<Button>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Button {
    /// 按钮 ID，同一键盘内唯一。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_data: Option<RenderData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ButtonAction>,
    /// 分组 ID。同一分组内有一个按钮被点击后，其它按钮变灰不可点。
    /// ⚠️ 仅 `action.type = 1`（回调按钮）时有效。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RenderData {
    /// 按钮文字，最多 10 字符。
    pub label: String,
    /// 点击后文字，不传则保持不变。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visited_label: Option<String>,
    /// 0 = 灰色线框，1 = 蓝色线框，3 = 白底红字，4 = 蓝底白字。
    #[serde(default)]
    pub style: u8,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ButtonAction {
    /// 0 = 跳转按钮，1 = 回调按钮，2 = 指令按钮。
    #[serde(rename = "type")]
    pub action_type: u8,
    /// 回调数据，`type = 1 / 2` 时必填。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// 操作权限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<Permission>,
    /// 【已废弃】可点击次数限制，0 = 无限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub click_limit: Option<u32>,
    /// 版本过低时的提示文案。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupport_tips: Option<String>,
    /// 指令按钮可用：点击后直接自动发送 `data`。⚠️ **仅单聊可用**，默认 false。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<bool>,
    /// 指令按钮可用：指令是否带引用回复本消息，默认 false。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<bool>,
    /// 仅指令按钮有效，设置后会忽略 `enter`。
    /// `1` = 点击唤起手Q选图器（仅手机端单聊，桌面端不支持）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<u32>,
    /// 用户点击时的二次确认弹窗。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modal: Option<Modal>,
}

/// 按钮操作权限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permission {
    /// 0 = 指定用户，1 = 管理员，2 = 所有人。
    #[serde(rename = "type")]
    pub permission_type: u8,
    /// 有权限的用户 id 列表（`permission_type = 0` 时使用）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub specify_user_ids: Vec<String>,
    /// 有权限的身份组 id 列表。⚠️ **仅频道可用**，群聊/单聊填了也不会生效。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub specify_role_ids: Vec<String>,
}

impl Permission {
    /// 所有人可点（`type = 2`）。
    pub fn everyone() -> Self {
        Self { permission_type: 2, specify_user_ids: Vec::new(), specify_role_ids: Vec::new() }
    }

    /// 仅管理员可点（`type = 1`）。
    pub fn admin() -> Self {
        Self { permission_type: 1, specify_user_ids: Vec::new(), specify_role_ids: Vec::new() }
    }

    /// 仅指定用户可点（`type = 0`）。
    pub fn users(ids: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            permission_type: 0,
            specify_user_ids: ids.into_iter().map(Into::into).collect(),
            specify_role_ids: Vec::new(),
        }
    }
}

/// 按钮的二次确认弹窗。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modal {
    /// 提示文本。⚠️ 最多 40 字符，且**不能包含 URL**。
    pub content: String,
    /// 确认按钮文字，最多 4 字符，默认「确认」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_text: Option<String>,
    /// 取消按钮文字，最多 4 字符，默认「取消」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_text: Option<String>,
}

/// 引用回复。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageReference {
    /// 被引用消息 ID。
    ///
    /// 取法有两种，别搞混：
    /// - **非机器人**发的消息：从消息事件 `MessageScene.ext` 数组的 `msg_idx` 取；
    /// - **机器人自己**发的消息：从发消息响应的 `ext_info.ref_idx` 取。
    pub message_id: String,
}

/// 消息扩展信息（发送响应里的 `ext_info`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageExtInfo {
    /// 引用消息索引。对应消息时间 ext 里的 `msg_idx` 与 `ref_msg_idx`。
    #[serde(default)]
    pub ref_idx: Option<String>,
}

/// 流式消息的输入模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamInputMode {
    /// 默认。`content_raw` 拼接到待下发内容之后。
    #[default]
    Append,
    /// `content_raw` 是当前**全量正文**，必须以上游已下发的前缀 `SentContent` 开头；
    /// 合并后待下发区只保留未下发的后缀。
    Replace,
}

/// 流式消息的输入状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "u8", from = "u8")]
pub enum StreamInputState {
    /// 1 — 生成中。
    Generating,
    /// 10 — 生成结束。
    Finished,
}

impl StreamInputState {
    pub const fn as_u8(self) -> u8 {
        match self {
            StreamInputState::Generating => 1,
            StreamInputState::Finished => 10,
        }
    }
}

impl From<StreamInputState> for u8 {
    fn from(s: StreamInputState) -> u8 {
        s.as_u8()
    }
}

impl From<u8> for StreamInputState {
    fn from(v: u8) -> Self {
        if v == 10 {
            StreamInputState::Finished
        } else {
            StreamInputState::Generating
        }
    }
}

/// 流式消息的内容格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamContentType {
    #[default]
    Text,
    Markdown,
}

/// 流式消息分片请求体（`POST /v2/users/{user_openid}/stream_messages`）。
///
/// 规则：
/// - 每个分片携带**同一个** `stream_msg_id`，`index` 从 0 递增；
/// - **首片不传** `stream_msg_id`，由服务端生成并在响应的 `id` 里返回；
/// - `input_state = 10` 的那一片表示生成结束；
/// - ⚠️ 官方明确「**群消息不支持流式参数**」，该接口仅单聊可用。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_mode: Option<StreamInputMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_state: Option<StreamInputState>,
    /// 分片序号，从 0 递增。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<StreamContentType>,
    /// 文本内容（`content_type` 决定按纯文本还是 Markdown 解释）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_raw: Option<String>,
    /// 被动回复事件 ID（与 `msg_id` 二选一）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// 被动回复消息 ID（与 `event_id` 二选一）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
    /// 流式消息 ID。首片不填，后续分片填上一分片响应返回的 `id`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_msg_id: Option<String>,
    /// 消息序号，用于去重。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_seq: Option<u32>,
    /// 是否为召回消息。`true` 时**不校验** `msg_id` / `event_id` 有效期。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_wakeup: Option<bool>,
}

impl StreamMessage {
    /// 一个分片（`input_mode` 走官方默认的 `append`）。
    pub fn chunk(
        index: u32,
        state: StreamInputState,
        content_type: StreamContentType,
        content_raw: impl Into<String>,
    ) -> Self {
        Self {
            input_state: Some(state),
            index: Some(index),
            content_type: Some(content_type),
            content_raw: Some(content_raw.into()),
            ..Self::default()
        }
    }

    /// 沿用上一分片返回的 `stream_msg_id`。
    pub fn with_stream_msg_id(mut self, id: impl Into<String>) -> Self {
        self.stream_msg_id = Some(id.into());
        self
    }

    /// 被动回复凭证（与 `event_id` 二选一）。
    pub fn with_msg_id(mut self, msg_id: impl Into<String>) -> Self {
        self.msg_id = Some(msg_id.into());
        self
    }

    /// 被动回复凭证（与 `msg_id` 二选一）。
    pub fn with_event_id(mut self, event_id: impl Into<String>) -> Self {
        self.event_id = Some(event_id.into());
        self
    }

    pub fn with_msg_seq(mut self, msg_seq: u32) -> Self {
        self.msg_seq = Some(msg_seq);
        self
    }

    pub fn with_input_mode(mut self, mode: StreamInputMode) -> Self {
        self.input_mode = Some(mode);
        self
    }
}

/// 流式消息分片的响应体。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct StreamMessageResult {
    /// 消息 ID。**首片返回的就是后续分片要带的 `stream_msg_id`**。
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub ext_info: Option<MessageExtInfo>,
    /// 流式消息剩余长度（字符数）。
    #[serde(default)]
    pub remain_msg_len: Option<u64>,
}

/// 互动事件的回调结果码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(into = "u8", from = "u8")]
pub enum InteractionCode {
    /// 0 — 成功。
    #[default]
    Success,
    /// 1 — 操作失败。
    Failed,
    /// 2 — 操作频繁。
    Frequent,
    /// 3 — 重复操作。
    Duplicate,
    /// 4 — 没有权限。
    NoPermission,
    /// 5 — 仅管理员操作。
    AdminOnly,
}

impl InteractionCode {
    pub const fn as_u8(self) -> u8 {
        match self {
            InteractionCode::Success => 0,
            InteractionCode::Failed => 1,
            InteractionCode::Frequent => 2,
            InteractionCode::Duplicate => 3,
            InteractionCode::NoPermission => 4,
            InteractionCode::AdminOnly => 5,
        }
    }
}

impl From<InteractionCode> for u8 {
    fn from(c: InteractionCode) -> u8 {
        c.as_u8()
    }
}

impl From<u8> for InteractionCode {
    fn from(v: u8) -> Self {
        match v {
            1 => InteractionCode::Failed,
            2 => InteractionCode::Frequent,
            3 => InteractionCode::Duplicate,
            4 => InteractionCode::NoPermission,
            5 => InteractionCode::AdminOnly,
            _ => InteractionCode::Success,
        }
    }
}

/// 互动事件响应体（`PUT /interactions/{interaction_id}`）。
///
/// 收到 `INTERACTION_CREATE` 后必须回应，否则用户端会一直 loading 到超时
/// （指令回调类场景超时时间 **3 秒**）。同一个 `interaction_id` 只能回应一次。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InteractionResponse {
    /// 回调结果，默认 0（成功）。
    #[serde(default)]
    pub code: InteractionCode,
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

    // ---------- 流式消息 ----------

    #[test]
    fn stream_message_serializes_like_the_doc_example() {
        // 官方「首片消息 (input_state=1, index=0)」示例
        let m = StreamMessage::chunk(0, StreamInputState::Generating, StreamContentType::Markdown, "正在生成回答，请稍候")
            .with_msg_id("ROBOT1.0_x")
            .with_msg_seq(1);
        let v: serde_json::Value = serde_json::to_value(&m).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "input_state": 1,
                "index": 0,
                "content_type": "markdown",
                "content_raw": "正在生成回答，请稍候",
                "msg_id": "ROBOT1.0_x",
                "msg_seq": 1
            })
        );
        // 首片必须**不带** stream_msg_id
        assert!(v.get("stream_msg_id").is_none());
    }

    #[test]
    fn stream_continuation_carries_the_stream_msg_id() {
        let m = StreamMessage::chunk(2, StreamInputState::Finished, StreamContentType::Markdown, "最终结果")
            .with_stream_msg_id("a1b2c3d4")
            .with_msg_id("ROBOT1.0_x")
            .with_msg_seq(1);
        let v: serde_json::Value = serde_json::to_value(&m).unwrap();
        assert_eq!(v["stream_msg_id"], "a1b2c3d4");
        assert_eq!(v["input_state"], 10, "结束片必须是 10");
    }

    #[test]
    fn stream_message_result_parses_the_doc_example() {
        let r: StreamMessageResult = serde_json::from_str(
            r#"{"id":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","timestamp":"2026-07-21T10:00:00+08:00","ext_info":{"ref_idx":"REFIDX_xxxxxxxxxxxxxxx=="}}"#,
        )
        .unwrap();
        assert_eq!(r.id.as_deref(), Some("a1b2c3d4-e5f6-7890-abcd-ef1234567890"));
        assert_eq!(r.ext_info.and_then(|e| e.ref_idx).as_deref(), Some("REFIDX_xxxxxxxxxxxxxxx=="));
        assert_eq!(r.remain_msg_len, None, "可选字段缺失不能报错");
    }

    #[test]
    fn stream_input_state_maps_both_ways() {
        assert_eq!(StreamInputState::Generating.as_u8(), 1);
        assert_eq!(StreamInputState::Finished.as_u8(), 10);
        assert_eq!(StreamInputState::from(10u8), StreamInputState::Finished);
        assert_eq!(StreamInputState::from(1u8), StreamInputState::Generating);
        // 未知值兜底为「生成中」，不能 panic
        assert_eq!(StreamInputState::from(200u8), StreamInputState::Generating);
    }

    #[test]
    fn stream_path_is_c2c_only() {
        // 官方：「群消息不支持流式参数」
        assert_eq!(
            Target::c2c("U1").stream_messages_path().as_deref(),
            Some("/v2/users/U1/stream_messages")
        );
        assert!(Target::group("G1").stream_messages_path().is_none());
    }

    // ---------- 互动事件响应 ----------

    #[test]
    fn interaction_response_roundtrip() {
        let r = InteractionResponse { code: InteractionCode::AdminOnly };
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"code":5}"#);
        let back: InteractionResponse = serde_json::from_str(r#"{"code":5}"#).unwrap();
        assert_eq!(back.code, InteractionCode::AdminOnly);
        // 官方响应是 {}，字段缺失时应落到默认的成功
        let empty: InteractionResponse = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.code, InteractionCode::Success);
    }

    #[test]
    fn interaction_codes_match_the_doc_table() {
        assert_eq!(InteractionCode::Success.as_u8(), 0);
        assert_eq!(InteractionCode::Failed.as_u8(), 1);
        assert_eq!(InteractionCode::Frequent.as_u8(), 2);
        assert_eq!(InteractionCode::Duplicate.as_u8(), 3);
        assert_eq!(InteractionCode::NoPermission.as_u8(), 4);
        assert_eq!(InteractionCode::AdminOnly.as_u8(), 5);
    }

    // ---------- 键盘 ----------

    #[test]
    fn keyboard_matches_the_doc_example() {
        let kb = Keyboard::rows(vec![KeyboardRow {
            buttons: vec![Button {
                id: Some("btn_signin".into()),
                render_data: Some(RenderData { label: "签到".into(), visited_label: None, style: 1 }),
                action: Some(ButtonAction {
                    action_type: 2,
                    data: Some("/签到".into()),
                    permission: Some(Permission::everyone()),
                    enter: Some(true),
                    ..ButtonAction::default()
                }),
                group_id: None,
            }],
        }]);
        let m = OutMessage::markdown("## 每日签到").with_keyboard(kb).with_msg_id("M1").with_msg_seq(1);
        let v: serde_json::Value = serde_json::to_value(&m).unwrap();
        let btn = &v["keyboard"]["content"]["rows"][0]["buttons"][0];
        assert_eq!(btn["id"], "btn_signin");
        assert_eq!(btn["render_data"]["label"], "签到");
        assert_eq!(btn["render_data"]["style"], 1);
        assert_eq!(btn["action"]["type"], 2, "指令按钮的 type 必须是 2");
        assert_eq!(btn["action"]["data"], "/签到");
        assert_eq!(btn["action"]["enter"], true);
        assert_eq!(btn["action"]["permission"]["type"], 2);
        // 未设置的按钮字段不应出现在 JSON 里
        assert!(btn.get("group_id").is_none());
        assert!(btn["action"].get("click_limit").is_none());
    }

    #[test]
    fn keyboard_template_and_rows_are_mutually_exclusive() {
        let t: serde_json::Value = serde_json::to_value(Keyboard::template("kb_1")).unwrap();
        assert_eq!(t, serde_json::json!({"id": "kb_1"}));
        let r: serde_json::Value =
            serde_json::to_value(Keyboard::rows(vec![KeyboardRow::default()])).unwrap();
        assert_eq!(r, serde_json::json!({"content": {"rows": [{"buttons": []}]}}));
    }

    #[test]
    fn keyboard_deserializes_from_the_doc_json() {
        // 官方「消息交互概述」里的按钮示例（含 specify_role_ids / click_limit 等可选字段）
        let kb: Keyboard = serde_json::from_str(
            r#"{"id":"keyboard_id_xxx","content":{"rows":[{"buttons":[{"id":"button_1","render_data":{"label":"确认","visited_label":"已确认","style":1},"action":{"type":2,"permission":{"type":2,"specify_role_ids":[],"specify_user_ids":[]},"click_limit":1,"data":"/action_confirm","at_bot_show_channel_list":false,"reply":true,"enter":true}}]}]}}"#,
        )
        .unwrap();
        let rows = kb.content.expect("content 存在").rows;
        let btn = &rows[0].buttons[0];
        assert_eq!(btn.id.as_deref(), Some("button_1"));
        let action = btn.action.as_ref().expect("action 存在");
        assert_eq!(action.action_type, 2);
        assert_eq!(action.click_limit, Some(1));
        assert_eq!(action.reply, Some(true));
        assert_eq!(action.permission.as_ref().map(|p| p.permission_type), Some(2));
    }

    #[test]
    fn permission_constructors() {
        assert_eq!(serde_json::to_value(Permission::everyone()).unwrap(), serde_json::json!({"type": 2}));
        assert_eq!(serde_json::to_value(Permission::admin()).unwrap(), serde_json::json!({"type": 1}));
        assert_eq!(
            serde_json::to_value(Permission::users(["u1", "u2"])).unwrap(),
            serde_json::json!({"type": 0, "specify_user_ids": ["u1", "u2"]})
        );
    }

    #[test]
    fn message_reference_shape() {
        let m = OutMessage::media("FILEINFO")
            .with_msg_id("M1")
            .with_msg_seq(2)
            .with_message_reference("ROBOT1.0_yyy");
        let v: serde_json::Value = serde_json::to_value(&m).unwrap();
        assert_eq!(v["message_reference"], serde_json::json!({"message_id": "ROBOT1.0_yyy"}));
        // 不设置时不应出现该字段
        let bare: serde_json::Value = serde_json::to_value(OutMessage::text("hi")).unwrap();
        assert!(bare.get("message_reference").is_none());
    }
}

