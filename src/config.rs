//! 配置加载。
//!
//! 优先级：环境变量 > bot.txt > config.toml。
//!
//! 密钥**只**从外部读取，不写进代码；`bot.txt` 已加入 .gitignore。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use qqbot_api::{ApiClientConfig, Intents};
use qqbot_render::RenderConfig;
use qqbot_store::{StoreConfig, DAY_SECS};

/// 默认数据库位置。
const DEFAULT_DB_PATH: &str = "data/qqbot.db";
/// 默认保留期（天）。
const DEFAULT_RETENTION_DAYS: u64 = 365;

/// 保留期 / 统计窗口的天数上界（100 年）。
///
/// `retention_days * DAY_SECS` 是**未检查**的 u64 乘法：没有上界时，一个荒谬的
/// 环境变量会让乘积在 release 下回绕成很小的值，最终把 `cutoff` 算成「刚刚」，
/// 于是 `DELETE FROM messages WHERE created_at < cutoff` 会清空整张表。
const MAX_RETENTION_DAYS: u64 = 36_500;
/// 默认词云统计窗口（天）。
const DEFAULT_WORDCLOUD_WINDOW_DAYS: u64 = 30;

#[derive(Debug, Clone)]
pub struct Config {
    pub app_id: String,
    pub client_secret: String,
    pub api_base: String,
    pub intents: Intents,
    /// `None` 表示采用 `/gateway/bot` 返回的建议分片数。
    pub shards: Option<u32>,
    pub session_shards: usize,
    pub event_buffer: usize,
    pub dedup_capacity: u64,
    pub dispatch_concurrency: usize,
    pub render: RenderConfig,
    /// 覆盖网关地址（`QQBOT_GATEWAY_URL`）；`None` 表示走 `/gateway/bot` 探测。
    pub gateway_url: Option<String>,
    /// 消息持久化配置。`None` 表示不启用（词云退化为内存语料）。
    pub store: Option<StoreConfig>,
    /// 词云统计窗口：只看最近这段时间的消息。
    pub wordcloud_window: Duration,
}

impl Config {
    pub fn api_config(&self) -> ApiClientConfig {
        ApiClientConfig {
            app_id: self.app_id.clone(),
            client_secret: self.client_secret.clone(),
            base_url: self.api_base.clone(),
        }
    }

    /// 按优先级加载配置。
    pub fn load() -> Result<Self> {
        let (app_id, client_secret) = match (
            std::env::var("QQBOT_APP_ID").ok().filter(|s| !s.is_empty()),
            std::env::var("QQBOT_APP_SECRET").ok().filter(|s| !s.is_empty()),
        ) {
            (Some(a), Some(s)) => (a, s),
            _ => load_from_bot_txt().context(
                "未能获取凭据：请设置 QQBOT_APP_ID / QQBOT_APP_SECRET，或在 bot.txt 中填写 AppID 与 AppSecret",
            )?,
        };

        let api_base = std::env::var("QQBOT_API_BASE")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| qqbot_api::DEFAULT_API_BASE.to_string());

        // 群聊 + 单聊；需要按钮交互时再加上 INTERACTION。
        let intents = Intents::GROUP_AND_C2C_EVENT | Intents::INTERACTION;

        // 消息持久化：QQBOT_DB_PATH 显式置空可关闭（此时词云只用内存语料）。
        let db_path = match std::env::var("QQBOT_DB_PATH") {
            Ok(v) if v.trim().is_empty() => None,
            Ok(v) => Some(PathBuf::from(v.trim())),
            Err(_) => Some(PathBuf::from(DEFAULT_DB_PATH)),
        };
        // clamp 同时给出下界与上界；上界保证下面 `* DAY_SECS` 不可能溢出。
        let retention_days =
            env_u64("QQBOT_RETENTION_DAYS", DEFAULT_RETENTION_DAYS).clamp(1, MAX_RETENTION_DAYS);
        let window_days = env_u64("QQBOT_WORDCLOUD_WINDOW_DAYS", DEFAULT_WORDCLOUD_WINDOW_DAYS)
            .clamp(1, MAX_RETENTION_DAYS);
        let store = db_path.map(|path| StoreConfig {
            path,
            retention: Duration::from_secs(retention_days * DAY_SECS),
            ..StoreConfig::default()
        });

        Ok(Self {
            app_id,
            client_secret,
            api_base,
            intents,
            shards: None,
            session_shards: qqbot_core::recommended_shards(),
            event_buffer: 2048,
            dedup_capacity: 8192,
            dispatch_concurrency: 16,
            render: RenderConfig {
                timeout: Duration::from_secs(5),
                ..RenderConfig::default()
            },
            gateway_url: std::env::var("QQBOT_GATEWAY_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            store,
            wordcloud_window: Duration::from_secs(window_days * DAY_SECS),
        })
    }
}

/// 读取正整数环境变量，缺省或非法时回退。
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// 解析 bot.txt。官方文档用的是全角冒号，这里统一成半角再切分。
fn load_from_bot_txt() -> Result<(String, String)> {
    let path = find_bot_txt().ok_or_else(|| anyhow!("未找到 bot.txt"))?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取 {} 失败", path.display()))?;

    let mut app_id = None;
    let mut secret = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let normalized = line.replace('：', ":");
        let Some((k, v)) = normalized.split_once(':') else { continue };
        let key = k.trim().to_ascii_lowercase();
        let value = v.trim().trim_matches('"').trim_matches('\'').to_string();
        if value.is_empty() {
            continue;
        }
        if key.contains("appid") || key.contains("app_id") {
            app_id = Some(value);
        } else if key.contains("secret") {
            secret = Some(value);
        }
    }

    match (app_id, secret) {
        (Some(a), Some(s)) => {
            tracing::debug!(path = %path.display(), "已从 bot.txt 读取凭据");
            Ok((a, s))
        }
        _ => Err(anyhow!("{} 中缺少 AppID 或 AppSecret", path.display())),
    }
}

fn find_bot_txt() -> Option<PathBuf> {
    let mut dir: Option<&Path> = Some(Path::new("."));
    while let Some(d) = dir {
        let candidate = d.join("bot.txt");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}
