//! 塔科夫相关命令。
//!
//! 从 cq-bot 的 `TkfPlugin` / `BulletPlugin` 迁移而来。源项目把它们拆在两个
//! 文件里（时间/子弹在 `BulletPlugin`，BOSS/任务在 `TkfPlugin`），这里合并，
//! 免得每加一个命令就多一个只含几十行的文件。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use qqbot_store::now_unix;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::timewin::{format_datetime, MOSCOW_OFFSET, SHANGHAI_OFFSET};

const DAY: i64 = 86_400;

/// 游戏内时间流速相对现实的倍数。
const GAME_SPEED: i64 = 7;
/// 两个可选出发时间相差的小时数。
const CLOCK_SPLIT_SECS: i64 = 12 * 3600;


/// 塔科夫插件配置。
#[derive(Debug, Clone)]
pub struct TarkovConfig {
    /// GraphQL 端点。
    ///
    /// ⚠️ 目前**没有用到**：它的后端自 2026-09 起对所有查询返回 422，
    /// BOSS 刷新率已改走 `maps_url` 的静态 JSON。保留字段是为了它恢复后
    /// 能拿回中文名 —— 静态 JSON 的名字是 slug。
    pub graphql_url: String,
    /// BOSS 刷新率的静态 JSON。
    pub maps_url: String,
    /// BOSS 刷新率缓存时长。
    ///
    /// 刷新率是**静态数据**，缓存纯粹是为了防刷 —— 源项目也是 10 分钟。
    pub boss_cache: Duration,
    /// 服务器状态接口前缀（不含 `/api/...`）。
    pub status_base: String,
    /// 服务器状态缓存时长。源项目是 20 分钟。
    pub status_cache: Duration,
}

impl Default for TarkovConfig {
    fn default() -> Self {
        Self {
            graphql_url: "https://api.tarkov.dev/graphql".into(),
            maps_url: "https://json.tarkov.dev/regular/maps".into(),
            boss_cache: Duration::from_secs(600),
            status_base: "https://status.escapefromtarkov.com".into(),
            status_cache: Duration::from_secs(1200),
        }
    }
}

// ---- 游戏内时间 ----

/// 塔科夫游戏内时刻（一天中的秒数），返回 `(左, 右)`。
///
/// 游戏内时间流速是现实的 **7 倍**（`now × 7`），再按**莫斯科时区**取时刻；
/// 两个值相差 12 小时，对应游戏里可选的两个出发时间。
///
/// 源实现乘的是 `epochMilli`，这里乘秒 —— 两者对「一天中的时刻」完全等价，
/// 而且少了 1000 倍，不至于把中间结果推到 i64 边界附近。
pub fn tarkov_clock(now_secs: i64) -> (i64, i64) {
    let game = now_secs.saturating_mul(GAME_SPEED) + MOSCOW_OFFSET;
    ((game - CLOCK_SPLIT_SECS).rem_euclid(DAY), game.rem_euclid(DAY))
}

/// 格式化后的 `(左, 右)`，形如 `03:00:00`。
pub fn tarkov_time(now_secs: i64) -> (String, String) {
    let (left, right) = tarkov_clock(now_secs);
    (format_clock(left), format_clock(right))
}

fn format_clock(secs: i64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

// ---- BOSS 刷新率 ----

#[derive(Debug, Deserialize)]
pub struct MapEntry {
    /// 可读 slug（`factory` / `customs`）。静态 JSON 的 `name` 是翻译键。
    #[serde(default, rename = "normalizedName")]
    pub normalized_name: String,
    #[serde(default)]
    pub bosses: Vec<BossEntry>,
}

#[derive(Debug, Deserialize)]
pub struct BossEntry {
    /// 形如 `bossTagilla` —— 静态 JSON 里它已经是可读的标识，不是翻译键。
    #[serde(default)]
    pub mob: String,
    /// 0.0 ~ 1.0 的比例，不是百分数。
    #[serde(default, rename = "spawnChance")]
    pub spawn_chance: f64,
}

#[derive(Debug, Deserialize)]
struct MapsResponse {
    #[serde(default)]
    data: Option<MapsData>,
}

#[derive(Debug, Deserialize)]
struct MapsData {
    #[serde(default)]
    maps: HashMap<String, MapEntry>,
}

/// 把静态 JSON 解析成按名称排序的地图列表。
///
/// 上游的 `maps` 是**按 id 键控的对象**，迭代顺序不保证；
/// 排序让同一份数据每次输出的排列一致。
pub fn parse_maps(body: &str) -> Result<Vec<MapEntry>, String> {
    let parsed: MapsResponse =
        serde_json::from_str(body).map_err(|err| format!("解析地图数据失败：{err}"))?;
    let mut maps: Vec<MapEntry> = parsed
        .data
        .ok_or("地图响应缺少 data")?
        .maps
        .into_values()
        .filter(|m| !m.normalized_name.is_empty())
        .collect();
    maps.sort_by(|a, b| a.normalized_name.cmp(&b.normalized_name));
    Ok(maps)
}

/// 整理成「地图 → BOSS 平均刷新率」的文本，末尾附查询时间。
///
/// 同一张图上同一个 BOSS 可能有多个刷新点，取**平均值**（与源项目一致）。
///
/// 地图按 `normalizedName` 排序后输出。源项目用的是 Java `HashMap`，
/// 迭代顺序随机，同一份数据每次刷新出来的排列都不一样；保序是刻意的改进。
pub fn format_boss_chance(maps: &[MapEntry], now: i64) -> String {
    let mut out = String::new();
    for map in maps {
        // 保序聚合：同一个 BOSS 的多个刷新点收集到一起。
        let mut per_boss: Vec<(&str, Vec<f64>)> = Vec::new();
        for entry in &map.bosses {
            let name = entry.mob.trim();
            if name.is_empty() {
                continue;
            }
            match per_boss.iter_mut().find(|(n, _)| *n == name) {
                Some((_, chances)) => chances.push(entry.spawn_chance),
                None => per_boss.push((name, vec![entry.spawn_chance])),
            }
        }
        if per_boss.is_empty() {
            // 源项目会把没有 BOSS 的地图也打出一行空标题，这里跳过。
            continue;
        }
        out.push_str(&map.normalized_name);
        out.push('\n');
        for (name, chances) in per_boss {
            let avg = chances.iter().sum::<f64>() / chances.len() as f64;
            let _ = writeln!(out, "    {name}: {:.0}%", avg * 100.0);
        }
    }
    out.push_str(&format_datetime(now, SHANGHAI_OFFSET));
    out
}

// ---- 服务器状态 ----

#[derive(Debug, Deserialize)]
pub struct ServiceStatus {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: i64,
}

#[derive(Debug, Deserialize)]
pub struct GlobalStatus {
    #[serde(default)]
    pub status: i64,
    #[serde(default)]
    pub message: String,
}

/// 服务名中文化。未知服务原样显示 —— 官方随时可能加服务，
/// 用 `_ => ""` 把它们吞掉会让状态页看起来少了几行。
fn cn_service_name(name: &str) -> String {
    let label = match name {
        "Website" => "游戏官网",
        "Forum" => "官方论坛",
        "Authentication" => "身份认证",
        "Launcher" => "启动器",
        "Group lobby" => "组队功能",
        "Trading" => "交易功能",
        "Matchmaking" => "战局匹配",
        "Friends and msg" => "好友消息",
        "Inventory operations" => "库存操作",
        other => other,
    };
    format!("{label}：")
}

fn cn_status(status: i64) -> &'static str {
    match status {
        0 => "🟢服务正常",
        1 => "⚙️正在更新",
        2 => "🟡部分故障",
        3 => "🔴服务不可用",
        _ => "⚪未知",
    }
}

/// 整理成状态速报文本。
pub fn format_server_status(
    services: &[ServiceStatus],
    global: &GlobalStatus,
    now: i64,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", format_datetime(now, SHANGHAI_OFFSET));
    out.push_str("服务器状态速报：\n");
    for service in services {
        let _ = writeln!(out, "{}{}", cn_service_name(&service.name), cn_status(service.status));
    }
    let _ = write!(out, "\n总体状态：{}", cn_status(global.status));
    if !global.message.is_empty() {
        let _ = write!(out, "\n信息：{}", global.message);
    }
    out
}

/// 是否是服务器状态查询。
///
/// 源项目正则是 `^(?i)((塔科夫|tkf)?服务器(状态)?)$` —— 前缀可选、后缀可选，
/// 一共 6 种写法，但**必须是整串**。
pub fn is_server_query(content: &str) -> bool {
    let lower = content.trim().to_lowercase();
    let rest = lower
        .strip_prefix("塔科夫")
        .or_else(|| lower.strip_prefix("tkf"))
        .unwrap_or(lower.as_str());
    matches!(rest, "服务器" | "服务器状态")
}

/// 是否是 BOSS 刷新率查询。
///
/// 源项目的正则是 `^(?i)(boss(刷|概))`：`boss刷` / `boss概` 开头、后面随意。
/// 保留这个宽松度（用户会写 `boss刷新率`、`boss概率`），
/// 但前缀本身足够特别，日常聊天里撞上的概率很低。
pub fn is_boss_query(content: &str) -> bool {
    let lower = content.trim().to_lowercase();
    lower.starts_with("boss刷") || lower.starts_with("boss概")
}

// ---- 插件 ----

/// 塔科夫命令插件。
///
/// `Clone` 是必需的：同一个实例要挂到「塔科夫时间」与「boss刷」两条路由上，
/// 缓存必须是 `Arc` 共享的，否则两条路由各缓存一份。
#[derive(Clone)]
pub struct TarkovPlugin {
    config: TarkovConfig,
    http: reqwest::Client,
    /// 只缓存一份：刷新率是静态数据，与用户、群都无关。
    boss_cache: Arc<Mutex<Option<(Instant, String)>>>,
    /// 服务器状态同理，且源项目也是 20 分钟一刷。
    status_cache: Arc<Mutex<Option<(Instant, String)>>>,
}

impl TarkovPlugin {
    pub fn new(config: TarkovConfig, http: reqwest::Client) -> Self {
        Self {
            config,
            http,
            boss_cache: Arc::new(Mutex::new(None)),
            status_cache: Arc::new(Mutex::new(None)),
        }
    }

    /// BOSS 刷新率文本，命中缓存则不打上游。
    async fn boss_chance_text(&self) -> String {
        // 锁跨越 await：并发触发时只有一个请求打到上游，其余等锁后直接命中缓存。
        let mut guard = self.boss_cache.lock().await;
        if let Some((at, text)) = guard.as_ref()
            && at.elapsed() < self.config.boss_cache
        {
            return text.clone();
        }
        match self.fetch_boss_chance().await {
            Ok(text) => {
                *guard = Some((Instant::now(), text.clone()));
                text
            }
            // 失败结果**不进缓存**：否则一次网络抖动会让「查询失败」挂十分钟。
            Err(err) => {
                tracing::warn!(error = %err, "查询塔科夫 BOSS 刷新率失败");
                "查询失败，稍后再试".to_string()
            }
        }
    }

    /// 服务器状态文本，命中缓存则不打上游。
    async fn server_status_text(&self) -> String {
        let mut guard = self.status_cache.lock().await;
        if let Some((at, text)) = guard.as_ref()
            && at.elapsed() < self.config.status_cache
        {
            return text.clone();
        }
        match self.fetch_server_status().await {
            Ok(text) => {
                *guard = Some((Instant::now(), text.clone()));
                text
            }
            Err(err) => {
                tracing::warn!(error = %err, "查询塔科夫服务器状态失败");
                "查询失败，稍后再试".to_string()
            }
        }
    }

    async fn fetch_server_status(&self) -> anyhow::Result<String> {
        let base = self.config.status_base.trim_end_matches('/');
        let services: Vec<ServiceStatus> = self
            .get_json(&format!("{base}/api/services"))
            .await
            .context("获取服务列表失败")?;
        let global: GlobalStatus = self
            .get_json(&format!("{base}/api/global/status"))
            .await
            .context("获取总体状态失败")?;
        Ok(format_server_status(&services, &global, now_unix()))
    }

    /// GET 一个 JSON 接口。
    ///
    /// 状态站会按 UA 拦请求，所以带一个常见的浏览器 UA —— 源项目也这么做。
    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> anyhow::Result<T> {
        let res = self
            .http
            .get(url)
            .header("Accept", "application/json")
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) qqbot-rs")
            .send()
            .await
            .with_context(|| format!("请求 {url} 失败"))?;
        let status = res.status();
        let body = res.text().await.with_context(|| format!("读取 {url} 响应失败"))?;
        if !status.is_success() {
            let snippet: String = body.chars().take(200).collect();
            anyhow::bail!("{url} 返回 {status}：{snippet}");
        }
        serde_json::from_str(&body).with_context(|| format!("解析 {url} 响应失败"))
    }

    /// 取 BOSS 刷新率。
    ///
    /// 走**静态 JSON**而不是 GraphQL：两者是同一份数据，但 GraphQL 后端
    /// 自 2026-09 起对所有查询返回 422，而静态 JSON 一直可用。
    /// 代价是名字从中文变成 slug（`bossTagilla`）。
    async fn fetch_boss_chance(&self) -> anyhow::Result<String> {
        let body = self
            .get_text(&self.config.maps_url)
            .await
            .context("下载地图数据失败")?;
        let maps = parse_maps(&body).map_err(anyhow::Error::msg)?;
        if maps.is_empty() {
            anyhow::bail!("地图数据里没有可用条目，上游格式可能变了");
        }
        Ok(format_boss_chance(&maps, now_unix()))
    }

    /// GET 一段文本。
    async fn get_text(&self, url: &str) -> anyhow::Result<String> {
        let res = self
            .http
            .get(url)
            .header("Accept", "application/json")
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) qqbot-rs")
            .send()
            .await
            .with_context(|| format!("请求 {url} 失败"))?;
        let status = res.status();
        let body = res.text().await.with_context(|| format!("读取 {url} 响应失败"))?;
        if !status.is_success() {
            let snippet: String = body.chars().take(200).collect();
            anyhow::bail!("{url} 返回 {status}：{snippet}");
        }
        Ok(body)
    }
}

#[async_trait]
impl Handler for TarkovPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        let content = ctx.content();

        if content == "塔科夫时间" {
            // 与源项目一致：两行裸时刻，不带标签。
            let (left, right) = tarkov_time(now_unix());
            let _ = ctx.reply_text(format!("{left}\n{right}")).await;
            return Handled::Consumed;
        }

        if is_server_query(content) {
            let _ = ctx.reply_text(self.server_status_text().await).await;
            return Handled::Consumed;
        }

        if is_boss_query(content) {
            let _ = ctx.reply_text(self.boss_chance_text().await).await;
            return Handled::Consumed;
        }

        Handled::Next
    }

    fn name(&self) -> &'static str {
        "塔科夫"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_gives_a_known_clock() {
        // 现实 0 点 → 游戏内 0 点，加莫斯科 +3 得 03:00:00；
        // 另一个值早 12 小时，取模后是 15:00:00。
        assert_eq!(tarkov_time(0), ("15:00:00".to_string(), "03:00:00".to_string()));
    }

    #[test]
    fn the_two_clocks_are_twelve_hours_apart() {
        for now in [0, 1_700_000_000, 1_759_000_000, 2_000_000_000] {
            let (left, right) = tarkov_clock(now);
            assert_eq!(
                (left - right).rem_euclid(DAY),
                CLOCK_SPLIT_SECS,
                "两个出发时间必须相差 12 小时（now={now}）"
            );
        }
    }

    #[test]
    fn clock_advances_seven_times_faster() {
        let (_, before) = tarkov_clock(1_700_000_000);
        let (_, after) = tarkov_clock(1_700_000_000 + 3600);
        assert_eq!((after - before).rem_euclid(DAY), 7 * 3600);
    }

    #[test]
    fn clock_is_zero_padded() {
        assert_eq!(format_clock(0), "00:00:00");
        assert_eq!(format_clock(59), "00:00:59");
        assert_eq!(format_clock(3600 + 120 + 3), "01:02:03");
        assert_eq!(format_clock(DAY - 1), "23:59:59");
    }

    fn entry(mob: &str, chance: f64) -> BossEntry {
        BossEntry { mob: mob.into(), spawn_chance: chance }
    }

    #[test]
    fn averages_multiple_spawn_points_of_one_boss() {
        // 同一张图上同一个 BOSS 的两个刷新点，取平均。
        let maps = vec![MapEntry {
            normalized_name: "customs".into(),
            bosses: vec![entry("bossReshala", 0.4), entry("bossReshala", 0.6)],
        }];
        let text = format_boss_chance(&maps, 0);
        assert!(text.contains("customs\n"), "地图名独占一行: {text}");
        assert!(text.contains("    bossReshala: 50%"), "(0.4+0.6)/2 = 50%: {text}");
    }

    #[test]
    fn preserves_the_given_map_order() {
        // 源项目用 HashMap，顺序随机；这里至少要保持调用方给的顺序，
        // 这样 `parse_maps` 的排序才是唯一的不确定来源。
        let maps = vec![
            MapEntry {
                normalized_name: "a".into(),
                bosses: vec![entry("二", 0.1), entry("一", 0.2)],
            },
            MapEntry { normalized_name: "b".into(), bosses: vec![entry("三", 0.3)] },
        ];
        let text = format_boss_chance(&maps, 0);
        let a = text.find("二").unwrap();
        let b = text.find("一").unwrap();
        let c = text.find("三").unwrap();
        assert!(a < b && b < c, "应当保持给定顺序: {text}");
    }

    #[test]
    fn skips_maps_without_bosses_and_ends_with_the_timestamp() {
        let maps = vec![
            MapEntry { normalized_name: "empty".into(), bosses: vec![] },
            MapEntry { normalized_name: "woods".into(), bosses: vec![entry("bossKilla", 1.0)] },
        ];
        let text = format_boss_chance(&maps, 0);
        assert!(!text.contains("empty"), "没有 BOSS 的地图不该出现: {text}");
        assert!(text.contains("    bossKilla: 100%"), "1.0 是比例不是百分数: {text}");
        assert!(
            text.ends_with("1970-01-01 08:00:00"),
            "末尾应当是北京时间戳: {text}"
        );
    }

    #[test]
    fn entries_without_a_boss_name_are_ignored() {
        let maps = vec![MapEntry {
            normalized_name: "map".into(),
            bosses: vec![BossEntry { mob: String::new(), spawn_chance: 0.5 }],
        }];
        let text = format_boss_chance(&maps, 0);
        assert!(!text.contains("map\n"), "没有名字的条目不该产生标题: {text}");
    }

    #[test]
    fn parses_maps_from_the_static_json() {
        // 形状照实测的 `json.tarkov.dev/regular/maps`：`maps` 是**按 id 键控的对象**。
        let body = concat!(
            r#"{"data":{"maps":{"#,
            r#""m2":{"normalizedName":"woods","bosses":[{"mob":"bossShturman","spawnChance":0.6}]},"#,
            r#""m1":{"normalizedName":"customs","bosses":[{"mob":"bossReshala","spawnChance":0.35}]},"#,
            r#""m3":{"normalizedName":"","bosses":[]}"#,
            r#"}}}"#,
        );
        let maps = parse_maps(body).unwrap();
        assert_eq!(maps.len(), 2, "没有 slug 的地图要被丢掉: {maps:?}");
        assert_eq!(maps[0].normalized_name, "customs", "应当按名称排序，不依赖 HashMap 顺序");
        assert_eq!(maps[1].normalized_name, "woods");
        assert_eq!(maps[0].bosses[0].mob, "bossReshala");
        assert!((maps[0].bosses[0].spawn_chance - 0.35).abs() < f64::EPSILON);
    }

    /// 实跑验证：打**真实**接口。默认 `#[ignore]`，手动跑：
    ///
    /// ```text
    /// cargo test -p qqbot-plugins --lib -- --ignored live_maps
    /// ```
    ///
    /// 存在的意义：mock 只能证明「按我理解的形状能解析」，
    /// 证明不了「上游真的是这个形状」。
    #[tokio::test]
    #[ignore = "需要网络"]
    async fn live_maps_endpoint_parses() {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        let body = http
            .get("https://json.tarkov.dev/regular/maps")
            .header("User-Agent", "Mozilla/5.0 qqbot-rs")
            .send()
            .await
            .expect("请求失败")
            .text()
            .await
            .expect("读取失败");
        let maps = parse_maps(&body).expect("解析失败");
        assert!(maps.len() >= 10, "应当解析出十几张地图，实际 {}", maps.len());
        let with_boss = maps.iter().filter(|m| !m.bosses.is_empty()).count();
        assert!(with_boss >= 5, "应当有若干张图带 BOSS，实际 {with_boss}");
        let text = format_boss_chance(&maps, 0);
        assert!(text.contains("customs"), "输出里应当有 customs: {text}");
    }

    #[test]
    fn parse_maps_rejects_broken_input() {
        assert!(parse_maps("不是 JSON").is_err());
        assert!(parse_maps(r#"{"data":null}"#).is_err());
    }

    fn service(name: &str, status: i64) -> ServiceStatus {
        ServiceStatus { name: name.into(), status }
    }

    #[test]
    fn recognizes_all_six_server_forms() {
        for text in [
            "服务器",
            "服务器状态",
            "塔科夫服务器",
            "塔科夫服务器状态",
            "tkf服务器",
            "TKF服务器状态",
            "  服务器  ",
        ] {
            assert!(is_server_query(text), "应当识别：{text}");
        }
    }

    #[test]
    fn server_query_rejects_lookalikes() {
        for text in [
            "服务器状态怎么样",
            "看看服务器",
            "塔科夫服务器状态如何",
            "tkf 服务器",
            "塔科夫",
            "",
        ] {
            assert!(!is_server_query(text), "不该识别：{text}");
        }
    }

    #[test]
    fn formats_server_status_with_chinese_labels() {
        let services = vec![service("Website", 0), service("Matchmaking", 2)];
        let global = GlobalStatus { status: 1, message: "正在维护".into() };
        let text = format_server_status(&services, &global, 0);
        assert!(text.starts_with("1970-01-01 08:00:00\n服务器状态速报：\n"), "{text}");
        assert!(text.contains("游戏官网：🟢服务正常"), "{text}");
        assert!(text.contains("战局匹配：🟡部分故障"), "{text}");
        assert!(text.contains("\n总体状态：⚙️正在更新"), "{text}");
        assert!(text.ends_with("信息：正在维护"), "{text}");
    }

    #[test]
    fn unknown_services_and_statuses_are_shown_not_dropped() {
        // 官方随时可能加服务或加状态码，静默吞掉会让状态页看起来少了几行。
        let services = vec![service("Something New", 9)];
        let global = GlobalStatus { status: 9, message: String::new() };
        let text = format_server_status(&services, &global, 0);
        assert!(text.contains("Something New：⚪未知"), "{text}");
        assert!(text.contains("总体状态：⚪未知"), "{text}");
        assert!(!text.contains("信息："), "没有 message 就不该出现这一行: {text}");
    }

    #[test]
    fn recognizes_boss_queries_case_insensitively() {
        for text in ["boss刷", "boss概率", "BOSS刷新率", "Boss概览", "  boss刷  "] {
            assert!(is_boss_query(text), "应当识别：{text}");
        }
        for text in ["boss", "刷boss", "塔科夫boss", "塔科夫时间", ""] {
            assert!(!is_boss_query(text), "不该识别：{text}");
        }
    }
}
