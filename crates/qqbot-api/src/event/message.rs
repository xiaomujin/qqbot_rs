use serde::Deserialize;

use crate::message::Target;

/// 用户对象。
///
/// 所有字段都可缺省：不同事件场景下官方只会填充其中一部分
/// （群聊填 `member_openid`，单聊填 `user_openid`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct User {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub bot: bool,
    #[serde(default)]
    pub union_openid: Option<String>,
    #[serde(default)]
    pub union_user_account: Option<String>,
    /// 单聊场景的用户标识。
    #[serde(default)]
    pub user_openid: Option<String>,
    /// 群聊场景的群成员标识。
    #[serde(default)]
    pub member_openid: Option<String>,
    /// `member` / `admin` / `owner`。
    #[serde(default)]
    pub member_role: Option<String>,
}

/// 消息场景上下文。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MessageScene {
    #[serde(default)]
    pub source: Option<String>,
    /// `key=value` 形式，常见键：`msg_idx`、`ref_msg_idx`、`auth_token`。
    #[serde(default)]
    pub ext: Vec<String>,
}

/// 消息附件。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MessageAttachment {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub voice_wav_url: Option<String>,
    #[serde(default)]
    pub asr_refer_text: Option<String>,
}

/// 结构化卡片数据。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ArkData {
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub ark_type: Option<String>,
    #[serde(default)]
    pub ark_name: Option<String>,
    #[serde(default)]
    pub fields: Option<serde_json::Value>,
}

/// 消息元素（支持递归嵌套）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MsgElement {
    #[serde(default)]
    pub msg_idx: Option<String>,
    #[serde(default)]
    pub author: Option<User>,
    #[serde(default)]
    pub message_type: Option<i32>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub attachments: Vec<MessageAttachment>,
    #[serde(default)]
    pub ark_data: Option<ArkData>,
    #[serde(default)]
    pub msg_elements: Vec<MsgElement>,
}

/// 群聊 / 单聊消息事件的统一载体。
///
/// 官方对 `C2C_MESSAGE_CREATE`、`GROUP_AT_MESSAGE_CREATE`、`GROUP_MESSAGE_CREATE`
/// 使用同一套字段，差异只在 `group_openid` 是否存在，因此合并为一个结构。
#[derive(Debug, Clone, Deserialize)]
pub struct MessageEvent {
    /// 消息 ID，可用于被动回复与撤回。
    pub id: String,
    #[serde(default)]
    pub author: User,
    /// 消息文本。
    ///
    /// ⚠️ **全量模式下 @机器人 不会被剥离**：`GROUP_MESSAGE_CREATE` 的 content
    /// 形如 `<@9FC25631...> 日报`（实测）。只有 `GROUP_AT_MESSAGE_CREATE` 才带
    /// 剥离行为，而全量模式不会下发该事件。因此匹配前必须走 [`Self::trimmed`]。
    #[serde(default)]
    pub content: String,
    /// 群聊场景存在，单聊场景为空。
    #[serde(default)]
    pub group_openid: Option<String>,
    #[serde(default, deserialize_with = "crate::event::de_lenient_opt_string")]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub message_type: Option<i32>,
    #[serde(default)]
    pub message_scene: Option<MessageScene>,
    #[serde(default)]
    pub attachments: Vec<MessageAttachment>,
    /// 消息中 @ 的用户（不含机器人自身）。
    #[serde(default)]
    pub mentions: Vec<User>,
    #[serde(default)]
    pub ark_data: Option<ArkData>,
    #[serde(default)]
    pub msg_elements: Vec<MsgElement>,
}

impl MessageEvent {
    pub fn is_group(&self) -> bool {
        self.group_openid.is_some()
    }

    /// 推导发送目标。
    pub fn target(&self) -> Option<Target> {
        match self.group_openid.as_deref() {
            Some(g) if !g.is_empty() => Some(Target::group(g)),
            _ => self
                .author
                .user_openid
                .as_deref()
                .or(self.author.id.as_deref())
                .filter(|s| !s.is_empty())
                .map(Target::c2c),
        }
    }

    /// 发送者标识：群聊优先 `member_openid`，单聊用 `user_openid`。
    pub fn sender_openid(&self) -> Option<&str> {
        self.author
            .member_openid
            .as_deref()
            .or(self.author.user_openid.as_deref())
            .or(self.author.id.as_deref())
    }

    /// 群内角色：`member` / `admin` / `owner`。
    pub fn sender_role(&self) -> Option<&str> {
        self.author.member_role.as_deref()
    }

    pub fn is_admin(&self) -> bool {
        matches!(self.sender_role(), Some("admin") | Some("owner"))
    }

    /// 去除首尾空白与**开头 @提及**后的命令文本。
    ///
    /// 这是路由匹配的唯一依据。全量模式下官方不剥离 @机器人 前缀，
    /// content 形如 `<@9FC25631...> 日报`；不处理的话，
    /// 用户 **@机器人 发命令永远匹配不上**（裸关键词却正常，很容易误判成权限问题）。
    ///
    /// 剥掉的是**任意**开头提及而非「仅机器人」：群聊里机器人的 openid 是
    /// **按群**下发的，事先无从得知；而「@某人 日报」这种消息本来也是在跟机器人说话。
    /// 正文中间的提及**不动** —— 那通常是消息内容的一部分。
    pub fn trimmed(&self) -> &str {
        let mut text = self.content.trim();
        while let Some(rest) = text.strip_prefix("<@")
            && let Some(end) = rest.find('>')
        {
            text = rest[end + 1..].trim_start();
        }
        text
    }

    /// 第一个附件（常用于取用户发送的图片）。
    pub fn first_attachment(&self) -> Option<&MessageAttachment> {
        self.attachments.first()
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    /// 全量模式下 @机器人 不会被服务端剥离，而是内联成 `<@openid>` 文本。
    /// 实测：`content = "<@0F1E2D3C4B5A69788796A5B4C3D2E1F0> 日报"`（openid 已脱敏）。
    #[test]
    fn trimmed_strips_leading_mention() {
        let ev: MessageEvent = serde_json::from_str(
            r#"{"id":"M1","content":"<@0F1E2D3C4B5A69788796A5B4C3D2E1F0> 日报","group_openid":"G1"}"#,
        )
        .unwrap();
        assert_eq!(ev.trimmed(), "日报");
    }

    #[test]
    fn trimmed_strips_repeated_mentions() {
        let ev: MessageEvent = serde_json::from_str(
            r#"{"id":"M1","content":"<@AAA> <@BBB>   查子弹 7N39","group_openid":"G1"}"#,
        )
        .unwrap();
        assert_eq!(ev.trimmed(), "查子弹 7N39");
    }

    #[test]
    fn trimmed_keeps_inline_and_malformed_mentions() {
        // 正文中间的提及是消息内容的一部分，不能动。
        let inline: MessageEvent = serde_json::from_str(
            r#"{"id":"M1","content":"喊 <@AAA> 来","group_openid":"G1"}"#,
        )
        .unwrap();
        assert_eq!(inline.trimmed(), "喊 <@AAA> 来");

        // 没有闭合尖括号时不能死循环，也不能把内容吃掉。
        let broken: MessageEvent = serde_json::from_str(
            r#"{"id":"M2","content":"<@unterminated","group_openid":"G1"}"#,
        )
        .unwrap();
        assert_eq!(broken.trimmed(), "<@unterminated");
    }

    #[test]
    fn mention_only_message_becomes_empty() {
        let ev: MessageEvent = serde_json::from_str(
            r#"{"id":"M1","content":"<@AAA>","group_openid":"G1"}"#,
        )
        .unwrap();
        assert_eq!(ev.trimmed(), "");
    }

    #[test]
    fn parses_group_at_message() {
        let raw = r#"{
            "id":"MSG1",
            "author":{"id":"U1","member_openid":"M1","member_role":"admin","username":"小明"},
            "content":"签到",
            "group_openid":"G1",
            "timestamp":"2026-07-21T10:30:00+08:00"
        }"#;
        let ev: MessageEvent = serde_json::from_str(raw).unwrap();
        assert!(ev.is_group());
        assert_eq!(ev.target(), Some(Target::group("G1")));
        assert_eq!(ev.sender_openid(), Some("M1"));
        assert!(ev.is_admin());
        assert_eq!(ev.trimmed(), "签到");
    }

    #[test]
    fn parses_c2c_message_without_group() {
        let raw = r#"{"id":"MSG2","author":{"user_openid":"U2"},"content":"hi"}"#;
        let ev: MessageEvent = serde_json::from_str(raw).unwrap();
        assert!(!ev.is_group());
        assert_eq!(ev.target(), Some(Target::c2c("U2")));
        assert_eq!(ev.sender_openid(), Some("U2"));
        assert!(!ev.is_admin());
    }

    #[test]
    fn missing_author_does_not_fail_parse() {
        let ev: MessageEvent = serde_json::from_str(r#"{"id":"X","group_openid":"G"}"#).unwrap();
        assert_eq!(ev.target(), Some(Target::group("G")));
        assert_eq!(ev.sender_openid(), None);
    }
}
