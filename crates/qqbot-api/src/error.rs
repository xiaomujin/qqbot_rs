use thiserror::Error;

/// 协议层统一错误。
#[derive(Debug, Error)]
pub enum ApiError {
    #[error("HTTP 传输错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON 解析错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("协议错误: {0}")]
    Protocol(String),

    /// 官方业务错误。注意：这类错误的 HTTP 状态码通常仍是 200。
    #[error("业务错误 {code}: {message} (trace_id={trace_id:?})")]
    Business {
        code: i64,
        message: String,
        trace_id: Option<String>,
    },

    #[error("鉴权失败 (HTTP 401)")]
    Unauthorized,

    #[error("触发频率限制 (HTTP 429)")]
    RateLimited,

    #[error("非预期状态码 {status}: {body}")]
    Status { status: u16, body: String },

    #[error("通道已关闭")]
    Shutdown,
}

impl ApiError {
    /// 是否值得重试。
    ///
    /// 官方文档明确：`11281` / `11252` 属系统错误，**最多只能重试一次**。
    pub fn is_retryable(&self) -> bool {
        match self {
            ApiError::Http(e) => e.is_timeout() || e.is_connect(),
            ApiError::RateLimited => true,
            ApiError::Status { status, .. } => *status >= 500,
            ApiError::Business { code, .. } => matches!(*code, 11281 | 11252),
            _ => false,
        }
    }

    /// 官方系统错误：只允许重试一次。
    pub fn is_retry_once(&self) -> bool {
        matches!(self, ApiError::Business { code, .. } if matches!(*code, 11281 | 11252))
    }

    /// 被动回复窗口已过期（`40034005`），应改走主动消息。
    pub fn is_passive_expired(&self) -> bool {
        matches!(self, ApiError::Business { code, .. } if *code == 40034005)
    }

    /// 面向运维的排查建议（依据官方错误码表）。
    ///
    /// 真机联调时日志里能直接看到「该去开放平台申请哪个权限」，
    /// 而不是只有一个裸错误码。
    pub fn hint(&self) -> Option<&'static str> {
        let code = self.business_code()?;
        Some(match code {
            10001 => "账号异常，检查机器人状态",
            10004 => "机器人不存在，确认 AppID 是否正确",
            11251 | 11261 => "AppID 错误或 token 与 AppID 不匹配",
            11252 | 11281 => "平台系统错误，最多重试一次",
            11253 => "机器人未获得该接口权限 —— 请到 QQ 开放平台管理端申请「富媒体/上传」等对应权限",
            11254 => "该接口已被封禁，需联系平台",
            11282 => "需要管理员权限，应提示用户重新授权",
            40034005 => "被动回复 msg_id 已过期，会自动改走主动消息",
            850018 => "群被禁言，或机器人被禁言",
            850019 => "不支持的文件格式，检查 file_type",
            850026 => "平台下载原始文件失败，检查 URL 可访问性",
            850027 => "平台发送数据超时，稍后重试",
            850031 => "文件超过大小限制（图片软限 20MB / 硬限 200MB）",
            40093001 => "文件上传失败，可重试",
            40093002 => "超过当天发送文件容量上限",
            _ => return None,
        })
    }

    pub fn business_code(&self) -> Option<i64> {
        match self {
            ApiError::Business { code, .. } => Some(*code),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn biz(code: i64) -> ApiError {
        ApiError::Business { code, message: String::new(), trace_id: None }
    }

    #[test]
    fn known_codes_have_actionable_hints() {
        // 真机联调最可能撞上的两个：权限未申请、被动窗口过期
        assert!(biz(11253).hint().unwrap().contains("开放平台"));
        assert!(biz(40034005).hint().unwrap().contains("主动消息"));
        assert!(biz(850031).hint().unwrap().contains("大小限制"));
    }

    #[test]
    fn unknown_code_has_no_hint() {
        assert!(biz(999999).hint().is_none());
        assert!(ApiError::RateLimited.hint().is_none());
    }

    #[test]
    fn retry_classification_is_not_confused_by_hints() {
        // 11253 是权限问题，不该被判为可重试
        assert!(!biz(11253).is_retryable());
        assert!(biz(11281).is_retryable());
    }
}
