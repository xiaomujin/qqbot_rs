use serde::Deserialize;

use crate::message::Target;

/// 「引用消息」在 `msg_elements[].message_type` 里的取值（**实测值，官方文档没写**）。
///
/// 回复一条消息时，事件里会多出一个 `message_type = 103` 的 `msg_elements` 元素：
/// 被引用消息的正文与附件都在它里面，同时 `message_scene.ext` 多出 `ref_msg_idx`。
/// 这是「回复收录」能成立的唯一依据 —— 早先基于非回复消息得出的
/// 「引用内容不在事件里」是错的。
pub const QUOTED_MESSAGE_TYPE: i32 = 103;

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
    /// 实测存在但文档未写；目前观察到为空串。
    #[serde(default)]
    pub content: Option<String>,
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

impl MessageAttachment {
    /// 能下载的 url：字段缺失与空串都算「没有」。
    ///
    /// 协议允许附件没有 url（实测引用元素里就有 `"url":""`），而空地址必定下载失败。
    /// 「空 url 不算数」这条规则放在字段旁边，收录路径按需取用 ——
    /// 返回 `Option<&str>` 而不是 `bool`，是为了让调用方**结构性地**拿不到那个空串。
    pub fn usable_url(&self) -> Option<&str> {
        self.url.as_deref().filter(|u| !u.is_empty())
    }
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
    /// 原始事件 JSON。
    ///
    /// `#[serde(skip)]`：它不是协议字段，而是由 `Event::parse` 在解析后回填。
    /// 需要它是因为类型里只声明了文档写到的字段 ——
    /// **没声明的会在反序列化时静默丢掉**，而附件、引用、聊天记录
    /// 恰恰是文档写得最含糊的部分。
    #[serde(skip)]
    pub raw: Option<String>,
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

    /// 第一个附件。
    ///
    /// **只用于日志**（定位「发了图却没被识别」）：它刻意不过滤空 url ——
    /// 日志要如实反映事件里有什么。要挑「能下载的那张」请用
    /// [`Self::quoted_attachments`] 那一侧的做法，由调用方过滤。
    pub fn first_attachment(&self) -> Option<&MessageAttachment> {
        self.attachments.first()
    }

    /// **被引用消息**里的所有附件，按 `msg_elements` 的顺序展开。
    ///
    /// 返回迭代器而不是「第一个」：附件允许没有 url（协议如此，且这种附件必定下载
    /// 失败），调用方要能继续往后找那一张真图 —— 返回 `Option` 会让「第一个恰好
    /// 没有 url」变成「这张消息没有图」。协议知识（那个 103）
    /// 留在协议类型旁边，插件不必自己解析原文。
    pub fn quoted_attachments(&self) -> impl Iterator<Item = &MessageAttachment> {
        self.msg_elements
            .iter()
            .filter(|e| e.message_type == Some(QUOTED_MESSAGE_TYPE))
            .flat_map(|e| e.attachments.iter())
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

    /// 实测结构：回复一条带图的消息时，被引用消息的附件在 `msg_elements` 里，
    /// 该元素的 `message_type = 103`，`message_scene.ext` 同时多出 `ref_msg_idx`。
    #[test]
    fn quoted_attachments_come_from_the_103_element() {
        let raw = r#"{"id":"M1","content":" 收录 引用图","group_openid":"G1","message_type":103,"message_scene":{"ext":["ref_msg_idx=REFIDX_AAA","msg_idx=REFIDX_BBB","auth_token=T"],"source":"default"},"msg_elements":[{"message_type":103,"content":"1\n2","msg_idx":"REFIDX_AAA","attachments":[{"content":"","content_type":"image/jpeg","filename":"a.jpeg","height":440,"size":47195,"url":"https://example.invalid/quoted.jpeg","width":583}]}]}"#;
        let ev: MessageEvent = serde_json::from_str(raw).unwrap();
        let att = ev.quoted_attachments().next().expect("应当从引用消息里取到附件");
        assert_eq!(att.url.as_deref(), Some("https://example.invalid/quoted.jpeg"));
        assert_eq!(att.filename.as_deref(), Some("a.jpeg"));
        assert_eq!(att.size, Some(47195));
        assert_eq!(att.content_type.as_deref(), Some("image/jpeg"));
        // 引用附件与「本条消息的附件」是两个来源，不能互相顶替。
        assert!(ev.first_attachment().is_none());
    }

    #[test]
    fn quoted_attachments_ignore_other_element_types() {
        // message_type 不是 103 的元素不是「被引用的消息」，不能当成图源。
        let ev: MessageEvent = serde_json::from_str(
            r#"{"id":"M1","msg_elements":[{"message_type":0,"attachments":[{"url":"https://x/a.png"}]}]}"#,
        )
        .unwrap();
        assert_eq!(ev.quoted_attachments().count(), 0);
    }

    #[test]
    fn quoted_attachments_is_empty_without_attachments() {
        let ev: MessageEvent = serde_json::from_str(
            r#"{"id":"M1","msg_elements":[{"message_type":103,"content":"纯文本"}]}"#,
        )
        .unwrap();
        assert_eq!(ev.quoted_attachments().count(), 0);
    }

    /// 回归：引用消息里**第一个**附件没有 url 时，迭代器必须能给出第二个。
    ///
    /// 收录原来只取「第一个」再在别处过滤空 url，于是这种情况会被当成
    /// 「这条消息没有图」，转而回查历史消息 —— 拿到上一条消息的图且不报错。
    #[test]
    fn quoted_attachments_keep_going_past_an_empty_url() {
        let ev: MessageEvent = serde_json::from_str(
            r#"{"id":"M1","msg_elements":[{"message_type":103,"attachments":[{"url":""},{"url":"https://example.invalid/real.png"}]}]}"#,
        )
        .unwrap();
        let urls: Vec<_> = ev.quoted_attachments().filter_map(|a| a.url.as_deref()).collect();
        assert_eq!(urls, vec!["", "https://example.invalid/real.png"]);
    }

    /// 多个 103 元素（多层引用）按出现顺序展开。
    #[test]
    fn quoted_attachments_flatten_every_103_element() {
        let ev: MessageEvent = serde_json::from_str(
            r#"{"id":"M1","msg_elements":[
                 {"message_type":103,"attachments":[{"url":"https://x/1.png"}]},
                 {"message_type":0,"attachments":[{"url":"https://x/ignored.png"}]},
                 {"message_type":103,"attachments":[{"url":"https://x/2.png"}]}
               ]}"#,
        )
        .unwrap();
        let urls: Vec<_> = ev.quoted_attachments().filter_map(|a| a.url.as_deref()).collect();
        assert_eq!(urls, vec!["https://x/1.png", "https://x/2.png"]);
    }

    #[test]
    fn own_attachment_and_quoted_attachment_are_distinct() {
        let ev: MessageEvent = serde_json::from_str(
            r#"{"id":"M1","attachments":[{"url":"https://example.invalid/own.png"}],"msg_elements":[{"message_type":103,"attachments":[{"url":"https://example.invalid/quoted.png"}]}]}"#,
        )
        .unwrap();
        assert_eq!(
            ev.first_attachment().and_then(|a| a.url.as_deref()),
            Some("https://example.invalid/own.png")
        );
        assert_eq!(
            ev.quoted_attachments().next().and_then(|a| a.url.as_deref()),
            Some("https://example.invalid/quoted.png")
        );
    }

    #[test]
    fn usable_url_rejects_missing_and_empty() {
        let att = |url: Option<&str>| MessageAttachment {
            url: url.map(str::to_string),
            ..MessageAttachment::default()
        };
        assert_eq!(att(Some("https://x/a.png")).usable_url(), Some("https://x/a.png"));
        assert_eq!(att(Some("")).usable_url(), None, "空串必须当成「没有」");
        assert_eq!(att(None).usable_url(), None);
    }
}
