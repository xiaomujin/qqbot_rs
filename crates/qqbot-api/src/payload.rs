use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::intents::Intents;
use crate::opcode::OpCode;

/// 网关上下行统一结构：`{ id, op, d, s, t }`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payload<T> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub op: OpCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub d: Option<T>,
    /// 下行序列号；Resume 时需要回传。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub s: Option<i64>,
    /// 事件类型（仅 op = Dispatch 时有值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
}

/// 下行 payload：`d` 保持为未解析的原始 JSON，避免为每个事件都做一次完整反序列化。
pub type RawPayload = Payload<Box<RawValue>>;

impl<T> Payload<T> {
    pub fn new(op: OpCode, d: Option<T>) -> Self {
        Self { id: None, op, d, s: None, t: None }
    }
}

impl RawPayload {
    /// 取 `d` 的原始 JSON 文本。
    pub fn raw_d(&self) -> Option<&str> {
        self.d.as_deref().map(|v| v.get())
    }
}

/// `op = 2` Identify 的 `d`。
#[derive(Clone, Serialize)]
pub struct Identify {
    pub token: String,
    /// 用 `Intents` 而非裸 `u32`：否则它和下面的 `shard` 可以互换，
    /// 手写 payload 时能把分片号填进 intents。线格式不变。
    pub intents: Intents,
    /// `[当前分片序号, 分片总数]`。
    pub shard: [u32; 2],
    pub properties: Properties,
}

/// 手写 `Debug`：`token` 是凭据，不能因为一次 `?payload` 就进日志。
impl std::fmt::Debug for Identify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identify")
            .field("token", &"<redacted>")
            .field("intents", &self.intents)
            .field("shard", &self.shard)
            .field("properties", &self.properties)
            .finish()
    }
}

/// `op = 6` Resume 的 `d`。
#[derive(Clone, Serialize)]
pub struct Resume {
    pub token: String,
    pub session_id: String,
    pub seq: i64,
}

/// 同 `Identify`：`token` 不进 `Debug`。
impl std::fmt::Debug for Resume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resume")
            .field("token", &"<redacted>")
            .field("session_id", &self.session_id)
            .field("seq", &self.seq)
            .finish()
    }
}

/// Identify 的 `properties`（官方说明目前无实际作用，可留空）。
#[derive(Debug, Clone, Serialize)]
pub struct Properties {
    #[serde(rename = "$os")]
    pub os: String,
    #[serde(rename = "$browser")]
    pub browser: String,
    #[serde(rename = "$device")]
    pub device: String,
}

impl Default for Properties {
    fn default() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            browser: "qqbot-rs".to_string(),
            device: "qqbot-rs".to_string(),
        }
    }
}

/// `op = 10` Hello。
#[derive(Debug, Clone, Deserialize)]
pub struct Hello {
    /// 心跳间隔（毫秒）。官方必定下发，这里给默认值以防字段缺失。
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval: u64,
}

const fn default_heartbeat_interval() -> u64 {
    30_000
}

/// 鉴权成功后下发的 READY 事件（`t = "READY"`）。
#[derive(Debug, Clone, Deserialize)]
pub struct Ready {
    #[serde(default)]
    pub version: u32,
    pub session_id: String,
    pub user: ReadyUser,
    #[serde(default)]
    pub shard: [u32; 2],
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadyUser {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub bot: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_raw_payload_keeps_d_unparsed() {
        let text = r#"{"id":"e1","op":0,"s":42,"t":"C2C_MESSAGE_CREATE","d":{"id":"m1","content":"hi"}}"#;
        let p: RawPayload = serde_json::from_str(text).unwrap();
        assert_eq!(p.op, OpCode::Dispatch);
        assert_eq!(p.s, Some(42));
        assert_eq!(p.t.as_deref(), Some("C2C_MESSAGE_CREATE"));
        assert!(p.raw_d().unwrap().contains("\"content\":\"hi\""));
    }

    #[test]
    fn heartbeat_serializes_without_null_fields() {
        let p: Payload<i64> = Payload::new(OpCode::Heartbeat, Some(251));
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(s, r#"{"op":1,"d":251}"#);
    }

    #[test]
    fn unknown_opcode_roundtrips() {
        let p: RawPayload = serde_json::from_str(r#"{"op":99}"#).unwrap();
        assert_eq!(p.op, OpCode::Unknown(99));
    }
}
