//! 存储层的领域模型。

use std::borrow::Cow;
use std::time::{SystemTime, UNIX_EPOCH};

/// 会话类型。与 `qqbot_api::Target` 的两种场景一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Group,
    C2c,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Group => "group",
            Scope::C2c => "c2c",
        }
    }
}

/// 单条消息入库时允许保留的最大字符数。
///
/// 群消息本身长度有限，但异常输入（粘贴超长文本）可能把库撑大；
/// 截断保证单行体积有界。
pub const MAX_CONTENT_CHARS: usize = 2000;

/// 一条待入库的消息。
#[derive(Debug, Clone)]
pub struct NewMessage {
    /// 消息 id。作为**主键**，天然实现「同一条消息被重复推送只入库一次」，
    /// 与内存里的 dedup 缓存形成双保险。
    pub id: String,
    pub scope: Scope,
    /// 群 OpenID 或用户 OpenID。
    pub target_id: String,
    pub sender_id: Option<String>,
    pub sender_name: Option<String>,
    /// 事件名，保留来源便于排查（GROUP_MESSAGE_CREATE / C2C_MESSAGE_CREATE …）。
    pub event_name: String,
    pub content: String,
    /// Unix 秒。
    pub created_at: i64,
}

impl NewMessage {
    /// 截断过长的正文。绝大多数消息不超限，因此借用而非拷贝。
    pub fn truncated_content(&self) -> Cow<'_, str> {
        if self.content.chars().count() <= MAX_CONTENT_CHARS {
            return Cow::Borrowed(&self.content);
        }
        let mut out: String = self.content.chars().take(MAX_CONTENT_CHARS).collect();
        out.push('…');
        Cow::Owned(out)
    }
}

/// 当前 Unix 秒。
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 把秒数格式化成便于阅读的 UTC 时间（仅用于日志）。
pub fn fmt_unix(ts: i64) -> String {
    // 不引入 chrono：这里只需要「看起来像时间」，用天数换算即可。
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    format!("day+{days} {:02}:{:02}:{:02}Z", secs / 3600, (secs % 3600) / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(content: &str) -> NewMessage {
        NewMessage {
            id: "M1".into(),
            scope: Scope::Group,
            target_id: "G1".into(),
            sender_id: None,
            sender_name: None,
            event_name: "GROUP_MESSAGE_CREATE".into(),
            content: content.into(),
            created_at: 0,
        }
    }

    #[test]
    fn short_content_is_untouched() {
        assert_eq!(msg("你好").truncated_content(), "你好");
    }

    #[test]
    fn long_content_is_truncated_by_chars_not_bytes() {
        let long = "中".repeat(MAX_CONTENT_CHARS + 500);
        let owned = msg(&long);
        let cut = owned.truncated_content();
        assert_eq!(cut.chars().count(), MAX_CONTENT_CHARS + 1, "截断后应恰好是多一个省略号");
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn scope_strings_match_schema_check_constraint() {
        assert_eq!(Scope::Group.as_str(), "group");
        assert_eq!(Scope::C2c.as_str(), "c2c");
    }
}
