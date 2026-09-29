//! 会话状态、路由、插件与发送层。
//!
//! # Actor 边界（不可随意扩大）
//!
//! - [`SessionRegistry`]：固定 N 个 shard actor，按 `scene:openid` 哈希路由
//! - 其余全部是普通 `async fn` + 责任链，**不要** actor 化
//!
//! 这样做的直接收益：官方协议的三条硬约束（`msg_seq` 递增、被动窗口、主动配额）
//! 全部落在单个 shard 内串行处理，零锁、不可能死锁、天然有序。

pub mod ctx;
pub mod dispatch;
pub mod error;
pub mod message;
pub mod plugin;
pub mod session;

pub use ctx::{Ctx, Services};
pub use dispatch::{DispatchConfig, Dispatcher};
pub use error::CoreError;
pub use message::{Body, SendRequest};
pub use plugin::{FnHandler, Handled, Handler, Matcher, RouteInfo, Router, Rule, Scope};
pub use session::{
    recommended_shards, Quota, QuotaRules, SessionMsg, SessionRegistry, SessionState, WindowRules,
};
