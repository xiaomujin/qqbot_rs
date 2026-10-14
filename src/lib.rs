//! QQ 机器人（官方 API v2）—— 库入口。
//!
//! 只有两个模块：
//! - [`assembly`]：把 [`Config`] 装配成可运行的服务栈；
//! - [`config`]：配置加载（环境变量 > `config.toml` > 内置默认值）。
//!
//! 之所以要有这个 lib target：集成测试（`tests/end_to_end.rs`）必须能调用
//! **产品那条装配路径**。装配只写在 bin 里的话集成测试根本引用不到，
//! 只能自己再抄一份 —— 之前就是这么漂移的：渲染超时、会话分片数、去重容量
//! 各写各的，而 `spawn_sweeper`（保留期清理）一次都没被走到。

pub mod assembly;
pub mod config;

pub use assembly::{assemble, assemble_with, App};
pub use config::Config;
