use thiserror::Error;

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("模板错误: {0}")]
    Template(#[from] minijinja::Error),

    #[error("SVG 解析失败: {0}")]
    Svg(String),

    #[error("画布尺寸非法或超出上限: {width}x{height}")]
    BadSize { width: u32, height: u32 },

    #[error("PNG 编码失败: {0}")]
    Encode(String),

    #[error("渲染超时")]
    Timeout,

    #[error("渲染队列已关闭")]
    Closed,

    #[error("渲染任务被取消")]
    Canceled,
}
