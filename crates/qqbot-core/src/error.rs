use thiserror::Error;

impl CoreError {
    /// 透传底层 API 错误码的排查建议。
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            CoreError::Api(e) => e.hint(),
            CoreError::Media(e) => e.hint(),
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("API 错误: {0}")]
    Api(#[from] qqbot_api::ApiError),

    #[error("媒体错误: {0}")]
    Media(#[from] qqbot_media::MediaError),

    #[error("渲染错误: {0}")]
    Render(#[from] qqbot_render::RenderError),

    #[error("主动消息配额已用尽（单关系 20/min、1000/天）")]
    QuotaExceeded,

    #[error("会话 actor 已关闭")]
    Closed,

    #[error("发送结果通道被取消")]
    Canceled,
}
