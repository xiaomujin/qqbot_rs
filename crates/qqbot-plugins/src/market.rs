//! 塔科夫跳蚤市场（B2 / B3 合并成一条命令）。
//!
//! 源项目里 B2 走 `tarkov-market.com`（模糊搜索）、B3 走 `api.tarkov.dev` GraphQL
//! （按 id 精确查），两者都只是「查跳蚤价格」。而 tarkov-market.com 已经死了
//! （403 + 加密载荷），GraphQL 后端也挂着。所以这里用同一张本地表：
//! 参数是 **24 位十六进制 id** 就给详情，否则当关键词搜。
//!
//! # 为什么全是 Markdown
//!
//! 列表里的「详情」必须是**可点文本**（`qqbot-cmd-input`，点击把指令插进输入框），
//! 而富媒体卡片（`msg_type=7`）的 content 是纯文本、塞不进这种标签 ——
//! 两者只能二选一。于是整个插件改走 `msg_type=2`：列表与详情都是 Markdown。
//!
//! 版式约定见 `docs/qq-bot-api-v2-protocol.md` 的「Markdown 消息」一节：
//! 手机一行约 30 个显示单位（汉字算 2），**行末要放零宽空格**才强制换行。
//!
//! # 数据从哪来
//!
//! 只读本地 `tarkov_item` 表，由 `tarkov_data` 每小时从上游静态 JSON 刷新。
//! 本模块没有任何 HTTP：查询失败只可能是数据库问题，不是网络问题。

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use qqbot_store::{ResourceStore, TarkovItem};

use crate::ammo::query_tokens;
use crate::md::{
    BR, format_price, image_block, is_item_id, link_line, md_safe, price_lines, scan_footer,
    trader_line,
};
use crate::task::cmd_input;

/// 一次最多列几件。
///
/// 十条是**手机屏**的折中：两行一件、十条正好一屏多一点；再多用户也不会读完，
/// 而消息长度与「补个关键词」的成本都在涨。
const LIST_LIMIT: usize = 10;

/// 跳蚤市场插件配置。
#[derive(Clone, Default)]
pub struct MarketConfig {
    /// 物品表。`None` 表示未启用持久化。
    pub store: Option<Arc<ResourceStore>>,
}

/// 一行价格摘要（列表第二行用）。
///
/// 分隔符两侧**不留空格**：手机一行只有 30 个单位，
/// `商人 8,278｜跳蚤 9.36万｜详情` 正好 27，加上空格就是 31 —— 会折行。
fn price_summary(item: &TarkovItem) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(price) = item.sell_trader_price {
        parts.push(format!("商人 {}", format_price(price)));
    }
    match item.flea_price() {
        Some(price) => parts.push(format!("跳蚤 {}", format_price(price))),
        // 说清楚「没有」而不是显示 0 —— 0 会被当成「不值钱」。
        None => parts.push("无跳蚤数据".into()),
    }
    // 可点文本：点一下只是把「跳蚤 <id>」插进输入框，用户还得按发送
    // （群聊不支持 qqbot-cmd-enter，见协议文档 §12.1）。
    parts.push(cmd_input(&format!("跳蚤 {}", item.id), "详情"));
    parts.join("｜")
}

/// 搜索结果（Markdown）。
///
/// 每件两行：`- **名称**` + 缩进的 `商人 … ｜ 跳蚤 … ｜ 详情`。
/// 两行版式依赖「列表项内换行」，官方没有明确承诺 —— 所以这里做成**纯函数**，
/// 真机实测若被压成一行，只改这一个函数就能切到单行版。
pub fn render_list(items: &[TarkovItem], total: i64) -> String {
    let mut out = String::new();
    // 只有「被截断」时才提示条数：没截断还写一行，纯属噪音。
    if total > items.len() as i64 {
        let _ = writeln!(out, "匹配 {total} 件，显示前 {} 件。补个关键词能缩小范围。", items.len());
        out.push('\n');
    }
    for item in items {
        let name = md_safe(item.display_name());
        let _ = writeln!(out, "- **{name}**{BR}");
        let _ = writeln!(out, "  {}{BR}", price_summary(item));
    }
    out
}

/// 详情页（Markdown）。**空段不出现** —— 没有跳蚤价的物品不会印出一行「均价 -」。
///
/// 版式的公共部分（图片 / 商人段 / 价格段 / 链接 / 页脚）在 `crate::md`，
/// 弹药详情用的是同一套 —— 两处必须长得一样，否则改一处漏一处只是时间问题。
pub fn render_detail(item: &TarkovItem) -> String {
    let name = md_safe(item.display_name());
    let mut blocks: Vec<String> = vec![format!("# {name}")];

    // 图片必须是**公网 URL**（平台自己下载转存），所以这里只认 http(s)。
    blocks.extend(image_block(&name, item.image_url.as_deref()));
    blocks.extend(trader_line(item));

    let prices = price_lines(item);
    if !prices.is_empty() {
        // 分割线用 `***`（官方示例只有这个，`---` 不保证渲染）。
        blocks.push("***".to_string());
        blocks.push(prices.join("\n"));
    }

    blocks.extend(link_line(item));
    // 页脚用**上游的价格扫描时刻**，比「我们什么时候刷的」更贴近这个价的真实年龄。
    blocks.push(scan_footer(item));

    blocks.join("\n\n")
}


/// 跳蚤市场插件：只读本地表。
///
/// 数据由 `tarkov_data` 每小时刷新 —— 这里没有任何 HTTP。
#[derive(Clone)]
pub struct MarketPlugin {
    config: MarketConfig,
}

impl MarketPlugin {
    pub fn new(config: MarketConfig) -> Self {
        Self { config }
    }

    /// `跳蚤<空格可有可无> <24位id | 关键词…>`。
    async fn query(&self, ctx: &Ctx, raw: &str) -> Handled {
        let Some(store) = &self.config.store else {
            let _ = ctx.reply.text("未启用持久化，跳蚤功能不可用").await;
            return Handled::Consumed;
        };
        if raw.is_empty() {
            let _ = ctx
                .reply
                .markdown("用法：`跳蚤 <名称片段>`（可多关键词，空格分隔），或 `跳蚤 <24位物品id>`")
                .await;
            return Handled::Consumed;
        }

        // 24 位十六进制 → 详情视图；否则当关键词搜。
        let markdown = if is_item_id(raw) {
            match store.get_item(raw).await {
                Ok(Some(item)) => render_detail(&item),
                Ok(None) => {
                    let _ = ctx
                        .reply
                        .text(self.empty_hint(store, "没有这个 id 对应的物品").await)
                        .await;
                    return Handled::Consumed;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "按 id 取物品失败");
                    let _ = ctx.reply.text("查询失败，稍后再试").await;
                    return Handled::Consumed;
                }
            }
        } else {
            let tokens = query_tokens(raw);
            if tokens.is_empty() {
                let _ = ctx
                    .reply
                    .markdown("用法：`跳蚤 <名称片段>`（可多关键词，空格分隔），或 `跳蚤 <24位物品id>`")
                    .await;
                return Handled::Consumed;
            }
            let items = match store.search_items(tokens.clone(), search_hint(raw), LIST_LIMIT).await
            {
                Ok(items) => items,
                Err(err) => {
                    tracing::warn!(error = %err, "检索物品失败");
                    let _ = ctx.reply.text("查询失败，稍后再试").await;
                    return Handled::Consumed;
                }
            };
            if items.is_empty() {
                let _ = ctx
                    .reply
                    .text(self.empty_hint(store, "没有匹配的物品").await)
                    .await;
                return Handled::Consumed;
            }
            let total = store.count_items(tokens).await.unwrap_or(items.len() as i64);
            render_list(&items, total)
        };

        if let Err(err) = ctx.reply.markdown(markdown).await {
            // 不降级成文本摘要：Markdown 发不出去时再发一条「两行一件」的纯文本
            // 只会更难看，而且用户已经等了同样久。失败就明说失败。
            tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "跳蚤消息发送失败");
            let _ = ctx.reply.text(format!("跳蚤消息发送失败：{err}")).await;
        }
        Handled::Consumed
    }

    /// 查不到时的话术。空表与「关键词不对」要给不同的话。
    async fn empty_hint(&self, store: &ResourceStore, miss: &str) -> String {
        match store.item_count().await {
            Ok(0) => "物品数据还没导入，请系统控制者发「塔科夫更新数据」".to_string(),
            _ => format!("{miss}，换个关键词试试"),
        }
    }
}

/// 排序用的「原样片段」：小写、去掉 LIKE 的通配符。
///
/// 参数里混进 `%` 或 `_` 会让排序条件变成「匹配一切」，
/// 排序就从「相关性」退化成随机。
fn search_hint(raw: &str) -> String {
    raw.chars()
        .filter(|c| *c != '%' && *c != '_')
        .collect::<String>()
        .trim()
        .to_lowercase()
}

#[async_trait]
impl Handler for MarketPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        let content = ctx.content().trim();
        // 路由用 `Matcher::Prefix("跳蚤")`，所以 `跳蚤 m4a1` 与 `跳蚤m4a1`
        // 都会到这里 —— 差别只在 strip 之后有没有前导空格。
        let Some(rest) = content.strip_prefix("跳蚤") else {
            return Handled::Next;
        };
        self.query(ctx, rest.trim()).await
    }

    fn name(&self) -> &'static str {
        "塔科夫跳蚤市场"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, slug: &str, name: Option<&str>) -> TarkovItem {
        TarkovItem {
            id: id.into(),
            normalized_name: slug.into(),
            name_zh: name.map(str::to_string),
            base_price: 18_397,
            last_low_price: Some(23_932),
            avg24h_price: Some(53_707),
            low24h_price: Some(20_000),
            high24h_price: Some(185_000),
            change_48h: Some(215),
            change_48h_percent: Some(0.4),
            offer_count: Some(20),
            sell_trader_name: Some("Prapor".into()),
            sell_trader_price: Some(8_278),
            min_level_flea: 25,
            grid_w: 1,
            grid_h: 2,
            weight: 3.4,
            icon_url: Some(format!("https://assets/{id}-icon.webp")),
            image_url: Some(format!("https://assets/{id}-grid.webp")),
            wiki_link: Some("https://wiki/m4".into()),
            dev_link: Some("https://tarkov.dev/item/m4".into()),
            last_scan: Some(1_794_538_082),
        }
    }

    fn bare(id: &str, slug: &str) -> TarkovItem {
        TarkovItem {
            name_zh: None,
            base_price: 100_000,
            last_low_price: None,
            avg24h_price: None,
            low24h_price: None,
            high24h_price: None,
            change_48h: None,
            change_48h_percent: None,
            offer_count: None,
            sell_trader_name: None,
            sell_trader_price: None,
            min_level_flea: 0,
            grid_w: 1,
            grid_h: 1,
            weight: 0.0,
            icon_url: None,
            image_url: None,
            wiki_link: None,
            dev_link: None,
            last_scan: None,
            id: id.into(),
            normalized_name: slug.into(),
        }
    }

    /// 列表：每件两行，第二行有商人价、跳蚤价与可点的「详情」。
    #[test]
    fn list_puts_name_prices_and_clickable_detail() {
        let md = render_list(&[item("5447a9cd4bdc2dbd208b4567", "colt-m4a1", Some("柯尔特 M4A1"))], 1);
        assert!(md.contains("- **柯尔特 M4A1**"), "第一行是名称: {md}");
        assert!(md.contains("商人 8278"), "第二行要有商人收购价: {md}");
        assert!(md.contains("跳蚤 5.37万"), "第二行要有跳蚤价: {md}");
        assert!(md.contains("<qqbot-cmd-input "), "详情必须是可点文本: {md}");
        assert!(md.contains("text=\"%E8%B7%B3%E8%9A%A4%20"), "点一下要插入「跳蚤 <id>」: {md}");
        assert!(md.contains('\u{200B}'), "行末要有零宽空格，否则不换行: {md}");
        assert!(!md.contains("匹配"), "没被截断就不该提条数: {md}");
    }

    #[test]
    fn list_says_when_it_truncated() {
        let items: Vec<TarkovItem> = (0..10).map(|i| bare(&format!("id{i}"), "s")).collect();
        let md = render_list(&items, 137);
        assert!(md.starts_with("匹配 137 件，显示前 10 件"), "要说明被截断: {md}");
        assert!(md.contains("补个关键词"), "{md}");
    }

    #[test]
    fn list_says_when_there_is_no_flea_data() {
        let md = render_list(&[bare("i1", "slick")], 1);
        assert!(md.contains("无跳蚤数据"), "缺价要说清楚，不能显示 0: {md}");
    }

    /// 详情：图片、三个价、48h、规格、链接、页脚一个都不少。
    #[test]
    fn detail_shows_everything_it_has() {
        let md = render_detail(&item("5447a9cd4bdc2dbd208b4567", "colt-m4a1", Some("柯尔特 M4A1")));
        assert!(md.starts_with("# 柯尔特 M4A1\n"), "{md}");
        assert!(md.contains("![柯尔特 M4A1 #96px #96px](https://assets/"), "图片是公网 URL: {md}");
        assert!(md.contains("**商人** 基准 1.84万｜最高收购 8278（Prapor）"), "{md}");
        assert!(md.contains("***"), "价格区前要有分割线: {md}");
        assert!(md.contains("**跳蚤** 均价 5.37万｜最低挂牌 2.39万"), "{md}");
        assert!(md.contains("**24h** 区间 2.00万 ~ 18.50万｜挂单 20 条"), "{md}");
        assert!(md.contains("**48h** +215（+0.40%）"), "{md}");
        assert!(md.contains("**规格** 1×2 格｜3.40 kg｜每格 2.69万"), "{md}");
        assert!(md.contains("**门槛** 跳蚤准入 25 级"), "{md}");
        assert!(md.contains("[wiki](https://wiki/m4)"), "{md}");
        assert!(md.ends_with("> 价格数据：2026-11-13 10:48:02"), "页脚是扫描时刻: {md}");
    }

    /// 空段不出现：没有跳蚤价、没有图、没有链接的物品只留名称与商人价。
    #[test]
    fn detail_omits_empty_sections() {
        let md = render_detail(&bare("5c0e53c886f7744a13f54933", "slick-body-armor"));
        assert!(md.starts_with("# slick-body-armor"), "没有中文名就用 slug: {md}");
        assert!(!md.contains("!["), "没有图就不该出现图片行: {md}");
        assert!(!md.contains("**跳蚤**"), "没有跳蚤价就没有这一段: {md}");
        assert!(!md.contains("**48h**"), "{md}");
        assert!(!md.contains("**规格**") || !md.contains("kg"), "重量 0 不印: {md}");
        assert!(md.contains("> 价格数据：未知"), "没有扫描时刻也要说清楚: {md}");
    }

    #[test]
    fn brackets_in_names_do_not_break_the_image_syntax() {
        let mut it = item("i1", "x", Some("M4A1 [测试]"));
        it.image_url = Some("https://assets/x.webp".into());
        let md = render_detail(&it);
        assert!(!md.contains("![M4A1 [测试]"), "半角方括号会提前闭合 ![](): {md}");
        assert!(md.contains("M4A1 ［测试］"), "{md}");
    }

    #[test]
    fn search_hint_strips_like_wildcards() {
        assert_eq!(search_hint("  M4A1 "), "m4a1");
        assert_eq!(search_hint("50%_x"), "50x");
    }
}
