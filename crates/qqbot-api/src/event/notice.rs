use serde::Deserialize;

use crate::message::Target;

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
    /// 事件 id（来自 payload **外层** 的 `id` 字段，不在 `d` 里）。
    ///
    /// 与 [`RawNotice::event_id`] 同理：它是事件类被动回复的凭证，
    /// 缺了它就只能发主动消息（受 20/min、1000/天 配额约束）。
    #[serde(skip)]
    pub event_id: Option<String>,
    /// 互动类型。
    #[serde(default)]
    pub interaction_type: i32,
    /// 会话类型。
    #[serde(default)]
    pub chat_type: i32,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    /// 群聊场景存在，单聊场景为空。
    #[serde(default)]
    pub group_openid: Option<String>,
    /// 群聊场景下触发按钮的用户。
    #[serde(default)]
    pub group_member_openid: Option<String>,
    /// 单聊场景存在。
    #[serde(default)]
    pub user_openid: Option<String>,
    #[serde(default, deserialize_with = "crate::event::de_lenient_opt_string")]
    pub timestamp: Option<String>,
}

impl InteractionCreate {
    /// 推导发送目标。
    ///
    /// 与 [`crate::event::MessageEvent::target`] 同风格：以 `group_openid`
    /// 是否存在区分群聊 / 单聊，而不是信任 `chat_type` 的枚举值
    /// （各端下发的取值并不一致）。
    pub fn target(&self) -> Option<Target> {
        match self.group_openid.as_deref() {
            Some(g) if !g.is_empty() => Some(Target::group(g)),
            _ => self
                .user_openid
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(Target::c2c),
        }
    }

    /// 触发按钮的用户：群聊取群成员，单聊取用户。
    pub fn sender_openid(&self) -> Option<&str> {
        self.group_member_openid
            .as_deref()
            .or(self.user_openid.as_deref())
            .filter(|s| !s.is_empty())
    }

    /// 回调按钮携带的 `data`。
    ///
    /// 兼容两种形状：
    /// - `data` 是对象时取其 `button_data` 字段（文档形态）；
    /// - `data` 本身就是字符串时直接用它（部分端如此下发）。
    pub fn button_data(&self) -> Option<String> {
        match self.data.as_ref()? {
            serde_json::Value::Object(map) => map
                .get("button_data")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            serde_json::Value::String(s) => Some(s.clone()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> InteractionCreate {
        serde_json::from_str(raw).unwrap()
    }

    /// 契约：button_data 既能从 JSON 对象的字段里取，也能把 data 当裸字符串用。
    #[test]
    fn button_data_reads_object_field_or_bare_string() {
        let obj = parse(r#"{"id":"I1","data":{"type":1,"button_data":"task:x:page:2"}}"#);
        assert_eq!(obj.button_data().as_deref(), Some("task:x:page:2"));

        let bare = parse(r#"{"id":"I2","data":"task:x:page:2"}"#);
        assert_eq!(bare.button_data().as_deref(), Some("task:x:page:2"));

        // 对象里没有 button_data 时不能瞎猜
        let empty = parse(r#"{"id":"I3","data":{"type":1}}"#);
        assert_eq!(empty.button_data(), None);
        let missing = parse(r#"{"id":"I4"}"#);
        assert_eq!(missing.button_data(), None);
    }

    /// 目标推导与发送者都只看 openid 是否存在，缺失时返回 None 而不是空串。
    #[test]
    fn target_and_sender_follow_the_openid_shape() {
        let group = parse(r#"{"id":"I1","group_openid":"G1","group_member_openid":"U1"}"#);
        assert_eq!(group.target(), Some(Target::group("G1")));
        assert_eq!(group.sender_openid(), Some("U1"));

        let c2c = parse(r#"{"id":"I2","user_openid":"U2"}"#);
        assert_eq!(c2c.target(), Some(Target::c2c("U2")));
        assert_eq!(c2c.sender_openid(), Some("U2"));

        let unknown = parse(r#"{"id":"I3"}"#);
        assert_eq!(unknown.target(), None);
        assert_eq!(unknown.sender_openid(), None);

        // 空字符串等同于缺失（线上见过空 openid）
        let blank = parse(r#"{"id":"I4","group_openid":"","user_openid":""}"#);
        assert_eq!(blank.target(), None);
    }
}
