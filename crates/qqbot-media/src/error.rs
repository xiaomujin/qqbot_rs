use thiserror::Error;

impl MediaError {
    /// 透传底层 API 错误码的排查建议。
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            MediaError::Api(e) => e.hint(),
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("API 错误: {0}")]
    Api(#[from] qqbot_api::ApiError),

    #[error("HTTP 错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("分片 {index} 上传失败: HTTP {status}")]
    PartFailed { index: u32, status: u16 },

    #[error("预上传/上传响应缺少字段: {0}")]
    MissingField(String),

    #[error("文件为空")]
    Empty,

    #[error("分片规划与文件长度不一致（total={total}, planned={planned}）")]
    PartMismatch { total: usize, planned: usize },
}
