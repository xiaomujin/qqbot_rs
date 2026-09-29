use serde::{Deserialize, Serialize};

/// 网关 opcode。
///
/// 取值见官方文档「事件订阅与通知 → OpCode」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "u8", from = "u8")]
pub enum OpCode {
    /// 0 — 服务端消息推送。
    Dispatch,
    /// 1 — 心跳。
    Heartbeat,
    /// 2 — 客户端鉴权。
    Identify,
    /// 6 — 恢复连接。
    Resume,
    /// 7 — 服务端要求重连。
    Reconnect,
    /// 9 — identify / resume 参数有误。
    InvalidSession,
    /// 10 — 连接建立后的第一条消息。
    Hello,
    /// 11 — 心跳成功。
    HeartbeatAck,
    /// 12 — 仅 webhook 模式：HTTP 回调 ACK。
    HttpCallbackAck,
    /// 13 — 仅 webhook 模式：回调地址验证。
    CallbackVerify,
    /// 未知 opcode（前向兼容，不 panic）。
    Unknown(u8),
}

impl OpCode {
    pub const fn as_u8(self) -> u8 {
        match self {
            OpCode::Dispatch => 0,
            OpCode::Heartbeat => 1,
            OpCode::Identify => 2,
            OpCode::Resume => 6,
            OpCode::Reconnect => 7,
            OpCode::InvalidSession => 9,
            OpCode::Hello => 10,
            OpCode::HeartbeatAck => 11,
            OpCode::HttpCallbackAck => 12,
            OpCode::CallbackVerify => 13,
            OpCode::Unknown(v) => v,
        }
    }
}

impl From<OpCode> for u8 {
    fn from(op: OpCode) -> u8 {
        op.as_u8()
    }
}

impl From<u8> for OpCode {
    fn from(v: u8) -> Self {
        match v {
            0 => OpCode::Dispatch,
            1 => OpCode::Heartbeat,
            2 => OpCode::Identify,
            6 => OpCode::Resume,
            7 => OpCode::Reconnect,
            9 => OpCode::InvalidSession,
            10 => OpCode::Hello,
            11 => OpCode::HeartbeatAck,
            12 => OpCode::HttpCallbackAck,
            13 => OpCode::CallbackVerify,
            other => OpCode::Unknown(other),
        }
    }
}
