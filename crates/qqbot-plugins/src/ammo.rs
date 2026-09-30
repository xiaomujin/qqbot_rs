//! 塔科夫弹药查询（B6）。
//!
//! 数据来自 `json.tarkov.dev/regular/items` —— **静态 JSON**，与 GraphQL 是同一份
//! 数据源，但 GraphQL 后端挂掉时它照样能用（2026-09 实测）。
//!
//! ⚠️ **名字是英文 slug 而不是中文。** 静态 JSON 里的 `name` 是翻译键
//! （形如 `54527a984bdc2d4e668b4567 Name`），只有 GraphQL 的 `lang: zh` 会解析它。
//! 所以这里用 `normalizedName`（`556x45mm-m855`）做检索与显示 ——
//! 玩家打的 `m855` / `bp` / `5.45` 都能命中，但界面不是中文。
//! 等 GraphQL 恢复或拿到语言包，**只需改这一处的名称来源**。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use qqbot_store::{Ammo, ResourceStore};
use serde::Deserialize;
use serde_json::json;

/// 数据源。
const ITEMS_URL: &str = "https://json.tarkov.dev/regular/items";
/// 一次最多列几发。
const MAX_ROWS: usize = 12;

/// 弹药插件配置。
// 不派生 Debug：ResourceStore 没有有意义的 Debug 表示。
#[derive(Clone)]
pub struct AmmoConfig {
    /// 静态 JSON 地址。
    pub items_url: String,
    /// 弹药表。`None` 表示未启用持久化。
    pub store: Option<Arc<ResourceStore>>,
}

impl Default for AmmoConfig {
    fn default() -> Self {
        Self { items_url: ITEMS_URL.into(), store: None }
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
    #[serde(default)]
    properties: Option<AmmoProperties>,
    #[serde(default, rename = "basePrice")]
    base_price: i64,
}

#[derive(Debug, Deserialize)]
struct AmmoProperties {
    #[serde(default, rename = "propertiesType")]
    properties_type: String,
    #[serde(default)]
    caliber: String,
    #[serde(default)]
    damage: i64,
    #[serde(default, rename = "penetrationPower")]
    penetration_power: i64,
    #[serde(default, rename = "armorDamage")]
    armor_damage: i64,
    #[serde(default, rename = "fragmentationChance")]
    fragmentation_chance: f64,
    #[serde(default, rename = "initialSpeed")]
    initial_speed: i64,
    #[serde(default, rename = "projectileCount")]
    projectile_count: i64,
    #[serde(default)]
    tracer: bool,
}

/// 从静态 JSON 里挑出弹药。
///
/// 判据用 `propertiesType == ItemPropertiesAmmo` 而不是 `types` 里含 `ammo` ——
/// 后者会把**手雷**也算进来（实测 `types: ["ammo","grenade"]`）。
pub fn parse_ammo(body: &str) -> Result<Vec<Ammo>, String> {
    let parsed: ItemsResponse =
        serde_json::from_str(body).map_err(|err| format!("解析响应失败：{err}"))?;
    let items = parsed.data.ok_or("响应缺少 data")?.items;

    let mut out: Vec<Ammo> = items
        .into_iter()
        .filter_map(|(id, item)| {
            let props = item.properties?;
            if props.properties_type != "ItemPropertiesAmmo" {
                return None;
            }
            let normalized_name = item.normalized_name.trim().to_string();
            if normalized_name.is_empty() {
                return None;
            }
            Some(Ammo {
                id,
                normalized_name,
                caliber: props.caliber,
                damage: props.damage,
                penetration_power: props.penetration_power,
                armor_damage: props.armor_damage,
                fragmentation_chance: props.fragmentation_chance,
                initial_speed: props.initial_speed,
                projectile_count: props.projectile_count.max(1),
                tracer: props.tracer,
                base_price: item.base_price,
            })
        })
        .collect();

    // 排序让输出稳定：HashMap 的迭代顺序随机，不排的话同一份数据两次导入结果不同。
    out.sort_by(|a, b| a.normalized_name.cmp(&b.normalized_name));
    Ok(out)
}

/// 把用户输入切成归一化片段。
///
/// 去掉点、连字符、空格并转小写：`5.45 BP` → `["545", "bp"]`。
/// 检索时要求**全部命中**，所以多打一个词只会缩小范围，不会引入误报。
pub fn query_tokens(raw: &str) -> Vec<String> {
    raw.split_whitespace()
        .map(|part| {
            part.chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|token| !token.is_empty())
        .collect()
}


/// 卡片行：左边名称，右边三项关键数值。
pub fn format_rows(items: &[Ammo]) -> Vec<serde_json::Value> {
    items
        .iter()
        .map(|a| {
            json!({
                "label": a.normalized_name,
                "value": format!("伤{} 穿{} 甲伤{}", a.damage, a.penetration_power, a.armor_damage),
            })
        })
        .collect()
}

/// `Clone` 是必需的：同一个实例要挂到两条命令路由上。
#[derive(Clone)]
pub struct AmmoPlugin {
    config: AmmoConfig,
    http: reqwest::Client,
}

impl AmmoPlugin {
    pub fn new(config: AmmoConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    /// `更新子弹`：整表重导。
    async fn reload(&self, ctx: &Ctx) -> Handled {
        if !ctx.is_group() || !ctx.is_admin() {
            let _ = ctx.reply_text("只有群管理员能更新弹药数据").await;
            return Handled::Consumed;
        }
        let Some(store) = &self.config.store else {
            let _ = ctx.reply_text("未启用持久化，弹药功能不可用").await;
            return Handled::Consumed;
        };

        let body = match self.fetch().await {
            Ok(body) => body,
            Err(reason) => {
                tracing::warn!(error = %reason, "下载弹药数据失败");
                let _ = ctx.reply_text(format!("下载失败：{reason}")).await;
                return Handled::Consumed;
            }
        };
        let items = match parse_ammo(&body) {
            Ok(items) if !items.is_empty() => items,
            Ok(_) => {
                let _ = ctx.reply_text("解析出 0 条弹药，上游格式可能变了").await;
                return Handled::Consumed;
            }
            Err(reason) => {
                tracing::warn!(error = %reason, "解析弹药数据失败");
                let _ = ctx.reply_text(format!("解析失败：{reason}")).await;
                return Handled::Consumed;
            }
        };

        match store.replace_ammo(items).await {
            Ok(count) => {
                let _ = ctx.reply_text(format!("弹药数据已更新，共 {count} 条")).await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "写入弹药数据失败");
                let _ = ctx.reply_text("写入失败，稍后再试").await;
            }
        }
        Handled::Consumed
    }

    /// `查子弹 <关键词>`。
    async fn search(&self, ctx: &Ctx) -> Handled {
        let query = ctx.content().trim().strip_prefix("查子弹").unwrap_or("").trim();
        let tokens = query_tokens(query);
        if tokens.is_empty() {
            let _ = ctx.reply_text("用法：查子弹 <名称片段>，例如 查子弹 m855").await;
            return Handled::Consumed;
        }
        let Some(store) = &self.config.store else {
            let _ = ctx.reply_text("未启用持久化，弹药功能不可用").await;
            return Handled::Consumed;
        };

        let items = match store.search_ammo(tokens, MAX_ROWS).await {
            Ok(items) => items,
            Err(err) => {
                tracing::warn!(error = %err, "检索弹药失败");
                let _ = ctx.reply_text("查询失败，稍后再试").await;
                return Handled::Consumed;
            }
        };

        if items.is_empty() {
            // 空表与「查不到」要给不同的话：前者是没导入，后者是关键词不对。
            let hint = match store.ammo_count().await {
                Ok(0) => "弹药数据还没导入，请管理员发「更新子弹」",
                _ => "没有匹配的弹药，换个关键词试试",
            };
            let _ = ctx.reply_text(hint).await;
            return Handled::Consumed;
        }

        let rows = format_rows(&items);
        let height = 156 + rows.len() * 48 + 40;
        let data = json!({
            "title": format!("弹药 · {query}"),
            "rows": rows,
            "width": 980,
            "height": height,
            "footer": format!("{} 条结果 · 伤/穿/甲伤", items.len()),
        });
        if let Err(err) = ctx.reply_template("card.svg", data).await {
            tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "弹药卡片发送失败");
            let _ = ctx.reply_text(format!("卡片生成失败：{err}")).await;
        }
        Handled::Consumed
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
impl Handler for AmmoPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        match ctx.content().split_whitespace().next().unwrap_or_default() {
            "更新子弹" => self.reload(ctx).await,
            "查子弹" => self.search(ctx).await,
            _ => Handled::Next,
        }
    }

    fn name(&self) -> &'static str {
        "塔科夫弹药"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 形状照实测的 `json.tarkov.dev/regular/items` 片段。
    ///
    /// 用 `concat!` 逐条拼，而不是一个跨行原始字符串 ——
    /// 后者少写或多写一个花括号时，报错只会说「解析失败」，看不出是哪一层。
    const FIXTURE: &str = concat!(
        r#"{"data":{"items":{"#,
        r#""54527a984bdc2d4e668b4567":{"normalizedName":"556x45mm-m855","basePrice":180,"properties":{"propertiesType":"ItemPropertiesAmmo","caliber":"Caliber556x45NATO","damage":54,"penetrationPower":31,"armorDamage":37,"fragmentationChance":0.5,"initialSpeed":922,"projectileCount":1,"tracer":false}},"#,
        r#""5c0d5e4486f77478390952fe":{"normalizedName":"545x39mm-bp","basePrice":110,"properties":{"propertiesType":"ItemPropertiesAmmo","caliber":"Caliber545x39","damage":51,"penetrationPower":37,"armorDamage":42,"fragmentationChance":0.16,"initialSpeed":890,"projectileCount":1,"tracer":false}},"#,
        r#""grenade":{"normalizedName":"f-1-grenade","basePrice":100,"properties":{"propertiesType":"ItemPropertiesGrenade","damage":80}},"#,
        r#""noprops":{"normalizedName":"no-props","basePrice":1},"#,
        r#""noslug":{"normalizedName":"","properties":{"propertiesType":"ItemPropertiesAmmo"}}"#,
        r#"}}}"#,
    );

    #[test]
    fn keeps_only_real_ammo() {
        let items = parse_ammo(FIXTURE).unwrap();
        // 手雷的 types 里也有 ammo，但 propertiesType 不是，必须排除。
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].normalized_name, "545x39mm-bp", "应当按名称排序");
        assert_eq!(items[1].normalized_name, "556x45mm-m855");
    }

    #[test]
    fn maps_the_ballistic_fields() {
        let items = parse_ammo(FIXTURE).unwrap();
        let bp = &items[0];
        assert_eq!(bp.damage, 51);
        assert_eq!(bp.penetration_power, 37);
        assert_eq!(bp.armor_damage, 42);
        assert_eq!(bp.initial_speed, 890);
        assert_eq!(bp.caliber, "Caliber545x39");
        assert!(!bp.tracer);
        assert_eq!(bp.base_price, 110);
    }

    #[test]
    fn projectile_count_defaults_to_one() {
        // 上游缺字段时是 0，显示成「0 发」会很怪。
        let items = parse_ammo(FIXTURE).unwrap();
        assert_eq!(items[0].projectile_count, 1);
    }

    #[test]
    fn broken_input_is_an_error_not_a_panic() {
        assert!(parse_ammo("不是 JSON").is_err());
        assert!(parse_ammo(r#"{"data":null}"#).is_err());
    }

    #[test]
    fn normalizes_the_query() {
        assert_eq!(query_tokens("5.45 BP"), vec!["545", "bp"]);
        assert_eq!(query_tokens("m855"), vec!["m855"]);
        assert_eq!(query_tokens("   "), Vec::<String>::new());
        assert_eq!(query_tokens("545x39mm-bp"), vec!["545x39mmbp"]);
    }

    #[test]
    fn query_tokens_match_the_stored_slug() {
        // 这是检索能用的关键：用户打的片段要真的落在 normalizedName 里。
        let slug = "545x39mm-bp";
        for raw in ["5.45 bp", "545 bp", "BP", "545x39"] {
            let tokens = query_tokens(raw);
            assert!(
                tokens.iter().all(|t| slug.contains(t.as_str())),
                "{raw} → {tokens:?} 应当全部命中 {slug}"
            );
        }
    }

}
