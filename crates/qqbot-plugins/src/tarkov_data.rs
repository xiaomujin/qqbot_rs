//! 塔科夫数据刷新：**一次下载 → 三张表**。
//!
//! 上游是七个静态 JSON（items / items_zh / traders / traders_zh / tasks /
//! tasks_zh / maps_zh），合计约 21 MB。改造前它们被三个插件各下一遍
//! （更新物品 / 更新子弹 / 更新任务），三条命令各写各的表 ——
//! 于是「子弹是新的、物品是旧的」这种半新半旧状态是常态，而带宽付了三份。
//!
//! 现在只有这一条刷新链路：7 个 URL 并发下载 → 解析 → 写 ammo / tarkov_item /
//! tarkov_task*（9 张表）。
//!
//! 三个触发点都走 [`TarkovData::refresh`]：
//!
//! 1. **每小时自动**（[`TarkovData::spawn_hourly`]，由 `register` 在启用时拉起）；
//! 2. **启动回填**：距上次成功刷新已超过周期（或表刚被 `ensure_tarkov_tables`
//!    重建过）时立刻刷一次；
//! 3. **系统控制者手动**：`塔科夫更新数据` / `tkf更新数据`。
//!
//! # 三条硬规则
//!
//! - **先全部下载、全部解析，再动数据库**：任何一个端点挂了都直接返回错误，
//!   一张表都不写。宁可整体失败，也不要「物品更新了、任务没更新」。
//! - **失败保留旧数据**：不 DELETE、不清空、不写半截。稍后自动重试。
//! - **中文只是增强，不是前提**：早先任务导入有一道 98% 覆盖率闸门，不达标就
//!   整批放弃。代价是上游语言包一抖，查询就停更 —— 现在改为**降级**：
//!   名字依次退回「中文 → 英文名 → slug」，覆盖率只进日志与指标。
//!
//! 写库是三张表各自的整表替换（每个方法自带事务）。三个事务之间**不是**一个
//! 大事务：混在一起的代价是任何一张表写失败都要回滚另外两张，而回滚后的状态
//! 与「刷新前」并无区别 —— 既然下一次刷新会全覆盖，就不值得为原子性引入
//! 一个跨表事务（它还会长时间占住写锁，挡住消息入库）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use qqbot_store::{REFRESH_SETTING_KEY, ResourceStore, TarkovItem, now_unix};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::ammo::{ItemLangPack, parse_ammo, parse_item_lang};
use crate::task::{Coverage, LangPacks, parse_lang_pack, parse_tasks};
use crate::timewin::{SHANGHAI_OFFSET, format_datetime};

/// 一次刷新失败后等多久再试。
///
/// 不用「等满一个周期」：网络抖动是五分钟级别的事，等一小时等于放弃这一小时
/// 的价格新鲜度；也不能立刻重试 —— 每小时 21 MB，热循环会把上游和我们自己都打趴。
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(600);

/// 刷新用的配置。
///
/// `refresh_hours` 默认 **0（关闭自动刷新）**：库的默认值不联网。
/// 产品路径由 `plugins_from_config` 按 `[tarkov] refresh_hours` 填（默认 1），
/// 测试与 e2e 用默认值，于是**不会有后台任务去打真实上游**。
#[derive(Clone)]
pub struct TarkovDataConfig {
    pub items_url: String,
    pub items_zh_url: String,
    pub traders_url: String,
    pub traders_zh_url: String,
    pub tasks_url: String,
    pub tasks_zh_url: String,
    pub maps_zh_url: String,
    /// 自动刷新周期（小时）。`0` 表示**关闭**自动刷新（手动命令仍然可用）。
    pub refresh_hours: u64,
    /// 资源库。`None` 表示未启用持久化，刷新与查询都不可用。
    pub store: Option<Arc<ResourceStore>>,
}

impl Default for TarkovDataConfig {
    fn default() -> Self {
        Self {
            items_url: "https://json.tarkov.dev/regular/items".into(),
            items_zh_url: "https://json.tarkov.dev/regular/items_zh".into(),
            traders_url: "https://json.tarkov.dev/regular/traders".into(),
            traders_zh_url: "https://json.tarkov.dev/regular/traders_zh".into(),
            tasks_url: "https://json.tarkov.dev/regular/tasks".into(),
            tasks_zh_url: "https://json.tarkov.dev/regular/tasks_zh".into(),
            maps_zh_url: "https://json.tarkov.dev/regular/maps_zh".into(),
            refresh_hours: 0,
            store: None,
        }
    }
}

/// 一次刷新的结果。
#[derive(Debug, Clone)]
pub struct RefreshReport {
    pub items: usize,
    pub ammo: usize,
    pub tasks: usize,
    pub coverage: Coverage,
    pub elapsed: Duration,
}

impl RefreshReport {
    /// 给系统控制者看的回执（Markdown）。
    pub fn summary(&self, now: i64) -> String {
        format!(
            "## 塔科夫数据已更新\n\n- 物品 {} 件\n- 弹药 {} 条\n- 任务 {} 条\n\n{}\n\n> 用时 {:.1}s · {}",
            self.items,
            self.ammo,
            self.tasks,
            self.coverage.summary(),
            self.elapsed.as_secs_f64(),
            format_datetime(now, SHANGHAI_OFFSET),
        )
    }
}

/// 三张表的当前行数。
#[derive(Debug, Clone, Copy, Default)]
pub struct Counts {
    pub items: i64,
    pub ammo: i64,
    pub tasks: i64,
}

/// 塔科夫数据句柄：刷新逻辑 + 并发闸 + 定时器。
///
/// `Clone` 是必需的：手动命令插件与定时任务必须共用**同一把锁**，
/// 否则「管理员手点刷新」与「整点自动刷新」会同时下两份 21 MB。
#[derive(Clone)]
pub struct TarkovData {
    config: TarkovDataConfig,
    http: reqwest::Client,
    lock: Arc<Mutex<()>>,
}

impl TarkovData {
    /// `http` 应当是用 [`crate::http::build_bulk_client`] 建的（300s 超时）。
    pub fn new(config: TarkovDataConfig, http: reqwest::Client) -> Self {
        Self { config, http, lock: Arc::new(Mutex::new(())) }
    }

    /// 资源库句柄。
    pub fn store(&self) -> Option<&Arc<ResourceStore>> {
        self.config.store.as_ref()
    }

    /// 自动刷新周期。`refresh_hours == 0` 时为 `None`。
    pub fn interval(&self) -> Option<Duration> {
        (self.config.refresh_hours > 0)
            .then(|| Duration::from_secs(self.config.refresh_hours * 3600))
    }

    /// 上一次**成功**刷新的时刻（unix 秒）。
    pub async fn last_success(&self) -> Option<i64> {
        let store = self.config.store.as_ref()?;
        match store.get_setting(REFRESH_SETTING_KEY).await {
            Ok(Some(raw)) => raw.trim().parse().ok(),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(error = %err, "读取上次刷新时间失败，按「从未刷新」处理");
                None
            }
        }
    }

    /// 三张表当前的行数。读失败按 0 处理 —— 这只用于提示文案。
    pub async fn counts(&self) -> Counts {
        let Some(store) = self.config.store.as_ref() else {
            return Counts::default();
        };
        Counts {
            items: store.item_count().await.unwrap_or(0),
            ammo: store.ammo_count().await.unwrap_or(0),
            tasks: store.task_count().await.unwrap_or(0),
        }
    }

    /// 立刻刷新一次。**带锁**：同一时刻只会有一次刷新在跑，后来者等它。
    ///
    /// # Errors
    ///
    /// 未启用存储、任一端点下载/解析失败、写库失败。
    pub async fn refresh(&self) -> anyhow::Result<RefreshReport> {
        let Some(store) = self.config.store.clone() else {
            anyhow::bail!("未启用持久化，塔科夫数据无法刷新");
        };
        // 锁跨越 await：并发触发时只有第一个真的下载。
        let _guard = self.lock.lock().await;
        let started = Instant::now();
        let result = self.refresh_inner(&store).await;
        let elapsed = started.elapsed();
        match result {
            Ok(mut report) => {
                report.elapsed = elapsed;
                metrics::counter!("qqbot_tarkov_refresh_total", "result" => "ok").increment(1);
                metrics::histogram!("qqbot_tarkov_refresh_duration_seconds")
                    .record(elapsed.as_secs_f64());
                tracing::info!(
                    items = report.items,
                    ammo = report.ammo,
                    tasks = report.tasks,
                    coverage = %report.coverage.summary(),
                    elapsed = ?elapsed,
                    "塔科夫数据刷新完成"
                );
                Ok(report)
            }
            Err(err) => {
                metrics::counter!("qqbot_tarkov_refresh_total", "result" => "fail").increment(1);
                tracing::warn!(error = %err, "塔科夫数据刷新失败，保留旧数据");
                Err(err)
            }
        }
    }

    /// 下载 → 解析 → 写库。**不**加锁、**不**打指标，只由 [`Self::refresh`] 调用。
    async fn refresh_inner(&self, store: &Arc<ResourceStore>) -> anyhow::Result<RefreshReport> {
        let fetched = self.fetch_all().await?;

        // 语言包失败**不再是致命错误**：名字退回 slug 也照样能用，
        // 而整批放弃会让一次无关的语言包抖动停掉整块功能。
        // 物品名语言包走 `parse_item_lang`（它认的就是 `<id> Name` 这套键），
        // 其余三份走 `parse_lang_pack`（键是翻译键本身）。
        let item_pack = match parse_item_lang(&fetched.items_zh) {
            Ok(pack) => pack,
            Err(err) => {
                tracing::warn!(error = %err, "物品名语言包解析失败，本次按无中文处理");
                ItemLangPack::new()
            }
        };
        let tasks_pack = self.lang_or_empty(&fetched.tasks_zh, "任务名语言包");
        let traders_pack = self.lang_or_empty(&fetched.traders_zh, "商人名语言包");
        let maps_pack = self.lang_or_empty(&fetched.maps_zh, "地图名语言包");
        let packs = LangPacks {
            tasks: tasks_pack,
            items: item_pack.clone(),
            traders: traders_pack.clone(),
            maps: maps_pack,
        };

        let trader_names = parse_trader_names(&fetched.traders, &traders_pack);
        let items =
            parse_items(&fetched.items, &item_pack, &trader_names).map_err(anyhow::Error::msg)?;
        let ammo = parse_ammo(&fetched.items, &item_pack).map_err(anyhow::Error::msg)?;
        let (tasks, coverage) =
            parse_tasks(&fetched.tasks, &fetched.traders, &packs).map_err(anyhow::Error::msg)?;

        // 三条「解析出 0 条」都说明上游换了形状 —— 那才是真正的灾难，
        // 写进去等于把好数据换成空表。
        if items.is_empty() {
            anyhow::bail!("物品数据解析出 0 件，上游格式可能变了");
        }
        if ammo.is_empty() {
            anyhow::bail!("弹药数据解析出 0 条，上游格式可能变了");
        }
        if tasks.is_empty() {
            anyhow::bail!("任务数据解析出 0 条，上游格式可能变了");
        }

        // 到这一步才开始写。三个整表替换各自成事务（理由见模块文档）。
        store.replace_ammo(ammo).await?;
        store.replace_items(items).await?;
        store.replace_tasks(tasks).await?;
        // 三张表都写成功了才记「刚刷过」—— 半截成功不该被当成有效刷新。
        store.set_setting(REFRESH_SETTING_KEY, &now_unix().to_string()).await?;

        let counts = self.counts().await;
        Ok(RefreshReport {
            items: usize::try_from(counts.items).unwrap_or(0),
            ammo: usize::try_from(counts.ammo).unwrap_or(0),
            tasks: usize::try_from(counts.tasks).unwrap_or(0),
            coverage,
            elapsed: Duration::ZERO,
        })
    }

    /// 解析一份语言包，失败就 warn 并用空包（= 全英文降级）。
    fn lang_or_empty(&self, body: &str, what: &str) -> ItemLangPack {
        match parse_lang_pack(body) {
            Ok(pack) => pack,
            Err(err) => {
                tracing::warn!(error = %err, what, "语言包解析失败，本次按无中文处理");
                ItemLangPack::new()
            }
        }
    }

    /// 七个端点**并发**下载。
    ///
    /// 串行要 30 秒以上（items 一个就 18s），并发把墙钟压到最慢的那个；
    /// 而这七次请求之间没有任何依赖。
    async fn fetch_all(&self) -> anyhow::Result<Fetched> {
        let cfg = &self.config;
        let http = self.http.clone();
        let get = |url: &str| {
            let http = http.clone();
            let url = url.to_string();
            async move { crate::http::fetch_text(&http, &url).await }
        };
        let (items, items_zh, traders, traders_zh, tasks, tasks_zh, maps_zh) = tokio::join!(
            get(&cfg.items_url),
            get(&cfg.items_zh_url),
            get(&cfg.traders_url),
            get(&cfg.traders_zh_url),
            get(&cfg.tasks_url),
            get(&cfg.tasks_zh_url),
            get(&cfg.maps_zh_url),
        );
        Ok(Fetched {
            items: items.map_err(|err| anyhow::anyhow!("下载物品数据失败：{err}"))?,
            items_zh: items_zh.map_err(|err| anyhow::anyhow!("下载物品语言包失败：{err}"))?,
            traders: traders.map_err(|err| anyhow::anyhow!("下载商人数据失败：{err}"))?,
            traders_zh: traders_zh.map_err(|err| anyhow::anyhow!("下载商人语言包失败：{err}"))?,
            tasks: tasks.map_err(|err| anyhow::anyhow!("下载任务数据失败：{err}"))?,
            tasks_zh: tasks_zh.map_err(|err| anyhow::anyhow!("下载任务语言包失败：{err}"))?,
            maps_zh: maps_zh.map_err(|err| anyhow::anyhow!("下载地图语言包失败：{err}"))?,
        })
    }

    /// 拉起每小时刷新。
    ///
    /// 返回 `None` 表示没启用（`refresh_hours == 0` 或没有存储）。
    /// **不复用 `tokio::time::interval`**：这里要的不是「固定节拍」，
    /// 而是「距上次成功刷新满一个周期」—— 进程重启、刷新失败重试、
    /// 表被重建，这几种情况只有按时间戳算才对得上。
    pub fn spawn_hourly(&self) -> Option<tokio::task::JoinHandle<()>> {
        self.interval()?;
        self.config.store.as_ref()?;
        let this = self.clone();
        Some(tokio::spawn(async move {
            loop {
                let wait = this.wait_before_next().await;
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
                if this.refresh().await.is_err() {
                    // 失败后按固定间隔重试，而不是立刻 —— 理由见 RETRY_AFTER_FAILURE。
                    tokio::time::sleep(RETRY_AFTER_FAILURE).await;
                }
            }
        }))
    }

    /// 距「下次该刷新」还有多久。`ZERO` 表示现在就刷。
    async fn wait_before_next(&self) -> Duration {
        let Some(interval) = self.interval() else {
            return Duration::ZERO;
        };
        let secs = i64::try_from(interval.as_secs()).unwrap_or(i64::MAX);
        match self.last_success().await {
            // 从未成功过（新库、刚重建表）→ 立刻刷。
            None => Duration::ZERO,
            Some(last) => {
                let elapsed = now_unix().saturating_sub(last);
                if elapsed >= secs {
                    Duration::ZERO
                } else {
                    Duration::from_secs(u64::try_from(secs - elapsed).unwrap_or(0))
                }
            }
        }
    }
}

/// 七个端点下载回来的原文。
struct Fetched {
    items: String,
    items_zh: String,
    traders: String,
    traders_zh: String,
    tasks: String,
    tasks_zh: String,
    maps_zh: String,
}

// ---- 上游 JSON 的形状 ----

#[derive(Debug, Deserialize)]
struct ItemsResponse {
    #[serde(default)]
    data: Option<ItemsData>,
}

#[derive(Debug, Deserialize)]
struct ItemsData {
    #[serde(default)]
    items: HashMap<String, ItemEntry>,
}

#[derive(Debug, Deserialize)]
struct ItemEntry {
    #[serde(default, rename = "normalizedName")]
    normalized_name: String,
    #[serde(default, rename = "basePrice")]
    base_price: i64,
    // 下面四个都可能缺 —— 实测 5498 件里只有 3550 件有跳蚤价格。
    #[serde(default, rename = "lastLowPrice")]
    last_low_price: Option<i64>,
    #[serde(default, rename = "avg24hPrice")]
    avg24h_price: Option<i64>,
    #[serde(default, rename = "low24hPrice")]
    low24h_price: Option<i64>,
    #[serde(default, rename = "high24hPrice")]
    high24h_price: Option<i64>,
    #[serde(default, rename = "changeLast48h")]
    change_48h: Option<i64>,
    #[serde(default, rename = "changeLast48hPercent")]
    change_48h_percent: Option<f64>,
    #[serde(default, rename = "lastOfferCount")]
    offer_count: Option<i64>,
    #[serde(default, rename = "minLevelForFlea")]
    min_level_flea: Option<i64>,
    #[serde(default)]
    width: i64,
    #[serde(default)]
    height: i64,
    #[serde(default)]
    weight: f64,
    #[serde(default, rename = "iconLink")]
    icon_link: Option<String>,
    // `gridImageLink` 是游戏内格子图标（64×64，透明底），跳蚤 / 弹药详情都用它。
    // 不用 `image512pxLink`：那张是 512px 的「照片」，放大看没意义、还把消息撑长。
    #[serde(default, rename = "gridImageLink")]
    grid_image_link: Option<String>,
    #[serde(default, rename = "wikiLink")]
    wiki_link: Option<String>,
    #[serde(default)]
    link: Option<String>,
    #[serde(default, rename = "lastScan")]
    last_scan: Option<String>,
    #[serde(default, rename = "sellToTrader")]
    sell_to_trader: Vec<SellOffer>,
}

/// 上游的一条「卖给某商人」报价。
#[derive(Debug, Deserialize)]
struct SellOffer {
    #[serde(default)]
    trader: String,
    /// 已换算成 RUB 的报价。**必须用它**：Peacekeeper 给的是美元
    /// （实测某件 `price: 54, priceRUB: 6622`），直接比 `price` 会把美元当卢布。
    #[serde(default, rename = "priceRUB")]
    price_rub: Option<i64>,
    #[serde(default)]
    price: i64,
}

impl SellOffer {
    fn rub(&self) -> i64 {
        self.price_rub.unwrap_or(self.price)
    }
}

#[derive(Debug, Deserialize)]
struct TradersResponse {
    #[serde(default)]
    data: Option<HashMap<String, TraderEntry>>,
}

#[derive(Debug, Deserialize)]
struct TraderEntry {
    /// 翻译键（`<id> Nickname`）。
    #[serde(default)]
    name: String,
    #[serde(default, rename = "normalizedName")]
    normalized_name: String,
}

/// 解析物品。没有 slug 的条目会被丢掉 —— 它们没法被检索到。
pub fn parse_items(
    body: &str,
    pack: &ItemLangPack,
    traders: &HashMap<String, String>,
) -> Result<Vec<TarkovItem>, String> {
    let parsed: ItemsResponse =
        serde_json::from_str(body).map_err(|err| format!("解析物品数据失败：{err}"))?;
    let items = parsed.data.ok_or("物品响应缺少 data")?.items;

    let mut out: Vec<TarkovItem> = items
        .into_iter()
        .filter_map(|(id, item)| {
            let normalized_name = item.normalized_name.trim().to_string();
            if normalized_name.is_empty() {
                return None;
            }
            // 最高收购价：按 RUB 比，取最大的一家。
            // 名字在**导入时**解析好 —— 查询路径不查第二张表，也不做 join。
            let best = item
                .sell_to_trader
                .iter()
                .filter(|o| !o.trader.is_empty())
                .max_by_key(|o| o.rub());
            let (sell_trader_name, sell_trader_price) = match best {
                Some(offer) => (
                    Some(
                        traders
                            .get(&offer.trader)
                            .cloned()
                            .unwrap_or_else(|| offer.trader.clone()),
                    ),
                    Some(offer.rub()),
                ),
                None => (None, None),
            };
            Some(TarkovItem {
                name_zh: crate::ammo::item_name(pack, &id),
                id,
                normalized_name,
                base_price: item.base_price,
                last_low_price: item.last_low_price,
                avg24h_price: item.avg24h_price,
                low24h_price: item.low24h_price,
                high24h_price: item.high24h_price,
                change_48h: item.change_48h,
                change_48h_percent: item.change_48h_percent,
                offer_count: item.offer_count,
                sell_trader_name,
                sell_trader_price,
                min_level_flea: item.min_level_flea.unwrap_or(0),
                grid_w: item.width,
                grid_h: item.height,
                weight: item.weight,
                icon_url: item.icon_link,
                image_url: item.grid_image_link,
                wiki_link: item.wiki_link,
                dev_link: item.link,
                last_scan: item.last_scan.as_deref().and_then(parse_iso8601_utc),
            })
        })
        .collect();

    out.sort_by(|a, b| a.normalized_name.cmp(&b.normalized_name));
    Ok(out)
}

/// 商人 id → 显示名。
///
/// `regular/traders` 的 `name` 是翻译键（`<id> Nickname`），由 `traders_zh`
/// 解析；解析不到就退回 `normalizedName`（`prapor`）。两者都没有就把 id 原样
/// 留着 —— 总比丢掉这条报价强。
pub fn parse_trader_names(body: &str, pack: &ItemLangPack) -> HashMap<String, String> {
    let Ok(parsed) = serde_json::from_str::<TradersResponse>(body) else {
        tracing::warn!("解析商人数据失败，商人名将退回 id");
        return HashMap::new();
    };
    parsed
        .data
        .unwrap_or_default()
        .into_iter()
        .map(|(id, t)| {
            let name = pack
                .get(&t.name)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| t.normalized_name.trim().to_string());
            (id, name)
        })
        .collect()
}

/// 解析上游的 ISO-8601 UTC 时间戳（`2026-11-13T02:48:02.000Z`）。
///
/// 手写而不是引入 `chrono` / `time`：整个项目只有这一处需要解析时间，
/// 多一个日期库就多一份版本与体积。格式是上游固定的（带 `Z`，毫秒可变）。
pub fn parse_iso8601_utc(raw: &str) -> Option<i64> {
    let num = |range: std::ops::Range<usize>| -> Option<i64> { raw.get(range)?.parse().ok() };
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Howard Hinnant 的 `days_from_civil`：公历日期 → 距 1970-01-01 的天数。
///
/// 闰年规则（每 4 年、每 100 年、每 400 年）全在里面，
/// 不必自己写 `if` —— 自己写的那份必然会在 2100 年错一次。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---- 手动刷新命令 ----

/// `塔科夫更新数据` / `tkf更新数据`。
///
/// 旧的三条（`更新物品` / `更新子弹` / `更新任务`）**不再注册别名**：
/// 它们名字承诺的是「只刷一张表」，而现在三条都是全量刷新 ——
/// 留着只会让人以为可以只刷一半。
#[derive(Clone)]
pub struct TarkovUpdatePlugin {
    data: TarkovData,
}

impl TarkovUpdatePlugin {
    pub fn new(data: TarkovData) -> Self {
        Self { data }
    }
}

#[async_trait]
impl Handler for TarkovUpdatePlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        if !matches!(
            ctx.content().split_whitespace().next(),
            Some("塔科夫更新数据" | "tkf更新数据")
        ) {
            return Handled::Next;
        }
        let Some(store) = self.data.store().cloned() else {
            let _ = ctx.reply.text("未启用持久化，塔科夫数据不可用").await;
            return Handled::Consumed;
        };
        if !is_system_controller(ctx, &store).await {
            let _ = ctx.reply.text("只有系统控制者能更新塔科夫数据").await;
            return Handled::Consumed;
        }

        // 刚刷过就不下载：21 MB 不该被连点两下触发两遍。
        if let (Some(last), Some(interval)) = (self.data.last_success().await, self.data.interval()) {
            let interval = i64::try_from(interval.as_secs()).unwrap_or(i64::MAX);
            let age = now_unix().saturating_sub(last);
            if age < interval {
                let counts = self.data.counts().await;
                let _ = ctx
                    .reply
                    .markdown(format!(
                        "## 数据已是最新\n\n- 物品 {} 件\n- 弹药 {} 条\n- 任务 {} 条\n\n> 上次刷新在 {} 分钟前",
                        counts.items,
                        counts.ammo,
                        counts.tasks,
                        age / 60,
                    ))
                    .await;
                return Handled::Consumed;
            }
        }

        match self.data.refresh().await {
            Ok(report) => {
                if let Err(err) = ctx.reply.markdown(report.summary(now_unix())).await {
                    tracing::warn!(error = %err, "刷新回执发送失败");
                }
            }
            Err(err) => {
                // 失败也要回一句：命令发出去了却没有任何回应，比报错更难查。
                let _ = ctx.reply.text(format!("刷新失败：{err}")).await;
            }
        }
        Handled::Consumed
    }

    fn name(&self) -> &'static str {
        "塔科夫数据刷新"
    }
}

/// 是否是系统控制者。
///
/// 名单由 store 持有，与「系统控制者」命令是同一份数据 ——
/// 全量刷新是重操作（7 个端点、约 21 MB），只让系统控制者触发。
pub async fn is_system_controller(ctx: &Ctx, store: &ResourceStore) -> bool {
    let Some(openid) = ctx.sender_openid() else {
        return false;
    };
    // 键名与 JSON 解码都在 store 里，这里只问「是不是」。
    match store.is_system_controller(openid).await {
        Ok(hit) => hit,
        Err(err) => {
            tracing::warn!(error = %err, "读取系统控制者失败");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ITEMS: &str = r#"{"data":{"items":{"i1":{"normalizedName":"colt-m4a1-556x45-assault-rifle","basePrice":18397,"lastLowPrice":23932,"avg24hPrice":53707,"low24hPrice":20000,"high24hPrice":185000,"changeLast48h":215,"changeLast48hPercent":0.4,"lastOfferCount":20,"minLevelForFlea":25,"width":1,"height":2,"weight":3.4,"iconLink":"https://a/i1-icon.webp","gridImageLink":"https://a/i1-grid.webp","wikiLink":"https://wiki/m4","link":"https://tarkov.dev/item/m4","lastScan":"2026-11-13T02:48:02.000Z","sellToTrader":[{"trader":"t1","price":7358,"priceRUB":7358},{"trader":"t2","price":54,"priceRUB":6622}]},"i2":{"normalizedName":"slick-body-armor","basePrice":100000,"lastLowPrice":null,"avg24hPrice":null},"i3":{"normalizedName":"","basePrice":1}}}}"#;

    const TRADERS: &str = r#"{"data":{"t1":{"name":"t1 Nickname","normalizedName":"prapor"},"t2":{"name":"t2 Nickname","normalizedName":"peacekeeper"}}}"#;

    fn pack() -> ItemLangPack {
        let mut p = ItemLangPack::new();
        p.insert("i1 Name".to_string(), "柯尔特 M4A1".to_string());
        p.insert("t1 Nickname".to_string(), "Prapor".to_string());
        p
    }

    #[test]
    fn parses_every_price_field_and_the_best_trader() {
        let traders = parse_trader_names(TRADERS, &pack());
        let items = parse_items(ITEMS, &pack(), &traders).unwrap();
        assert_eq!(items.len(), 2, "没有 slug 的条目没法被检索到: {items:?}");
        let m4 = &items[0];
        assert_eq!(m4.name_zh.as_deref(), Some("柯尔特 M4A1"));
        assert_eq!(m4.change_48h, Some(215));
        assert_eq!(m4.change_48h_percent, Some(0.4));
        assert_eq!(m4.offer_count, Some(20));
        assert_eq!(m4.min_level_flea, 25);
        assert_eq!((m4.grid_w, m4.grid_h), (1, 2));
        assert_eq!(m4.image_url.as_deref(), Some("https://a/i1-grid.webp"));
        // 最高收购价按 RUB 比：7358 > 6622（后者是美元报价换算来的）。
        assert_eq!(m4.sell_trader_price, Some(7358));
        assert_eq!(m4.sell_trader_name.as_deref(), Some("Prapor"));
        assert_eq!(m4.last_scan, Some(1_794_538_082));
        // 每格价值：占 1×2 格。
        assert_eq!(m4.value_per_slot(), Some(53707 / 2));
    }

    #[test]
    fn missing_flea_prices_stay_none() {
        let items = parse_items(ITEMS, &ItemLangPack::new(), &HashMap::new()).unwrap();
        let slick = &items[1];
        assert_eq!(slick.flea_price(), None, "缺价不能变成 0");
        assert_eq!(slick.base_price, 100_000, "商人价仍在");
        assert_eq!(slick.value_per_slot(), None, "没有价就没有每格价值");
        // 没有语言包时中文名缺失，显示退回 slug。
        assert_eq!(slick.display_name(), "slick-body-armor");
    }

    #[test]
    fn trader_name_falls_back_to_the_slug() {
        let traders = parse_trader_names(TRADERS, &ItemLangPack::new());
        assert_eq!(traders.get("t1").map(String::as_str), Some("prapor"));
    }

    #[test]
    fn parses_iso_timestamps_without_a_date_crate() {
        assert_eq!(parse_iso8601_utc("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_iso8601_utc("2026-11-13T02:48:02.000Z"), Some(1_794_538_082));
        assert!(parse_iso8601_utc("2024-02-29T00:00:00Z").is_some(), "闰年 2 月 29 日有效");
        assert_eq!(parse_iso8601_utc(""), None);
        assert_eq!(parse_iso8601_utc("不是时间"), None);
        assert_eq!(parse_iso8601_utc("2026-13-01T00:00:00Z"), None, "13 月不合法");
    }

    #[test]
    fn broken_input_is_an_error_not_a_panic() {
        assert!(parse_items("不是 JSON", &ItemLangPack::new(), &HashMap::new()).is_err());
        assert!(parse_items(r#"{"data":null}"#, &ItemLangPack::new(), &HashMap::new()).is_err());
    }

    /// 启动回填的判据：按「上次**成功**刷新的时间戳」算，而不是固定节拍。
    ///
    /// 这条逻辑值得一条测试：它同时决定「新库立刻可用」与「重启不重复下载 21 MB」。
    #[tokio::test]
    async fn startup_backfills_only_when_stale() {
        let dir = std::env::temp_dir().join(format!("qqbot-tarkov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(
            qqbot_store::open_resource_store(dir.join("qqbot.db"))
                .await
                .expect("打开临时库"),
        );
        let cfg = TarkovDataConfig {
            store: Some(Arc::clone(&store)),
            refresh_hours: 1,
            ..Default::default()
        };
        let data = TarkovData::new(cfg.clone(), reqwest::Client::new());

        // 从未成功过（新库 / 表刚被重建）→ 立刻回填。
        assert!(data.last_success().await.is_none());
        assert!(data.wait_before_next().await.is_zero(), "新库应当立刻回填");

        // 刚刷过 → 等几乎一整个周期。
        store
            .set_setting(REFRESH_SETTING_KEY, &now_unix().to_string())
            .await
            .unwrap();
        let wait = data.wait_before_next().await;
        assert!(wait > Duration::from_secs(3500), "刚刷过就该等一小时，实际 {wait:?}");

        // 明确过期（两小时前）→ 立刻回填。
        store
            .set_setting(REFRESH_SETTING_KEY, &(now_unix() - 7200).to_string())
            .await
            .unwrap();
        assert!(data.wait_before_next().await.is_zero(), "超过周期就要立刻回填");

        // 关掉自动刷新：不起任务，也不会去下载。
        let off = TarkovData::new(
            TarkovDataConfig { refresh_hours: 0, ..cfg },
            reqwest::Client::new(),
        );
        assert!(off.spawn_hourly().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn interval_is_disabled_by_zero_hours() {
        let mut cfg = TarkovDataConfig::default();
        assert_eq!(cfg.refresh_hours, 0, "库默认不联网");
        assert!(TarkovData::new(cfg.clone(), reqwest::Client::new()).interval().is_none());
        cfg.refresh_hours = 2;
        assert_eq!(
            TarkovData::new(cfg, reqwest::Client::new()).interval(),
            Some(Duration::from_secs(7200))
        );
    }

    /// 实跑验证：七个端点真的是这个形状。默认 `#[ignore]`，手动跑：
    ///
    /// ```text
    /// cargo test -p qqbot-plugins --lib -- --ignored live_tarkov_refresh
    /// ```
    ///
    /// mock 只能证明「按我的理解能解析」，证明不了上游没变。这条测试真的下载
    /// 21 MB 并完整跑一遍解析（不写库），是这套刷新链路唯一的实弹演习。
    #[tokio::test]
    #[ignore = "需要网络，约 21 MB"]
    async fn live_tarkov_refresh_parses_everything() {
        let client = crate::http::build_bulk_client().expect("建 client");
        let data = TarkovData::new(TarkovDataConfig::default(), client);
        let fetched = data.fetch_all().await.expect("下载失败");

        let items_pack = parse_item_lang(&fetched.items_zh).expect("物品语言包");
        let traders_pack = parse_lang_pack(&fetched.traders_zh).expect("商人语言包");
        let traders = parse_trader_names(&fetched.traders, &traders_pack);
        let items = parse_items(&fetched.items, &items_pack, &traders).expect("物品解析");
        assert!(items.len() > 5000, "应当有五千件以上，实际 {}", items.len());
        let priced = items.iter().filter(|i| i.flea_price().is_some()).count();
        assert!(priced > 3000, "应当有三千件以上带跳蚤价，实际 {priced}");
        let with_trader = items.iter().filter(|i| i.sell_trader_price.is_some()).count();
        assert!(with_trader > 4000, "绝大多数物品都有商人收购价，实际 {with_trader}");
        assert!(
            items.iter().all(|i| i.image_url.is_some()),
            "每件都该有格子图（详情页要用）"
        );

        let packs = LangPacks {
            tasks: parse_lang_pack(&fetched.tasks_zh).expect("任务语言包"),
            items: items_pack.clone(),
            traders: traders_pack,
            maps: parse_lang_pack(&fetched.maps_zh).expect("地图语言包"),
        };
        let (tasks, coverage) =
            parse_tasks(&fetched.tasks, &fetched.traders, &packs).expect("任务解析");
        assert!(tasks.len() > 400, "应当有四百条以上任务，实际 {}", tasks.len());
        assert!(coverage.ratio() > 0.9, "中文覆盖率不该低到离谱: {}", coverage.summary());

        let ammo = parse_ammo(&fetched.items, &items_pack).expect("弹药解析");
        // 实测 2026-11：200 发（含手雷以外的全部 ItemPropertiesAmmo）。
        assert!(ammo.len() >= 150, "应当有一百五十发以上弹药，实际 {}", ammo.len());
    }
}
