use serde::Deserialize;

/// 通知类事件的宽松载体。
///
/// 官方对 `FRIEND_ADD` / `GROUP_ADD_ROBOT` / `*_MSG_REJECT` 等事件的字段
/// 在不同版本间有差异，这里全部设为可选，保证**任何形状都不会解析失败**。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawNotice {
    /// 事件 id（来自 payload **外层** 的 `id` 字段，不在 `d` 里）。
    ///
    /// 官方规定「被动消息（响应事件）」要携带 `event_id`，凭据就是它。
    /// 之前解析时把它丢掉了，导致事件类被动回复根本无从发起。
    #[serde(skip)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub openid: Option<String>,
    #[serde(default)]
    pub user_openid: Option<String>,
    #[serde(default)]
    pub group_openid: Option<String>,
    /// 操作者（`GROUP_ADD_ROBOT` / `GROUP_DEL_ROBOT` 场景）。
    #[serde(default)]
    pub op_member_openid: Option<String>,
    #[serde(default, deserialize_with = "crate::event::de_lenient_opt_string")]
    pub timestamp: Option<String>,
}

impl RawNotice {
    /// 该通知涉及的会话 key（若有）。
    pub fn session_key(&self) -> Option<String> {
        if let Some(g) = self.group_openid.as_deref().filter(|s| !s.is_empty()) {
            return Some(format!("group:{g}"));
        }
        self.user_openid
            .as_deref()
            .or(self.openid.as_deref())
            .filter(|s| !s.is_empty())
            .map(|u| format!("c2c:{u}"))
    }
}

/// 按钮 / 菜单互动事件。
#[derive(Debug, Clone, Deserialize)]
pub struct InteractionCreate {
    pub id: String,
    /// 互动类型。
    #[serde(default)]
    pub interaction_type: i32,
    /// 会话类型。
    #[serde(default)]
    pub chat_type: i32,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "crate::event::de_lenient_opt_string")]
    pub timestamp: Option<String>,
}
