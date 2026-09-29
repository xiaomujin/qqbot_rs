//! 塔科夫相关命令。
//!
//! 从 cq-bot 的 `TkfPlugin` / `BulletPlugin` 迁移而来。源项目把它们拆在两个
//! 文件里（时间/子弹在 `BulletPlugin`，BOSS/任务在 `TkfPlugin`），这里合并，
//! 免得每加一个命令就多一个只含几十行的文件。

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

/// BOSS 刷新率查询。
///
/// `lang: zh` 让接口直接返回中文名，省掉一份本地译名表。
const QUERY_BOSS_CHANCE: &str =
    "{ maps(gameMode: regular, lang: zh) { name bosses { boss { name } spawnChance } } }";

/// 塔科夫插件配置。
#[derive(Debug, Clone)]
pub struct TarkovConfig {
    /// GraphQL 端点。
    pub graphql_url: String,
    /// BOSS 刷新率缓存时长。
    ///
    /// 刷新率是**静态数据**，缓存纯粹是为了防刷 —— 源项目也是 10 分钟。
    pub boss_cache: Duration,
}

impl Default for TarkovConfig {
    fn default() -> Self {
        Self {
            graphql_url: "https://api.tarkov.dev/graphql".into(),
            boss_cache: Duration::from_secs(600),
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
struct GraphQlResponse {
    #[serde(default)]
    data: Option<MapsData>,
}

#[derive(Debug, Deserialize)]
struct MapsData {
    #[serde(default)]
    maps: Vec<MapEntry>,
}

#[derive(Debug, Deserialize)]
pub struct MapEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub bosses: Vec<BossEntry>,
}

#[derive(Debug, Deserialize)]
pub struct BossEntry {
    #[serde(default)]
    pub boss: Option<BossName>,
    /// 0.0 ~ 1.0 的比例，不是百分数。
    #[serde(default, rename = "spawnChance")]
    pub spawn_chance: f64,
}

#[derive(Debug, Deserialize)]
pub struct BossName {
    #[serde(default)]
    pub name: String,
}

/// 整理成「地图 → BOSS 平均刷新率」的文本，末尾附查询时间。
///
/// 同一张图上同一个 BOSS 可能有多个刷新点，取**平均值**（与源项目一致）。
///
/// 地图与 BOSS 都按**接口返回顺序**输出。源项目用的是 Java `HashMap`，
/// 迭代顺序随机，同一份数据每次刷新出来的排列都不一样；保序是刻意的改进。
pub fn format_boss_chance(maps: &[MapEntry], now: i64) -> String {
    let mut out = String::new();
    for map in maps {
        // 保序聚合：同一个 BOSS 的多个刷新点收集到一起。
        let mut per_boss: Vec<(&str, Vec<f64>)> = Vec::new();
        for entry in &map.bosses {
            let Some(name) = entry.boss.as_ref().map(|b| b.name.as_str()) else {
                continue;
            };
            match per_boss.iter_mut().find(|(n, _)| *n == name) {
                Some((_, chances)) => chances.push(entry.spawn_chance),
                None => per_boss.push((name, vec![entry.spawn_chance])),
            }
        }
        if per_boss.is_empty() {
            // 源项目会把没有 BOSS 的地图也打出一行空标题，这里跳过。
            continue;
        }
        out.push_str(&map.name);
        out.push('\n');
        for (name, chances) in per_boss {
            let avg = chances.iter().sum::<f64>() / chances.len() as f64;
            let _ = writeln!(out, "    {name}: {:.0}%", avg * 100.0);
        }
    }
    out.push_str(&format_datetime(now, SHANGHAI_OFFSET));
    out
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
}

impl TarkovPlugin {
    pub fn new(config: TarkovConfig, http: reqwest::Client) -> Self {
        Self { config, http, boss_cache: Arc::new(Mutex::new(None)) }
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

    async fn fetch_boss_chance(&self) -> anyhow::Result<String> {
        let res = self
            .http
            .post(&self.config.graphql_url)
            .header("Accept", "application/json")
            .json(&serde_json::json!({ "query": QUERY_BOSS_CHANCE }))
            .send()
            .await
            .context("请求塔科夫 GraphQL 失败")?;
        let status = res.status();
        let body = res.text().await.context("读取塔科夫 GraphQL 响应失败")?;
        if !status.is_success() {
            // 后端故障时返回 422 + {"errors":["GraphQL server unavailable..."]}。
            // 把原文带进错误里，否则排查时只看得到一个状态码。
            let snippet: String = body.chars().take(200).collect();
            anyhow::bail!("塔科夫 GraphQL 返回 {status}：{snippet}");
        }
        let parsed: GraphQlResponse =
            serde_json::from_str(&body).context("解析塔科夫 GraphQL 响应失败")?;
        let data = parsed.data.context("塔科夫 GraphQL 响应缺少 data")?;
        Ok(format_boss_chance(&data.maps, now_unix()))
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

    fn entry(boss: &str, chance: f64) -> BossEntry {
        BossEntry { boss: Some(BossName { name: boss.into() }), spawn_chance: chance }
    }

    #[test]
    fn averages_multiple_spawn_points_of_one_boss() {
        // 同一张图上同一个 BOSS 的两个刷新点，取平均。
        let maps = vec![MapEntry {
            name: "海关".into(),
            bosses: vec![entry("Reshala", 0.4), entry("Reshala", 0.6)],
        }];
        let text = format_boss_chance(&maps, 0);
        assert!(text.contains("海关\n"), "地图名独占一行: {text}");
        assert!(text.contains("    Reshala: 50%"), "(0.4+0.6)/2 = 50%: {text}");
    }

    #[test]
    fn keeps_the_api_order() {
        // 源项目用 HashMap，顺序随机；这里必须是接口给的顺序。
        let maps = vec![
            MapEntry { name: "A".into(), bosses: vec![entry("二", 0.1), entry("一", 0.2)] },
            MapEntry { name: "B".into(), bosses: vec![entry("三", 0.3)] },
        ];
        let text = format_boss_chance(&maps, 0);
        let a = text.find("二").unwrap();
        let b = text.find("一").unwrap();
        let c = text.find("三").unwrap();
        assert!(a < b && b < c, "应当保持接口顺序: {text}");
    }

    #[test]
    fn skips_maps_without_bosses_and_ends_with_the_timestamp() {
        let maps = vec![
            MapEntry { name: "空图".into(), bosses: vec![] },
            MapEntry { name: "有图".into(), bosses: vec![entry("Killa", 1.0)] },
        ];
        let text = format_boss_chance(&maps, 0);
        assert!(!text.contains("空图"), "没有 BOSS 的地图不该出现: {text}");
        assert!(text.contains("    Killa: 100%"), "1.0 是比例不是百分数: {text}");
        assert!(
            text.ends_with("1970-01-01 08:00:00"),
            "末尾应当是北京时间戳: {text}"
        );
    }

    #[test]
    fn entries_without_a_boss_name_are_ignored() {
        let maps = vec![MapEntry {
            name: "图".into(),
            bosses: vec![BossEntry { boss: None, spawn_chance: 0.5 }],
        }];
        let text = format_boss_chance(&maps, 0);
        assert!(!text.contains("图\n"), "没有名字的条目不该产生标题: {text}");
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
