//! 事件模型与两阶段解析。
//!
//! 第一阶段只把网关 payload 解析成 [`crate::payload::RawPayload`]（`d` 保持原始 JSON 文本），
//! 第二阶段再按 `t` 精确反序列化到具体类型。这样未知事件不会因为形状不符而报错，
//! 也避免了对每个事件都做一次完整的通用 JSON 解析。

pub mod message;
pub mod notice;

pub use message::{
    ArkData, MessageAttachment, MessageEvent, MessageScene, MsgElement, User,
};
pub use notice::{InteractionCreate, RawNotice};

use std::sync::Arc;

use serde_json::value::RawValue;

use crate::error::ApiError;
use crate::payload::Ready;

/// 宽松的字符串反序列化：同时接受 JSON 字符串与数字。
///
/// ⚠️ 线上实测：官方文档声明 `timestamp` 是 RFC3339 字符串，
/// 但**某些事件实际下发的是 Unix 时间戳整数**（如 `1790645533`）。
/// 严格按 `String` 解析会直接导致**网关断连**——一个字段的类型差异
/// 不该让整个长连接重连。
pub(crate) fn de_lenient_opt_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| match v {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }))
}

/// 已识别的事件。
#[derive(Debug, Clone)]
pub enum Event {
    /// 鉴权成功（`t = "READY"`）。
    Ready(Box<Ready>),
    /// 断线重连后事件补发完毕（`t = "RESUMED"`）。
    Resumed,
    /// 单聊消息。
    ///
    /// 用 `Arc` 而非 `Box`：事件会被去重缓存、日志、路由多处共享，克隆应零拷贝。
    C2cMessage(Arc<MessageEvent>),
    /// 群聊 @机器人 消息。
    GroupAtMessage(Arc<MessageEvent>),
    /// 群聊全量消息。
    GroupMessage(Arc<MessageEvent>),
    /// 好友添加 / 删除。
    FriendAdd(RawNotice),
    FriendDel(RawNotice),
    /// 机器人被加入 / 移出群聊。
    GroupAddRobot(RawNotice),
    GroupDelRobot(RawNotice),
    /// 主动消息开关变更、群通知开关变更。
    Notice { name: String, notice: RawNotice },
    /// 按钮 / 菜单互动。
    Interaction(Box<InteractionCreate>),
    /// 未识别事件（保留事件名，便于排查）。
    Unknown { name: String },
}

impl Event {
    /// 事件名（与官方 `t` 字段一致）。
    pub fn name(&self) -> &str {
        match self {
            Event::Ready(_) => "READY",
            Event::Resumed => "RESUMED",
            Event::C2cMessage(_) => "C2C_MESSAGE_CREATE",
            Event::GroupAtMessage(_) => "GROUP_AT_MESSAGE_CREATE",
            Event::GroupMessage(_) => "GROUP_MESSAGE_CREATE",
            Event::FriendAdd(_) => "FRIEND_ADD",
            Event::FriendDel(_) => "FRIEND_DEL",
            Event::GroupAddRobot(_) => "GROUP_ADD_ROBOT",
            Event::GroupDelRobot(_) => "GROUP_DEL_ROBOT",
            Event::Notice { name, .. } => name,
            Event::Interaction(_) => "INTERACTION_CREATE",
            Event::Unknown { name } => name,
        }
    }

    /// 取出消息事件（若是）。
    pub fn as_message(&self) -> Option<&MessageEvent> {
        match self {
            Event::C2cMessage(m) | Event::GroupAtMessage(m) | Event::GroupMessage(m) => Some(m),
            _ => None,
        }
    }

    /// 第二阶段解析：按事件名把 `d` 反序列化为具体类型。
    ///
    /// `event_id` 是 payload **外层** 的 `id`，事件类被动回复需要它作为凭证，
    /// 因此必须透传进来而不是丢弃。
    pub fn parse(
        name: Option<&str>,
        d: Option<&RawValue>,
        event_id: Option<&str>,
    ) -> Result<Option<Event>, ApiError> {
        let Some(name) = name else {
            return Ok(None);
        };
        let Some(d) = d else {
            return Ok(None);
        };
        let raw = d.get();
        let event_id = event_id.map(str::to_string);
        let with_id = |mut n: RawNotice| {
            n.event_id = event_id.clone();
            n
        };

        let ev = match name {
            "READY" => Event::Ready(Box::new(serde_json::from_str::<Ready>(raw)?)),
            "RESUMED" => Event::Resumed,

            "C2C_MESSAGE_CREATE" => {
                Event::C2cMessage(Arc::new(serde_json::from_str::<MessageEvent>(raw)?))
            }
            "GROUP_AT_MESSAGE_CREATE" => {
                Event::GroupAtMessage(Arc::new(serde_json::from_str::<MessageEvent>(raw)?))
            }
            "GROUP_MESSAGE_CREATE" => {
                Event::GroupMessage(Arc::new(serde_json::from_str::<MessageEvent>(raw)?))
            }

            "FRIEND_ADD" => Event::FriendAdd(with_id(serde_json::from_str(raw)?)),
            "FRIEND_DEL" => Event::FriendDel(with_id(serde_json::from_str(raw)?)),
            "GROUP_ADD_ROBOT" => Event::GroupAddRobot(with_id(serde_json::from_str(raw)?)),
            "GROUP_DEL_ROBOT" => Event::GroupDelRobot(with_id(serde_json::from_str(raw)?)),

            "C2C_MSG_REJECT" | "C2C_MSG_RECEIVE" | "GROUP_MSG_REJECT" | "GROUP_MSG_RECEIVE"
            | "MESSAGE_AUDIT_PASS" | "MESSAGE_AUDIT_REJECT" => Event::Notice {
                name: name.to_string(),
                notice: with_id(serde_json::from_str(raw).unwrap_or_default()),
            },

            "INTERACTION_CREATE" => {
                Event::Interaction(Box::new(serde_json::from_str::<InteractionCreate>(raw)?))
            }

            other => Event::Unknown { name: other.to_string() },
        };

        Ok(Some(ev))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(s: &str) -> Box<RawValue> {
        serde_json::from_str::<Box<RawValue>>(s).unwrap()
    }

    #[test]
    fn parses_c2c_message_event() {
        let d = raw(r#"{"id":"M1","author":{"user_openid":"U1"},"content":"hi"}"#);
        let ev = Event::parse(Some("C2C_MESSAGE_CREATE"), Some(&d), None).unwrap().unwrap();
        assert_eq!(ev.name(), "C2C_MESSAGE_CREATE");
        assert_eq!(ev.as_message().unwrap().trimmed(), "hi");
    }

    #[test]
    fn parses_group_at_message_event() {
        let d = raw(r#"{"id":"M2","author":{"member_openid":"MB1"},"content":"签到","group_openid":"G1"}"#);
        let ev = Event::parse(Some("GROUP_AT_MESSAGE_CREATE"), Some(&d), None).unwrap().unwrap();
        assert!(ev.as_message().unwrap().is_group());
    }

    #[test]
    fn resumed_carries_empty_d_string() {
        let d = raw(r#""""#);
        let ev = Event::parse(Some("RESUMED"), Some(&d), None).unwrap().unwrap();
        assert_eq!(ev.name(), "RESUMED");
    }

    #[test]
    fn unknown_event_is_preserved_not_failed() {
        let d = raw(r#"{"whatever":1}"#);
        let ev = Event::parse(Some("SOME_FUTURE_EVENT"), Some(&d), None).unwrap().unwrap();
        assert_eq!(ev.name(), "SOME_FUTURE_EVENT");
    }

    #[test]
    fn notice_with_unexpected_shape_still_parses() {
        let d = raw(r#"{"unknown_field":true}"#);
        let ev = Event::parse(Some("GROUP_MSG_RECEIVE"), Some(&d), None).unwrap().unwrap();
        match ev {
            Event::Notice { name, .. } => assert_eq!(name, "GROUP_MSG_RECEIVE"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn missing_t_or_d_returns_none() {
        assert!(Event::parse(None, Some(&raw("{}")), None).unwrap().is_none());
        assert!(Event::parse(Some("READY"), None, None).unwrap().is_none());
    }

    /// 事件类被动回复的凭据来自 payload **外层** 的 id，必须被保留。
    #[test]
    fn notice_keeps_outer_event_id() {
        let d = raw(r#"{"group_openid":"G1","op_member_openid":"U1"}"#);
        let ev = Event::parse(Some("GROUP_ADD_ROBOT"), Some(&d), Some("EVENT-123"))
            .unwrap()
            .unwrap();
        match ev {
            Event::GroupAddRobot(notice) => {
                assert_eq!(notice.event_id.as_deref(), Some("EVENT-123"));
                assert_eq!(notice.group_openid.as_deref(), Some("G1"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// 回归：线上实测某些事件把 timestamp 下发成 Unix 时间戳整数，
    /// 严格按 String 解析会导致网关断连。
    #[test]
    fn integer_timestamp_is_accepted() {
        let d = raw(
            r#"{"id":"M1","author":{"user_openid":"U1"},"content":"hi","timestamp":1790645533}"#,
        );
        let ev = Event::parse(Some("C2C_MESSAGE_CREATE"), Some(&d), None)
            .unwrap()
            .unwrap();
        assert_eq!(
            ev.as_message().unwrap().timestamp.as_deref(),
            Some("1790645533"),
            "整数时间戳应被转成字符串而不是解析失败"
        );
    }

    #[test]
    fn string_timestamp_still_works() {
        let d = raw(
            r#"{"id":"M1","author":{"user_openid":"U1"},"timestamp":"2026-07-21T10:30:00+08:00"}"#,
        );
        let ev = Event::parse(Some("C2C_MESSAGE_CREATE"), Some(&d), None).unwrap().unwrap();
        assert_eq!(
            ev.as_message().unwrap().timestamp.as_deref(),
            Some("2026-07-21T10:30:00+08:00")
        );
    }

    #[test]
    fn missing_outer_event_id_is_none_not_error() {
        let d = raw(r#"{"group_openid":"G1"}"#);
        let ev = Event::parse(Some("GROUP_ADD_ROBOT"), Some(&d), None).unwrap().unwrap();
        match ev {
            Event::GroupAddRobot(notice) => assert!(notice.event_id.is_none()),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
