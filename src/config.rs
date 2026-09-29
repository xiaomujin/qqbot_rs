//! 配置加载。
//!
//! 优先级（从高到低）：
//!
//! 1. **环境变量** `QQBOT_*` / `RUST_LOG` —— 部署期覆盖（容器 / CI / systemd）
//! 2. **`config.toml`** —— 本机基线，允许只写一部分
//! 3. **内置默认值**
//!
//! 三条设计约束：
//!
//! - **部分合并**：`config.toml` 里没写的键落到默认值，以后新增键不会让旧文件失效。
//! - **拼错就报错**（`deny_unknown_fields`）：serde 默认会**静默忽略**未知字段，
//!   那恰好是配置文件最坑人的默认行为。
//! - **「缺失」与「非法」分开**：缺失用默认；写了但解析不了则**直接报错**。
//!   静默回退（例如 `QQBOT_RETENTION_DAYS=abc` 悄悄变成 365 天）是最难查的一类问题。
//!
//! 另外，`Config::resolve` 是**纯函数**，环境变量作为参数注入。
//! 这不是洁癖：edition 2024 起 `std::env::set_var` 是 `unsafe`，而本 workspace
//! `unsafe_code = "deny"`，所以直接读 `std::env` 的加载器在本仓库里
//! **根本写不出测试**。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use qqbot_api::{ApiClientConfig, Intents};
use qqbot_render::RenderConfig;
use qqbot_store::{StoreConfig, DAY_SECS};
use serde::Deserialize;

/// 配置文件文件名。
const CONFIG_FILE: &str = "config.toml";
/// 默认数据库位置。
const DEFAULT_DB_PATH: &str = "data/qqbot.db";
/// 默认保留期（天）。
const DEFAULT_RETENTION_DAYS: u64 = 365;

/// 保留期 / 统计窗口的天数上界（100 年）。
///
/// `retention_days * DAY_SECS` 是**未检查**的 u64 乘法：没有上界时，一个荒谬的
/// 配置值会让乘积在 release 下回绕成很小的值，最终把 `cutoff` 算成「刚刚」，
/// 于是 `DELETE FROM messages WHERE created_at < cutoff` 会清空整张表。
const MAX_RETENTION_DAYS: u64 = 36_500;
/// 默认词云统计窗口（天）。
const DEFAULT_WORDCLOUD_WINDOW_DAYS: u64 = 30;
/// 默认日志级别。
const DEFAULT_LOG_LEVEL: &str = "info";
/// 默认单条事件处理并发。
const DEFAULT_DISPATCH_CONCURRENCY: usize = 16;
/// 默认渲染超时（秒）。比 `RenderConfig::default()` 的 3s 宽松一点，线上更稳。
const DEFAULT_RENDER_TIMEOUT_SECS: u64 = 5;

/// 会话分片数上界。它决定 `SessionRegistry` 内部 Vec 的长度，不设上界会被一条
/// 配置直接撑爆内存。
const MAX_SESSION_SHARDS: usize = 1024;
/// 事件处理并发上界。
const MAX_DISPATCH_CONCURRENCY: usize = 4096;
/// 渲染超时上界（秒）。
const MAX_RENDER_TIMEOUT_SECS: u64 = 3600;

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
    /// 日志级别。`RUST_LOG` 未设置时生效。
    pub log_level: String,
    /// 配置来源摘要，供启动日志打印。**不含任何密钥值**，只有来源名与路径。
    pub sources: String,
}

impl Config {
    pub fn api_config(&self) -> ApiClientConfig {
        ApiClientConfig {
            app_id: self.app_id.clone(),
            client_secret: self.client_secret.clone(),
            base_url: self.api_base.clone(),
        }
    }

    /// 按优先级加载配置：环境变量 > `config.toml` > 内置默认值。
    ///
    /// # Errors
    ///
    /// 以下情况返回错误（**不是**回退到默认值）：
    ///
    /// - 凭据在所有来源里都拿不到；
    /// - `QQBOT_CONFIG` 指定的文件不存在或读不出来；
    /// - `config.toml` 解析失败（含键名拼错 —— `deny_unknown_fields`）；
    /// - 某个 `QQBOT_*` 环境变量已设置但解析不了（如 `QQBOT_RETENTION_DAYS=abc`）。
    ///
    /// `config.toml` 不存在**本身不是错误**：环境变量给全了凭据就照常启动。
    pub fn load() -> Result<Self> {
        let env = EnvSource::process();
        let found = discover(&env)?;
        Self::resolve(&env, &found)
    }

    /// 纯函数：把「环境变量 + 配置文件」解析成最终配置。
    ///
    /// 拆出来是为了可测：测试传一个假 `EnvSource` 与内存里的 TOML 文本即可，
    /// 不需要（也无法）改真实进程环境。
    fn resolve(env: &EnvSource, found: &Discovered) -> Result<Self> {
        let file = found.config.as_ref();

        // ---- 凭据：env > config.toml ----
        let app_id = non_empty(env.get("QQBOT_APP_ID"))
            .or_else(|| file.and_then(|f| non_empty(f.app_id.as_deref())));
        let client_secret = non_empty(env.get("QQBOT_APP_SECRET"))
            .or_else(|| file.and_then(|f| non_empty(f.client_secret.as_deref())));

        let (Some(app_id), Some(client_secret)) = (app_id, client_secret) else {
            bail!(
                "未能获取凭据。已尝试：\n  · 环境变量 QQBOT_APP_ID / QQBOT_APP_SECRET\n  · 配置文件 {}\n\n复制模板后填写，或直接设置环境变量：\n  cp config.example.toml config.toml",
                found.path.display()
            );
        };

        // ---- 接口地址 ----
        let api_base = non_empty(env.get("QQBOT_API_BASE"))
            .or_else(|| file.and_then(|f| non_empty(f.api_base.as_deref())))
            .unwrap_or_else(|| qqbot_api::DEFAULT_API_BASE.to_string());
        let gateway_url = non_empty(env.get("QQBOT_GATEWAY_URL"))
            .or_else(|| file.and_then(|f| non_empty(f.gateway_url.as_deref())));

        // ---- 消息持久化 ----
        // db_path 是**三态**：未表态 → 继续往下找；表态为空 → 明确关闭；表态为路径 → 用它。
        // 所以这里不能用「空串视为未设置」的 `get()`，必须用 `get_raw()`。
        let db_path = match env.get_raw("QQBOT_DB_PATH") {
            Some(raw) if raw.trim().is_empty() => None,
            Some(raw) => Some(PathBuf::from(raw.trim())),
            None => match file.and_then(|f| f.db_path.as_deref()) {
                Some(raw) if raw.trim().is_empty() => None,
                Some(raw) => Some(PathBuf::from(raw.trim())),
                None => Some(PathBuf::from(DEFAULT_DB_PATH)),
            },
        };

        // clamp 同时给出下界与上界；上界保证下面 `* DAY_SECS` 不可能溢出。
        let retention_days = env
            .parsed::<u64>("QQBOT_RETENTION_DAYS")?
            .or(file.and_then(|f| f.retention_days))
            .unwrap_or(DEFAULT_RETENTION_DAYS)
            .clamp(1, MAX_RETENTION_DAYS);
        let window_days = env
            .parsed::<u64>("QQBOT_WORDCLOUD_WINDOW_DAYS")?
            .or(file.and_then(|f| f.wordcloud_window_days))
            .unwrap_or(DEFAULT_WORDCLOUD_WINDOW_DAYS)
            .clamp(1, MAX_RETENTION_DAYS);
        let store = db_path.map(|path| StoreConfig {
            path,
            retention: Duration::from_secs(retention_days * DAY_SECS),
            ..StoreConfig::default()
        });

        // ---- 日志：RUST_LOG 是既有的标准覆盖名，不另设 QQBOT_LOG_LEVEL ----
        let log_level = non_empty(env.get("RUST_LOG"))
            .or_else(|| file.and_then(|f| non_empty(f.log_level.as_deref())))
            .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_string());

        // ---- 并发 ----
        let session_shards = env
            .parsed::<usize>("QQBOT_SESSION_SHARDS")?
            .or(file.and_then(|f| f.session_shards))
            .unwrap_or_else(qqbot_core::recommended_shards)
            .clamp(1, MAX_SESSION_SHARDS);
        let dispatch_concurrency = env
            .parsed::<usize>("QQBOT_DISPATCH_CONCURRENCY")?
            .or(file.and_then(|f| f.dispatch_concurrency))
            .unwrap_or(DEFAULT_DISPATCH_CONCURRENCY)
            .clamp(1, MAX_DISPATCH_CONCURRENCY);
        let render_timeout_secs = env
            .parsed::<u64>("QQBOT_RENDER_TIMEOUT_SECS")?
            .or(file.and_then(|f| f.render.timeout_secs))
            .unwrap_or(DEFAULT_RENDER_TIMEOUT_SECS)
            .clamp(1, MAX_RENDER_TIMEOUT_SECS);

        Ok(Self {
            app_id,
            client_secret,
            api_base,
            // 群聊 + 单聊；需要按钮交互时再加上 INTERACTION。
            // intents / shards / event_buffer / dedup_capacity 刻意不做成配置项：
            // 它们是产品决策与内存上界，不是运维旋钮。
            intents: Intents::GROUP_AND_C2C_EVENT | Intents::INTERACTION,
            shards: None,
            session_shards,
            event_buffer: 2048,
            dedup_capacity: 8192,
            dispatch_concurrency,
            render: RenderConfig {
                timeout: Duration::from_secs(render_timeout_secs),
                ..RenderConfig::default()
            },
            gateway_url,
            store,
            wordcloud_window: Duration::from_secs(window_days * DAY_SECS),
            log_level,
            sources: describe_sources(env, found),
        })
    }
}

/// 生成「配置来源」摘要，供启动日志打印。
///
/// 只列**被设置过**的来源，不列默认值 —— 默认值不会让人困惑，
/// 「我改的到底是哪个文件」才会。
fn describe_sources(env: &EnvSource, found: &Discovered) -> String {
    let mut parts = Vec::new();

    // 不必排序：`EnvSource` 用 BTreeMap 存，`keys()` 本来就有序，
    // 这也让 `sources` 在多次运行之间是确定的。
    let env_keys: Vec<&str> = env
        .keys()
        .filter(|k| k.starts_with("QQBOT_") || *k == "RUST_LOG")
        .collect();
    if !env_keys.is_empty() {
        parts.push(format!("环境变量({})", env_keys.join(", ")));
    }
    if found.config.is_some() {
        parts.push(format!("配置文件({})", found.path.display()));
    }
    // 走到这里至少有一个来源：凭据必须来自环境变量或 config.toml，
    // 两者上面都登记过；都没有的话 resolve 早就报错返回了。
    parts.join(" + ")
}

/// 环境变量来源。
///
/// 单独抽出来是为了**可测试**：edition 2024 起 `env::set_var` 是 unsafe，
/// 而 workspace 禁用了 `unsafe_code`，所以测试无法改真实进程环境。
#[derive(Debug, Default, Clone)]
struct EnvSource {
    vars: BTreeMap<String, String>,
}

impl EnvSource {
    /// 读取真实进程环境。
    fn process() -> Self {
        Self {
            vars: std::env::vars().collect(),
        }
    }

    /// 构造一个假的环境（测试用）。
    #[cfg(test)]
    fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self {
            vars: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    fn keys(&self) -> impl Iterator<Item = &str> {
        self.vars.keys().map(String::as_str)
    }

    /// 已设置**且非空** → `Some`。空串一律视为「未设置」。
    fn get(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
    }

    /// 只要「设置过」就是 `Some`（可能是空串）。
    ///
    /// 用于空串**有特殊含义**的键（`QQBOT_DB_PATH=""` 表示关闭持久化）。
    fn get_raw(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// 解析成 `T`：缺失 → `None`；存在但非法 → **报错**（不静默回退）。
    fn parsed<T>(&self, key: &str) -> Result<Option<T>>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        match self.get(key) {
            None => Ok(None),
            Some(raw) => raw
                .trim()
                .parse::<T>()
                .map(Some)
                .map_err(|err| anyhow!("环境变量 {key} 的值 {raw:?} 无法解析：{err}")),
        }
    }
}

/// `config.toml` 的结构。
///
/// **全部字段可选**：没写的键落到默认值，所以升级新增键不会让旧配置文件失效。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    app_id: Option<String>,
    client_secret: Option<String>,
    api_base: Option<String>,
    gateway_url: Option<String>,
    /// 显式 `""` 表示**关闭持久化**（与 `QQBOT_DB_PATH=""` 同义）。
    db_path: Option<String>,
    retention_days: Option<u64>,
    wordcloud_window_days: Option<u64>,
    log_level: Option<String>,
    session_shards: Option<usize>,
    dispatch_concurrency: Option<usize>,
    #[serde(default)]
    render: RenderSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderSection {
    timeout_secs: Option<u64>,
}

/// 配置文件发现结果。
struct Discovered {
    /// 查找过的路径。**即使文件不存在也填**，用于报错时告诉用户该往哪写。
    path: PathBuf,
    config: Option<FileConfig>,
}

/// 定位并解析 `config.toml`。
///
/// 查找顺序：`QQBOT_CONFIG` 指定 → 当前目录及其父目录。
fn discover(env: &EnvSource) -> Result<Discovered> {
    // 显式指定路径时**必须存在**：静默回退到默认路径会让「配置没生效」
    // 变成一件完全无法察觉的事。
    if let Some(raw) = env.get("QQBOT_CONFIG") {
        let path = PathBuf::from(raw);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("读取 QQBOT_CONFIG 指定的 {} 失败", path.display()))?;
        let config = parse_file(&path, &text)?;
        return Ok(Discovered {
            path,
            config: Some(config),
        });
    }

    // 向上找，这样在子目录里 `cargo run` 也能找到仓库根的配置。
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dir: Option<&Path> = Some(&cwd);
    while let Some(d) = dir {
        let candidate = d.join(CONFIG_FILE);
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate)
                .with_context(|| format!("读取 {} 失败", candidate.display()))?;
            let config = parse_file(&candidate, &text)?;
            return Ok(Discovered {
                path: candidate,
                config: Some(config),
            });
        }
        dir = d.parent();
    }

    Ok(Discovered {
        path: cwd.join(CONFIG_FILE),
        config: None,
    })
}

fn parse_file(path: &Path, text: &str) -> Result<FileConfig> {
    toml::from_str(text).with_context(|| format!("解析 {} 失败", path.display()))
}

/// 空串与纯空白视为「未设置」，避免 `app_id = ""` 这种占位被当成真凭据。
///
/// 收 `Option<&str>` 而不是 `Option<String>`：调用点一边是环境变量的 `&str`，
/// 一边是配置文件里 `Option<String>` 的字段，收借用能让两边都不分配。
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 入库的配置模板必须能被解析 —— 键名写错、类型写错、或出现代码里
    /// 不存在的键（`deny_unknown_fields`），这个测试都会红。
    const EXAMPLE: &str = include_str!("../config.example.toml");

    fn env(pairs: &[(&str, &str)]) -> EnvSource {
        EnvSource::from_pairs(pairs)
    }

    fn found(toml_text: Option<&str>) -> Discovered {
        Discovered {
            path: PathBuf::from("config.toml"),
            config: toml_text.map(|t| toml::from_str(t).expect("测试用 TOML 必须合法")),
        }
    }

    fn resolve(pairs: &[(&str, &str)], toml_text: Option<&str>) -> Result<Config> {
        Config::resolve(&env(pairs), &found(toml_text))
    }

    /// 带凭据的最小环境，方便测其他键。
    fn creds<'a>(pairs: &'a [(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let mut v = vec![("QQBOT_APP_ID", "id"), ("QQBOT_APP_SECRET", "sec")];
        v.extend_from_slice(pairs);
        v
    }

    #[test]
    fn example_template_parses() {
        let parsed: FileConfig = toml::from_str(EXAMPLE).expect("config.example.toml 必须可解析");
        // 模板里的凭据是空占位，必须被当成「未设置」。
        assert_eq!(non_empty(parsed.app_id.as_deref()), None);
        assert_eq!(non_empty(parsed.client_secret.as_deref()), None);
    }

    #[test]
    fn all_keys_are_accepted() {
        // 这个字面量同时是「schema 清单」：改字段名会让它失败。
        let text = r#"
            app_id = "a"
            client_secret = "s"
            api_base = "https://example.invalid"
            gateway_url = "wss://example.invalid"
            db_path = "x.db"
            retention_days = 1
            wordcloud_window_days = 1
            log_level = "warn"
            session_shards = 1
            dispatch_concurrency = 1
            [render]
            timeout_secs = 1
        "#;
        let parsed: FileConfig = toml::from_str(text).expect("所有已暴露的键都必须被接受");
        assert_eq!(parsed.retention_days, Some(1));
        assert_eq!(parsed.render.timeout_secs, Some(1));
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = toml::from_str::<FileConfig>("nope = 1").unwrap_err();
        assert!(err.to_string().contains("nope"), "报错应点名未知的键：{err}");
    }

    #[test]
    fn env_beats_file() {
        let cfg = resolve(&creds(&[("QQBOT_RETENTION_DAYS", "10")]), Some("retention_days = 20")).unwrap();
        assert_eq!(cfg.store.unwrap().retention, Duration::from_secs(10 * DAY_SECS));
    }

    #[test]
    fn file_beats_default() {
        let cfg = resolve(&creds(&[]), Some("retention_days = 20")).unwrap();
        assert_eq!(cfg.store.unwrap().retention, Duration::from_secs(20 * DAY_SECS));
    }

    #[test]
    fn defaults_when_nothing_set() {
        let cfg = resolve(&creds(&[]), None).unwrap();
        let store = cfg.store.unwrap();
        assert_eq!(store.path, PathBuf::from(DEFAULT_DB_PATH));
        assert_eq!(store.retention, Duration::from_secs(DEFAULT_RETENTION_DAYS * DAY_SECS));
        assert_eq!(cfg.log_level, DEFAULT_LOG_LEVEL);
        assert_eq!(cfg.dispatch_concurrency, DEFAULT_DISPATCH_CONCURRENCY);
        assert_eq!(cfg.render.timeout, Duration::from_secs(DEFAULT_RENDER_TIMEOUT_SECS));
        // 只有凭据是环境变量给的，所以来源摘要里只应出现环境变量。
        assert_eq!(cfg.sources, "环境变量(QQBOT_APP_ID, QQBOT_APP_SECRET)");
    }

    #[test]
    fn malformed_env_is_an_error_not_a_silent_default() {
        let err = resolve(&creds(&[("QQBOT_RETENTION_DAYS", "abc")]), None).unwrap_err();
        assert!(err.to_string().contains("QQBOT_RETENTION_DAYS"), "{err}");
    }

    #[test]
    fn empty_env_var_is_treated_as_unset() {
        // 空串不是「非法值」，而是「没设置」：应当让 config.toml 接管。
        let cfg = resolve(
            &[("QQBOT_APP_ID", ""), ("QQBOT_APP_SECRET", "")],
            Some("app_id = \"file-id\"\nclient_secret = \"file-sec\""),
        )
        .unwrap();
        assert_eq!(cfg.app_id, "file-id");
        assert_eq!(cfg.client_secret, "file-sec");
    }

    #[test]
    fn empty_db_path_disables_persistence() {
        let cfg = resolve(&creds(&[("QQBOT_DB_PATH", "")]), Some("db_path = \"from-file.db\"")).unwrap();
        assert!(cfg.store.is_none(), "环境变量置空应关闭持久化，并压过文件里的路径");
    }

    #[test]
    fn empty_db_path_in_file_disables_persistence() {
        let cfg = resolve(&creds(&[]), Some("db_path = \"\"")).unwrap();
        assert!(cfg.store.is_none());
    }

    #[test]
    fn retention_is_clamped() {
        let cfg = resolve(&creds(&[("QQBOT_RETENTION_DAYS", "99999999")]), None).unwrap();
        assert_eq!(
            cfg.store.unwrap().retention,
            Duration::from_secs(MAX_RETENTION_DAYS * DAY_SECS)
        );
    }

    #[test]
    fn session_shards_is_clamped() {
        let cfg = resolve(&creds(&[("QQBOT_SESSION_SHARDS", "99999999")]), None).unwrap();
        assert_eq!(cfg.session_shards, MAX_SESSION_SHARDS);
    }

    #[test]
    fn missing_credentials_points_at_the_template() {
        let err = resolve(&[], None).unwrap_err().to_string();
        assert!(err.contains("config.example.toml"), "报错要给出下一步动作：{err}");
        assert!(err.contains("QQBOT_APP_ID"), "{err}");
    }

    #[test]
    fn sources_do_not_leak_secrets() {
        let cfg = resolve(
            &creds(&[("QQBOT_DB_PATH", "x.db")]),
            Some("retention_days = 7"),
        )
        .unwrap();
        assert!(!cfg.sources.contains("sec"), "{}", cfg.sources);
        assert!(cfg.sources.contains("QQBOT_APP_ID"), "{}", cfg.sources);
        assert!(cfg.sources.contains("配置文件"), "{}", cfg.sources);
    }

    #[test]
    fn config_path_can_come_from_env() {
        let src = env(&[("QQBOT_CONFIG", "C:\\tmp\\qqbot.toml")]);
        assert_eq!(src.get("QQBOT_CONFIG"), Some("C:\\tmp\\qqbot.toml"));
        // 空串视为未设置，会退回默认查找路径。
        assert_eq!(env(&[("QQBOT_CONFIG", "  ")]).get("QQBOT_CONFIG"), None);
    }
}
