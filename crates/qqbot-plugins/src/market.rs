//! 塔科夫跳蚤市场（B2 / B3）。
//!
//! 数据来自 `json.tarkov.dev/regular/items` —— 与 B4/B5/B6/B7 同一份静态 JSON。
//!
//! **B2 与 B3 合并成一条命令**：源项目里 B2 走 `tarkov-market.com`（模糊搜索），
//! B3 走 `api.tarkov.dev` GraphQL（按 id 精确查），两者都只是「查跳蚤价格」。
//! 而 tarkov-market.com 已经死了（403 + 加密载荷），GraphQL 后端也挂着。
//! 所以这里用同一张表：参数是 **24 位十六进制 id** 就给详情，否则当关键词搜。
//!
//! ⚠️ 名字是英文 slug 而不是中文，原因同 `ammo.rs`。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use qqbot_store::{ResourceStore, TarkovItem};
use serde::Deserialize;
use serde_json::json;

use crate::ammo::query_tokens;

/// 一次最多列几件。
const MAX_ROWS: usize = 12;

/// 跳蚤市场插件配置。
#[derive(Clone)]
pub struct MarketConfig {
    pub items_url: String,
    pub store: Option<Arc<ResourceStore>>,
}

impl Default for MarketConfig {
    fn default() -> Self {
        Self { items_url: "https://json.tarkov.dev/regular/items".into(), store: None }
    }
}

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
    // 下面四个都可能缺 —— 实测 5442 件里只有 3525 件有跳蚤价格。
    #[serde(default, rename = "lastLowPrice")]
    last_low_price: Option<i64>,
    #[serde(default, rename = "avg24hPrice")]
    avg24h_price: Option<i64>,
    #[serde(default, rename = "low24hPrice")]
    low24h_price: Option<i64>,
    #[serde(default, rename = "high24hPrice")]
    high24h_price: Option<i64>,
    #[serde(default)]
    weight: f64,
}

/// 解析物品。没有 slug 的条目会被丢掉 —— 它们没法被检索到。
pub fn parse_items(body: &str) -> Result<Vec<TarkovItem>, String> {
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
            Some(TarkovItem {
                id,
                normalized_name,
                base_price: item.base_price,
                last_low_price: item.last_low_price,
                avg24h_price: item.avg24h_price,
                low24h_price: item.low24h_price,
                high24h_price: item.high24h_price,
                weight: item.weight,
            })
        })
        .collect();

    out.sort_by(|a, b| a.normalized_name.cmp(&b.normalized_name));
    Ok(out)
}

/// 是不是游戏内物品 id（24 位十六进制）。
///
/// 用它区分「查详情」与「搜关键词」，而不是让用户多记一条命令。
pub fn is_item_id(raw: &str) -> bool {
    raw.len() == 24 && raw.chars().all(|c| c.is_ascii_hexdigit())
}

/// 价格写成「万」，否则六位数看不过来。
pub fn format_price(value: i64) -> String {
    if value.abs() >= 10_000 {
        format!("{:.2}万", value as f64 / 10_000.0)
    } else {
        value.to_string()
    }
}

/// 一行价格摘要，搜索列表与详情共用。
fn price_summary(item: &TarkovItem) -> String {
    match item.avg24h_price.or(item.last_low_price) {
        Some(flea) => {
            if item.base_price > 0 {
                format!("跳蚤 {} · 商人 {}", format_price(flea), format_price(item.base_price))
            } else {
                format!("跳蚤 {}", format_price(flea))
            }
        }
        // 说清楚「没有」而不是显示 0 —— 0 会被当成「不值钱」。
        None => "无跳蚤数据".to_string(),
    }
}

/// 搜索结果的行。
pub fn format_rows(items: &[TarkovItem]) -> Vec<serde_json::Value> {
    items
        .iter()
        .map(|it| json!({ "label": it.normalized_name, "value": price_summary(it) }))
        .collect()
}

/// 详情视图的行。
pub fn detail_rows(item: &TarkovItem) -> Vec<serde_json::Value> {
    let opt = |v: Option<i64>| v.map_or("-".to_string(), format_price);
    let mut rows = vec![
        json!({ "label": "物品", "value": item.normalized_name }),
        json!({ "label": "商人基础价", "value": format_price(item.base_price) }),
        json!({ "label": "最低挂牌", "value": opt(item.last_low_price) }),
        json!({ "label": "24 小时均价", "value": opt(item.avg24h_price) }),
        json!({
            "label": "24 小时区间",
            "value": match (item.low24h_price, item.high24h_price) {
                (Some(lo), Some(hi)) => format!("{} ~ {}", format_price(lo), format_price(hi)),
                _ => "-".to_string(),
            },
        }),
    ];
    if item.weight > 0.0 {
        rows.push(json!({ "label": "重量", "value": format!("{:.2} kg", item.weight) }));
    }
    rows
}

/// `Clone` 是必需的：同一个实例要挂到两条命令路由上。
#[derive(Clone)]
pub struct MarketPlugin {
    config: MarketConfig,
    http: reqwest::Client,
}

impl MarketPlugin {
    pub fn new(config: MarketConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    /// `更新物品`：整表重导。
    async fn reload(&self, ctx: &Ctx) -> Handled {
        if !ctx.is_group() || !ctx.is_admin() {
            let _ = ctx.reply_text("只有群管理员能更新物品数据").await;
            return Handled::Consumed;
        }
        let Some(store) = &self.config.store else {
            let _ = ctx.reply_text("未启用持久化，跳蚤功能不可用").await;
            return Handled::Consumed;
        };

        let body = match self.fetch().await {
            Ok(body) => body,
            Err(reason) => {
                tracing::warn!(error = %reason, "下载物品数据失败");
                let _ = ctx.reply_text(format!("下载失败：{reason}")).await;
                return Handled::Consumed;
            }
        };
        let items = match parse_items(&body) {
            Ok(items) if !items.is_empty() => items,
            Ok(_) => {
                let _ = ctx.reply_text("解析出 0 件物品，上游格式可能变了").await;
                return Handled::Consumed;
            }
            Err(reason) => {
                tracing::warn!(error = %reason, "解析物品数据失败");
                let _ = ctx.reply_text(format!("解析失败：{reason}")).await;
                return Handled::Consumed;
            }
        };

        match store.replace_items(items).await {
            Ok(count) => {
                let _ = ctx.reply_text(format!("物品数据已更新，共 {count} 条")).await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "写入物品数据失败");
                let _ = ctx.reply_text("写入失败，稍后再试").await;
            }
        }
        Handled::Consumed
    }

    /// `跳蚤 <24位id|关键词>`。
    async fn query(&self, ctx: &Ctx) -> Handled {
        let raw = ctx.content().trim().strip_prefix("跳蚤").unwrap_or("").trim();
        if raw.is_empty() {
            let _ = ctx.reply_text("用法：跳蚤 <名称片段>，或 跳蚤 <24位物品id>").await;
            return Handled::Consumed;
        }
        let Some(store) = &self.config.store else {
            let _ = ctx.reply_text("未启用持久化，跳蚤功能不可用").await;
            return Handled::Consumed;
        };

        // 24 位十六进制 → 详情视图；否则当关键词搜。
        let (title, rows) = if is_item_id(raw) {
            let found = match store.get_item(raw).await {
                Ok(found) => found,
                Err(err) => {
                    tracing::warn!(error = %err, "按 id 取物品失败");
                    let _ = ctx.reply_text("查询失败，稍后再试").await;
                    return Handled::Consumed;
                }
            };
            match found {
                Some(item) => (format!("跳蚤 · {}", item.normalized_name), detail_rows(&item)),
                None => {
                    let _ = ctx.reply_text(self.empty_hint(store, "没有这个 id 对应的物品").await).await;
                    return Handled::Consumed;
                }
            }
        } else {
            let tokens = query_tokens(raw);
            let items = match store.search_items(tokens, MAX_ROWS).await {
                Ok(items) => items,
                Err(err) => {
                    tracing::warn!(error = %err, "检索物品失败");
                    let _ = ctx.reply_text("查询失败，稍后再试").await;
                    return Handled::Consumed;
                }
            };
            if items.is_empty() {
                let _ = ctx.reply_text(self.empty_hint(store, "没有匹配的物品").await).await;
                return Handled::Consumed;
            }
            (format!("跳蚤 · {raw}"), format_rows(&items))
        };

        let height = 156 + rows.len() * 48 + 40;
        let data = json!({
            "title": title,
            "rows": rows,
            "width": 1000,
            "height": height,
            "footer": "价格单位：卢布 · 数据来自 tarkov.dev",
        });
        if let Err(err) = ctx.reply_template("card.svg", data).await {
            tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "跳蚤卡片发送失败");
            let _ = ctx.reply_text(format!("卡片生成失败：{err}")).await;
        }
        Handled::Consumed
    }

    /// 查不到时的话术。空表与「关键词不对」要给不同的话。
    async fn empty_hint(&self, store: &ResourceStore, miss: &str) -> String {
        match store.item_count().await {
            Ok(0) => "物品数据还没导入，请管理员发「更新物品」".to_string(),
            _ => format!("{miss}，换个关键词试试"),
        }
    }

    async fn fetch(&self) -> Result<String, String> {
        let res = self
            .http
            .get(&self.config.items_url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) qqbot-rs")
            .send()
            .await
            .map_err(|err| format!("请求失败：{err}"))?;
        let status = res.status();
        if !status.is_success() {
            return Err(format!("接口返回 HTTP {status}"));
        }
        res.text().await.map_err(|err| format!("读取响应失败：{err}"))
    }
}

#[async_trait]
impl Handler for MarketPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        match ctx.content().split_whitespace().next().unwrap_or_default() {
            "更新物品" => self.reload(ctx).await,
            "跳蚤" => self.query(ctx).await,
            _ => Handled::Next,
        }
    }

    fn name(&self) -> &'static str {
        "塔科夫跳蚤市场"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(
        r#"{"data":{"items":{"#,
        r#""i1":{"normalizedName":"colt-m4a1-556x45-assault-rifle","basePrice":18397,"lastLowPrice":23932,"avg24hPrice":93642,"low24hPrice":20000,"high24hPrice":185000,"weight":3.4},"#,
        r#""i2":{"normalizedName":"slick-body-armor","basePrice":100000,"lastLowPrice":null,"avg24hPrice":null},"#,
        r#""i3":{"normalizedName":"","basePrice":1}"#,
        r#"}}}"#,
    );

    #[test]
    fn keeps_items_with_a_slug_and_sorts() {
        let items = parse_items(FIXTURE).unwrap();
        assert_eq!(items.len(), 2, "没有 slug 的条目没法被检索到: {items:?}");
        assert_eq!(items[0].normalized_name, "colt-m4a1-556x45-assault-rifle");
        assert_eq!(items[1].normalized_name, "slick-body-armor");
    }

    #[test]
    fn missing_flea_prices_stay_none() {
        // 关键：缺价不能变成 0，否则会显示成「不值钱」。
        let items = parse_items(FIXTURE).unwrap();
        let slick = &items[1];
        assert_eq!(slick.last_low_price, None);
        assert_eq!(slick.avg24h_price, None);
        assert_eq!(slick.base_price, 100_000, "商人价仍在");
    }

    #[test]
    fn maps_the_price_fields() {
        let items = parse_items(FIXTURE).unwrap();
        let m4 = &items[0];
        assert_eq!(m4.base_price, 18397);
        assert_eq!(m4.last_low_price, Some(23932));
        assert_eq!(m4.avg24h_price, Some(93642));
        assert_eq!(m4.low24h_price, Some(20000));
        assert_eq!(m4.high24h_price, Some(185000));
    }

    /// 实跑验证：打**真实**接口。
    ///
    /// ```text
    /// cargo test -p qqbot-plugins --lib -- --ignored live_items
    /// ```
    #[tokio::test]
    #[ignore = "需要网络"]
    async fn live_items_endpoint_parses() {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap();
        let body = http
            .get("https://json.tarkov.dev/regular/items")
            .header("User-Agent", "Mozilla/5.0 qqbot-rs")
            .send()
            .await
            .expect("请求失败")
            .text()
            .await
            .expect("读取失败");
        let items = parse_items(&body).expect("解析失败");
        assert!(items.len() > 5000, "应当解析出五千件以上物品，实际 {}", items.len());
        let priced = items.iter().filter(|i| i.avg24h_price.is_some()).count();
        assert!(priced > 3000, "应当有三千件以上带跳蚤价，实际 {priced}");
        // 有价的那件必须能按 id 取回来 —— 这正是 B3 走的路。
        let sample = items.iter().find(|i| i.avg24h_price.is_some()).unwrap();
        assert!(is_item_id(&sample.id), "id 应当是 24 位十六进制: {}", sample.id);
    }

    #[test]
    fn broken_input_is_an_error_not_a_panic() {
        assert!(parse_items("不是 JSON").is_err());
        assert!(parse_items(r#"{"data":null}"#).is_err());
    }

    #[test]
    fn recognizes_item_ids() {
        assert!(is_item_id("5447a9cd4bdc2dbd208b4567"));
        assert!(!is_item_id("5447a9cd4bdc2dbd208b456"), "少一位不算");
        assert!(!is_item_id("5447a9cd4bdc2dbd208b45678"), "多一位不算");
        assert!(!is_item_id("slick-body-armor"), "slug 不算");
        assert!(!is_item_id("5447a9cd4bdc2dbd208b456g"), "非十六进制不算");
    }

    #[test]
    fn prices_switch_to_wan_at_five_digits() {
        assert_eq!(format_price(9999), "9999");
        assert_eq!(format_price(10_000), "1.00万");
        assert_eq!(format_price(93_642), "9.36万");
    }

    #[test]
    fn search_rows_say_when_there_is_no_flea_data() {
        let items = parse_items(FIXTURE).unwrap();
        let rows = format_rows(&items);
        assert_eq!(rows[0]["value"], "跳蚤 9.36万 · 商人 1.84万", "均价优先于最低挂牌");
        assert_eq!(rows[1]["value"], "无跳蚤数据", "缺价要说清楚，不能显示 0");
    }

    #[test]
    fn detail_rows_cover_every_field() {
        let items = parse_items(FIXTURE).unwrap();
        let rows = detail_rows(&items[0]);
        let labels: Vec<&str> = rows.iter().map(|r| r["label"].as_str().unwrap()).collect();
        assert_eq!(
            labels,
            vec!["物品", "商人基础价", "最低挂牌", "24 小时均价", "24 小时区间", "重量"]
        );
        assert_eq!(rows[4]["value"], "2.00万 ~ 18.50万");
        assert_eq!(rows[5]["value"], "3.40 kg");
    }

    #[test]
    fn detail_rows_use_a_dash_for_missing_values() {
        let items = parse_items(FIXTURE).unwrap();
        let rows = detail_rows(&items[1]);
        assert_eq!(rows[2]["value"], "-");
        assert_eq!(rows[4]["value"], "-", "只有一边有价也算缺");
        // 重量为 0 时整行不出现，而不是显示 0.00 kg。
        assert!(rows.iter().all(|r| r["label"] != "重量"));
    }
}
