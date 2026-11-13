//! 塔科夫弹药查询（B6）。
//!
//! 数据来自 `json.tarkov.dev/regular/items` —— **静态 JSON**，与 GraphQL 是同一份
//! 数据源，但 GraphQL 后端挂掉时它照样能用（2026-09 实测）。
//!
//! 本模块**只读**：下载与写库全在 `tarkov_data`（一次刷新同时刷弹药 / 物品 / 任务）。
//! 这里保留 `parse_ammo` / `parse_item_lang` / `item_name`，因为刷新器要用它们。
//!
//! 中文名来自 `regular/items_zh` 语言包（键是 `<物品 id> Name`）——
//! 静态 JSON 里的 `name` 是翻译键，语言包才是它的字典，不再依赖 GraphQL。
//! 检索同时匹配 `normalizedName` 与中文名：玩家打 `m855`、`bp`、`5.45`、`穿甲` 都能命中。
//!
//! # 为什么是 Markdown + 列表 / 详情
//!
//! 与跳蚤市场同一套版式（`market.rs`）：查子弹是为了**比**几发弹，所以先给列表、
//! 列表里每发都能点进详情。而群聊里唯一可点的是 `qqbot-cmd-input`（插入输入框），
//! 它只能出现在 `msg_type=2` 的 Markdown 里 —— 富媒体卡片的 `content` 是纯文本，
//! 塞不进这种标签，两者只能二选一。于是整个插件从卡片改成 Markdown。
//!
//! 弹道数据只在本表（`ammo`），而**价格 / 图片 / 链接在物品表（`tarkov_item`）**，
//! 同一个 id 一行；详情视图按 id 各取一次，缺了物品行就只印弹道。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use qqbot_store::{Ammo, ResourceStore, TarkovItem};
use serde::Deserialize;

use crate::md::{
    BR, image_block, is_item_id, labelled, link_line, md_safe, price_lines, scan_footer,
    trader_line,
};
use crate::task::cmd_input;

/// 一次最多列几发。
///
/// 十条照抄跳蚤列表：两行一发、十条一屏多一点；再多用户也不会读完，
/// 而消息长度与「补个关键词」的成本都在涨。
const LIST_LIMIT: usize = 10;

/// 弹药插件配置。
// 不派生 Debug：ResourceStore 没有有意义的 Debug 表示。
#[derive(Clone, Default)]
pub struct AmmoConfig {
    /// 弹药表。`None` 表示未启用持久化。
    ///
    /// 没有 URL 字段：数据由 `tarkov_data` 的刷新器写入，这个插件只读。
    pub store: Option<Arc<ResourceStore>>,
}

/// 物品名语言包：`<id> Name` → 中文。跳蚤市场也用它。
pub type ItemLangPack = HashMap<String, String>;

/// 语言包响应：`{"data": {"<物品 id> Name": "中文名"}}`。
///
/// 与 `ItemsResponse` 长得像但**不是一回事** —— 后者的 data 里还套着一层 items。
#[derive(Debug, Default, Deserialize)]
struct ItemLangResponse {
    #[serde(default)]
    data: Option<ItemLangPack>,
}

/// 解析一份物品名语言包。
pub fn parse_item_lang(body: &str) -> Result<ItemLangPack, String> {
    let parsed: ItemLangResponse =
        serde_json::from_str(body).map_err(|err| format!("解析语言包失败：{err}"))?;
    parsed.data.ok_or_else(|| "语言包缺少 data".to_string())
}

/// 取一件物品的中文名。查不到返回 `None`（显示退回 slug）。
pub fn item_name(pack: &ItemLangPack, id: &str) -> Option<String> {
    pack.get(&format!("{id} Name"))
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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
pub fn parse_ammo(body: &str, pack: &ItemLangPack) -> Result<Vec<Ammo>, String> {
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
            let name_zh = item_name(pack, &id);
            Some(Ammo {
                id,
                normalized_name,
                name_zh,
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
///
/// ⚠️ 用 `is_alphanumeric` 而不是 `is_ascii_alphanumeric` ——
/// 后者会把**中文整个滤掉**，于是 `查任务 彻夜难眠` 变成空 token，
/// 用户收到的是「用法：查任务 <名称片段>」，好像他没打参数一样。
/// 上游数据确实只有英文 slug，但那是**查不到**，不是**用错**，
/// 两者的提示必须不一样。
pub fn query_tokens(raw: &str) -> Vec<String> {
    raw.split_whitespace()
        .map(|part| {
            part.chars()
                .filter(|c: &char| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|token| !token.is_empty())
        .collect()
}

// ---- 排版 ----

/// 列表第二行：`伤害51｜穿透37｜甲伤42｜<详情>`。
///
/// 用**全称**而不是缩写：三段全称加上「详情」正好 28 个显示单位，
/// 仍在手机行宽（30）以内；`伤51｜穿37` 省下的两个单位换不来可读性。
fn summary(a: &Ammo) -> String {
    format!(
        "伤害{}｜穿透{}｜甲伤{}｜{}",
        a.damage,
        a.penetration_power,
        a.armor_damage,
        // 可点文本：点一下只是把「查子弹 <id>」插进输入框，用户还得按发送
        // （群聊不支持 qqbot-cmd-enter，见协议文档 §12.1）。
        cmd_input(&format!("查子弹 {}", a.id), "详情")
    )
}

/// 检索结果（Markdown）。
///
/// 每发两行：`- **名称**` + 缩进的 `伤害…｜穿透…｜甲伤…｜详情`，
/// 与跳蚤列表刻意保持一致（同一套「一件两行 + 可点详情」的习惯）。
pub fn render_list(items: &[Ammo], total: i64) -> String {
    let mut out = String::new();
    // 只有「被截断」时才提示条数：没截断还写一行，纯属噪音。
    if total > items.len() as i64 {
        let _ = writeln!(
            out,
            "匹配 {total} 发，显示前 {} 发。补个关键词能缩小范围。",
            items.len()
        );
        out.push('\n');
    }
    for a in items {
        let _ = writeln!(out, "- **{}**{BR}", md_safe(a.display_name()));
        let _ = writeln!(out, "  {}{BR}", summary(a));
    }
    out
}

/// 弹道段。伤害 / 穿透 / 甲伤是「查子弹」的全部理由，所以**必定出现**。
fn ballistics_line(a: &Ammo) -> String {
    let values = vec![
        format!("伤害 {}", a.damage),
        format!("穿透 {}", a.penetration_power),
        format!("甲伤 {}", a.armor_damage),
    ];
    labelled("弹道", &values).unwrap_or_default()
}

/// 飞行特性段。初速与碎弹率必定出现；弹丸数与曳光只在有信息量时出现
/// （`projectile_count` 导入时已兜底成 1，写「弹丸 1」是噪音）。
fn flight_line(a: &Ammo) -> String {
    let mut values = vec![
        format!("初速 {} m/s", a.initial_speed),
        format!("碎弹 {:.0}%", a.fragmentation_chance * 100.0),
    ];
    if a.projectile_count > 1 {
        values.push(format!("弹丸 {}", a.projectile_count));
    }
    if a.tracer {
        values.push("曳光".into());
    }
    labelled("飞行", &values).unwrap_or_default()
}

/// 详情页（Markdown）。
///
/// `item` 是同 id 在 `tarkov_item` 里的那一行 —— **价格 / 图片 / wiki 链接只在
/// 那张表里**（弹药表只存弹道）。缺了它（旧数据、上游没给）就只印弹道，
/// 不印空的价格段：一行「跳蚤 均价 -」比不印更糟。
///
/// ⚠️ 上游 `regular/items` 里 **200 发弹药全部没有跳蚤价**（实测 0/200，
/// 而全表 3551/5498 有），所以弹药详情通常只出「商人」与「24h 挂单 N 条」，
/// 跳蚤 / 48h 两段不会出现。这不是漏取字段，是静态 JSON 本来就没有 ——
/// 要跳蚤价得改走 GraphQL，而那正是本模块刻意避开的东西（见文件头）。
pub fn render_detail(ammo: &Ammo, item: Option<&TarkovItem>) -> String {
    let name = md_safe(ammo.display_name());
    let mut blocks: Vec<String> = vec![format!("# {name}")];

    blocks.extend(image_block(&name, item.and_then(|it| it.image_url.as_deref())));
    blocks.push(ballistics_line(ammo));
    blocks.push(flight_line(ammo));

    if let Some(item) = item {
        // 分割线**只在这一处**：线上是性能，线下是钱。
        let mut prices: Vec<String> = Vec::new();
        prices.extend(trader_line(item));
        prices.extend(price_lines(item));
        if !prices.is_empty() {
            blocks.push("***".to_string());
            blocks.push(prices.join("\n"));
        }
        blocks.extend(link_line(item));
        blocks.push(scan_footer(item));
    }

    blocks.join("\n\n")
}

/// 用法提示。空参数与「参数里全是符号」共用同一句。
async fn usage(ctx: &Ctx) {
    let _ = ctx
        .reply
        .markdown("用法：`查子弹 <名称片段>`（可多关键词，空格分隔），或 `查子弹 <24位物品id>`")
        .await;
}

/// 弹药插件：只读本地表。
///
/// 数据由 `tarkov_data::TarkovData` 每小时刷新 —— 这里没有任何 HTTP。
#[derive(Clone)]
pub struct AmmoPlugin {
    config: AmmoConfig,
}

impl AmmoPlugin {
    pub fn new(config: AmmoConfig) -> Self {
        Self { config }
    }

    /// `查子弹<空格可有可无> <24位id | 关键词…>`。
    async fn query(&self, ctx: &Ctx, raw: &str) -> Handled {
        let Some(store) = &self.config.store else {
            let _ = ctx.reply.text("未启用持久化，弹药功能不可用").await;
            return Handled::Consumed;
        };

        // 24 位十六进制 → 详情视图；否则当关键词搜。
        let markdown = if is_item_id(raw) {
            let ammo = match store.get_ammo(raw).await {
                Ok(Some(ammo)) => ammo,
                Ok(None) => {
                    let hint = self.empty_hint(store, "没有这个 id 对应的弹药").await;
                    let _ = ctx.reply.text(hint).await;
                    return Handled::Consumed;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "按 id 取弹药失败");
                    let _ = ctx.reply.text("查询失败，稍后再试").await;
                    return Handled::Consumed;
                }
            };
            // 价格 / 图片 / 链接在物品表里。读不到就退化成纯弹道视图 —— 不是错误。
            let item = store.get_item(raw).await.unwrap_or(None);
            render_detail(&ammo, item.as_ref())
        } else {
            let tokens = query_tokens(raw);
            if tokens.is_empty() {
                usage(ctx).await;
                return Handled::Consumed;
            }
            let items = match store.search_ammo(tokens.clone(), LIST_LIMIT).await {
                Ok(items) => items,
                Err(err) => {
                    tracing::warn!(error = %err, "检索弹药失败");
                    let _ = ctx.reply.text("查询失败，稍后再试").await;
                    return Handled::Consumed;
                }
            };
            if items.is_empty() {
                // 空表与「查不到」要给不同的话：前者是没导入，后者是关键词不对。
                let hint = self.empty_hint(store, "没有匹配的弹药").await;
                let _ = ctx.reply.text(hint).await;
                return Handled::Consumed;
            }
            let total = store.count_ammo(tokens).await.unwrap_or(items.len() as i64);
            render_list(&items, total)
        };

        if let Err(err) = ctx.reply.markdown(markdown).await {
            // 不降级成纯文本摘要：用户已经等了同样久，再发一条更难看的消息没有意义。
            tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "弹药消息发送失败");
            let _ = ctx.reply.text(format!("弹药消息发送失败：{err}")).await;
        }
        Handled::Consumed
    }

    /// 查不到时的话术。空表与「关键词不对」要给不同的话。
    async fn empty_hint(&self, store: &ResourceStore, miss: &str) -> String {
        match store.ammo_count().await {
            Ok(0) => "弹药数据还没导入，请系统控制者发「塔科夫更新数据」".to_string(),
            _ => format!("{miss}，换个关键词试试"),
        }
    }
}

#[async_trait]
impl Handler for AmmoPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        let content = ctx.content().trim();
        // 路由用 `Matcher::Prefix("查子弹")`，所以 `查子弹 m855` 与 `查子弹m855`
        // 都会到这里 —— 差别只在 strip 之后有没有前导空格。
        let Some(rest) = content.strip_prefix("查子弹") else {
            return Handled::Next;
        };
        self.query(ctx, rest.trim()).await
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

    fn ammo_stub() -> Ammo {
        Ammo {
            id: "5c0d5e4486f77478390952fe".into(),
            normalized_name: "545x39mm-bp".into(),
            name_zh: Some("5.45 BP".into()),
            caliber: "Caliber545x39".into(),
            damage: 51,
            penetration_power: 37,
            armor_damage: 42,
            fragmentation_chance: 0.16,
            initial_speed: 890,
            projectile_count: 1,
            tracer: false,
            base_price: 110,
        }
    }

    /// 物品表里同一 id 的那一行：价格 / 图片 / 链接都在那儿。
    fn item_stub() -> TarkovItem {
        TarkovItem {
            id: "5c0d5e4486f77478390952fe".into(),
            normalized_name: "545x39mm-bp".into(),
            name_zh: Some("5.45 BP".into()),
            base_price: 110,
            sell_trader_name: Some("Prapor".into()),
            sell_trader_price: Some(87),
            avg24h_price: Some(12_000),
            last_low_price: Some(11_000),
            image_url: Some("https://assets/bp-grid.webp".into()),
            wiki_link: Some("https://wiki/bp".into()),
            last_scan: Some(1_794_538_082),
            ..Default::default()
        }
    }

    #[test]
    fn keeps_only_real_ammo() {
        let items = parse_ammo(FIXTURE, &ItemLangPack::new()).unwrap();
        // 手雷的 types 里也有 ammo，但 propertiesType 不是，必须排除。
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].normalized_name, "545x39mm-bp", "应当按名称排序");
        assert_eq!(items[1].normalized_name, "556x45mm-m855");
    }

    #[test]
    fn maps_the_ballistic_fields() {
        let items = parse_ammo(FIXTURE, &ItemLangPack::new()).unwrap();
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
        let items = parse_ammo(FIXTURE, &ItemLangPack::new()).unwrap();
        assert_eq!(items[0].projectile_count, 1);
    }

    #[test]
    fn broken_input_is_an_error_not_a_panic() {
        assert!(parse_ammo("不是 JSON", &ItemLangPack::new()).is_err());
        assert!(parse_ammo(r#"{"data":null}"#, &ItemLangPack::new()).is_err());
    }

    #[test]
    fn normalizes_the_query() {
        assert_eq!(query_tokens("5.45 BP"), vec!["545", "bp"]);
        assert_eq!(query_tokens("m855"), vec!["m855"]);
        assert_eq!(query_tokens("   "), Vec::<String>::new());
        assert_eq!(query_tokens("545x39mm-bp"), vec!["545x39mmbp"]);
    }

    #[test]
    fn chinese_is_kept_not_silently_dropped() {
        // 曾经用 is_ascii_alphanumeric，中文被整个滤掉 → 空 token →
        // 用户收到「用法：...」，好像他没打参数。那是 bug，不是限制。
        assert_eq!(query_tokens("彻夜难眠"), vec!["彻夜难眠"]);
        assert_eq!(query_tokens("5.45 弹"), vec!["545", "弹"]);
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

    /// 列表：每发两行，第二行是弹道与可点的「详情」。
    #[test]
    fn list_puts_ballistics_and_clickable_detail() {
        let md = render_list(&[ammo_stub()], 1);
        assert!(md.contains("- **5.45 BP**"), "第一行是名称: {md}");
        assert!(md.contains("伤害51｜穿透37｜甲伤42"), "第二行是弹道: {md}");
        assert!(md.contains("<qqbot-cmd-input "), "详情必须是可点文本: {md}");
        // 点一下插入的是「查子弹 <id>」，urlencode 之后就是这个前缀。
        assert!(
            md.contains("text=\"%E6%9F%A5%E5%AD%90%E5%BC%B9%20"),
            "点一下要插入「查子弹 <id>」: {md}"
        );
        assert!(md.contains('\u{200B}'), "行末要有零宽空格，否则不换行: {md}");
        assert!(!md.contains("匹配"), "没被截断就不该提条数: {md}");
    }

    #[test]
    fn list_says_when_it_truncated() {
        let items: Vec<Ammo> = (0..10)
            .map(|i| {
                let mut a = ammo_stub();
                a.id = format!("id{i}");
                a
            })
            .collect();
        let md = render_list(&items, 137);
        assert!(md.starts_with("匹配 137 发，显示前 10 发"), "要说明被截断: {md}");
        assert!(md.contains("补个关键词"), "{md}");
    }

    /// 详情：图片、弹道、飞行、价格、链接、页脚一个都不少。
    #[test]
    fn detail_shows_ballistics_prices_and_links() {
        let item = item_stub();
        let md = render_detail(&ammo_stub(), Some(&item));
        assert!(md.starts_with("# 5.45 BP\n"), "{md}");
        assert!(
            md.contains("![5.45 BP #96px #96px](https://assets/bp-grid.webp)"),
            "图片来自物品表: {md}"
        );
        assert!(md.contains("**弹道** 伤害 51｜穿透 37｜甲伤 42"), "{md}");
        assert!(md.contains("**飞行** 初速 890 m/s｜碎弹 16%"), "{md}");
        assert!(!md.contains("弹丸 1"), "单发弹不印「弹丸 1」: {md}");
        assert!(md.contains("**商人** 基准 110｜最高收购 87（Prapor）"), "{md}");
        assert!(md.contains("**跳蚤** 均价 1.20万｜最低挂牌 1.10万"), "{md}");
        assert!(md.contains("[wiki](https://wiki/bp)"), "{md}");
        assert!(md.ends_with("> 价格数据：2026-11-13 10:48:02"), "页脚是扫描时刻: {md}");
        // 分割线要落在「性能」与「价格」之间，而不是随便插一行。
        let sep = md.find("***").expect("要有分割线");
        assert!(sep > md.find("**弹道**").unwrap(), "分割线在弹道之后: {md}");
        assert!(sep < md.find("**商人**").unwrap(), "分割线在商人段之前: {md}");
    }

    /// 物品表里缺这一行时只印弹道，不能印出空的价格段。
    #[test]
    fn detail_without_the_item_row_is_ballistics_only() {
        let md = render_detail(&ammo_stub(), None);
        assert!(md.contains("**弹道**"), "{md}");
        assert!(!md.contains("!["), "没有物品行就没有图: {md}");
        assert!(!md.contains("***"), "没有价格段就不该有分割线: {md}");
        assert!(!md.contains("**商人**"), "{md}");
        assert!(!md.contains("价格数据"), "没有物品行就没有扫描时刻: {md}");
    }

    #[test]
    fn shotgun_pellets_and_tracer_show_up() {
        let mut a = ammo_stub();
        a.projectile_count = 9;
        a.tracer = true;
        let md = render_detail(&a, None);
        assert!(md.contains("弹丸 9"), "霰弹要说几颗: {md}");
        assert!(md.contains("曳光"), "{md}");
    }
}
