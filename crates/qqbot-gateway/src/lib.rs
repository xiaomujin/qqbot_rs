//! WebSocket 网关（`GatewayActor`）。
//!
//! 每个分片一个 actor，独占维护 `session_id` / `last_seq` / 心跳 / 重连状态机。
//! 事件通过有界 `mpsc` 转发给上层，**绝不阻塞 WebSocket 读循环**（否则心跳会饿死导致掉线）。

pub mod actor;
pub mod backoff;

pub use actor::{spawn_gateway, GatewayCmd, GatewayConfig, GatewayHandle};
pub use backoff::Backoff;

use thiserror::Error;

/// 网关层错误。
#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("API 错误: {0}")]
    Api(#[from] qqbot_api::ApiError),

    #[error("WebSocket 错误: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("网关在 Hello 之前关闭了连接")]
    ClosedBeforeHello,

    #[error("事件通道已关闭")]
    EventChannelClosed,
}
