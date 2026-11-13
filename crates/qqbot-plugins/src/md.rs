//! Markdown 消息的公共排版助手（跳蚤市场 / 弹药共用）。
//!
//! 为什么单独一个模块：**详情页的价格段在两处一模一样**（商人 / 跳蚤 / 24h /
//! 48h / 规格 / 门槛），而版式恰好是本项目最容易被悄悄改坏的东西 ——
//! 手机一行约 30 个显示单位、分割线只有 `***`、强制换行只能靠行末零宽空格、
//! 名字里的半角方括号必须换全角。同一套约束抄两遍，改一处漏一处只是时间问题。
//!
//! 官方语法见 `docs/qq-bot-api-v2-protocol.md` §13。

use qqbot_store::TarkovItem;

use crate::timewin::{SHANGHAI_OFFSET, format_datetime};

/// 强换行：行末的零宽空格。官方文档给的唯一可靠换行手段。
pub const BR: char = '\u{200B}';

/// 是不是游戏内物品 id（24 位十六进制）。
///
/// 用它区分「查详情」与「搜关键词」，而不是让用户多记一条命令。
pub fn is_item_id(raw: &str) -> bool {
    raw.len() == 24 && raw.chars().all(|c| c.is_ascii_hexdigit())
}

/// 价格写成「万」，否则六位数看不过来。
pub fn format_price(value: i64) -> String {
    // `unsigned_abs` 而不是 `abs`：`i64::MIN.abs()` 在 debug 下会 panic。
    // 价格当然不可能是 i64::MIN，但一个会在越界输入上 panic 的格式化函数
    // 没有理由留着 —— 它迟早会被用在别的地方。
    if value.unsigned_abs() >= 10_000 {
        format!("{:.2}万", value as f64 / 10_000.0)
    } else {
        value.to_string()
    }
}

/// 48 小时变化：`+215（+0.40%）`。百分比直接用上游值（口径见 schema 注释）。
fn format_change(change: i64, percent: Option<f64>) -> String {
    let sign = if change >= 0 { "+" } else { "-" };
    let abs = i64::try_from(change.unsigned_abs()).unwrap_or(i64::MAX);
    match percent {
        Some(p) => format!("{sign}{}（{p:+.2}%）", format_price(abs)),
        None => format!("{sign}{}", format_price(abs)),
    }
}

/// Markdown 里半角方括号会提前闭合 `[...](...)`，换成全角（同 `bili` 的做法）。
///
/// 不用反斜杠转义：官方文档没有承诺支持 `\[`，而实测全角一定安全。
pub fn md_safe(raw: &str) -> String {
    raw.replace('[', "［").replace(']', "］")
}

/// `**标签** 值1｜值2` 一行；`values` 为空时不出这一行。
pub fn labelled(label: &str, values: &[String]) -> Option<String> {
    if values.is_empty() {
        return None;
    }
    Some(format!("**{label}** {}{BR}", values.join("｜")))
}

/// 详情页图片的显示边长（px）。
///
/// 上游 `gridImageLink` 的原图只有 **64×64**（实测 5498 件全是，约 2.5 KB），
/// 所以这里只放大到 1.5 倍 —— 再大就是一张马赛克，还把消息撑长。
/// 想更清楚写 64、想更大写 128：跳蚤与弹药两个详情视图都走这一个常量。
const IMAGE_PX: u32 = 96;

/// 图片行。`url` 必须是**公网 URL**（平台自己下载转存），所以只认 http(s)。
pub fn image_block(name: &str, url: Option<&str>) -> Option<String> {
    let url = url.filter(|u| u.starts_with("http"))?;
    Some(format!("![{name} #{IMAGE_PX}px #{IMAGE_PX}px]({url})"))
}

/// 商人段：基准价（手册口径）与最高收购（卖掉能拿多少）各一份。
pub fn trader_line(item: &TarkovItem) -> Option<String> {
    let mut values: Vec<String> = Vec::new();
    if item.base_price > 0 {
        values.push(format!("基准 {}", format_price(item.base_price)));
    }
    if let Some(price) = item.sell_trader_price {
        let who = item
            .sell_trader_name
            .as_deref()
            .map(|n| format!("（{}）", md_safe(n)))
            .unwrap_or_default();
        values.push(format!("最高收购 {}{who}", format_price(price)));
    }
    labelled("商人", &values)
}

/// 价格段：跳蚤 / 24h / 48h / 规格 / 门槛。
///
/// **空段不出现** —— 没有跳蚤价的物品不会印出一行「均价 -」。
pub fn price_lines(item: &TarkovItem) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();

    let mut flea: Vec<String> = Vec::new();
    if let Some(price) = item.avg24h_price {
        flea.push(format!("均价 {}", format_price(price)));
    }
    if let Some(price) = item.last_low_price {
        flea.push(format!("最低挂牌 {}", format_price(price)));
    }
    lines.extend(labelled("跳蚤", &flea));

    let mut range: Vec<String> = Vec::new();
    if let (Some(low), Some(high)) = (item.low24h_price, item.high24h_price) {
        range.push(format!("区间 {} ~ {}", format_price(low), format_price(high)));
    }
    if let Some(count) = item.offer_count {
        range.push(format!("挂单 {count} 条"));
    }
    lines.extend(labelled("24h", &range));

    let mut change: Vec<String> = Vec::new();
    if let Some(value) = item.change_48h {
        change.push(format_change(value, item.change_48h_percent));
    }
    lines.extend(labelled("48h", &change));

    let mut spec: Vec<String> = Vec::new();
    if item.grid_w > 0 && item.grid_h > 0 {
        spec.push(format!("{}×{} 格", item.grid_w, item.grid_h));
    }
    if item.weight > 0.0 {
        spec.push(format!("{:.2} kg", item.weight));
    }
    // 每格价值：塔科夫玩家的核心指标（同样 10 万，占 1 格与占 6 格不是一回事）。
    if let Some(value) = item.value_per_slot() {
        spec.push(format!("每格 {}", format_price(value)));
    }
    lines.extend(labelled("规格", &spec));

    let mut access: Vec<String> = Vec::new();
    if item.min_level_flea > 0 {
        access.push(format!("跳蚤准入 {} 级", item.min_level_flea));
    }
    lines.extend(labelled("门槛", &access));

    lines
}

/// 链接行：`[wiki](…)｜[tarkov.dev](…)`。
pub fn link_line(item: &TarkovItem) -> Option<String> {
    let mut links: Vec<String> = Vec::new();
    if let Some(url) = item.wiki_link.as_deref() {
        links.push(format!("[wiki]({url})"));
    }
    if let Some(url) = item.dev_link.as_deref() {
        links.push(format!("[tarkov.dev]({url})"));
    }
    if links.is_empty() {
        return None;
    }
    Some(links.join("｜"))
}

/// 页脚：上游的价格扫描时刻，比「我们什么时候刷的」更贴近这个价的真实年龄。
pub fn scan_footer(item: &TarkovItem) -> String {
    let scanned = item
        .last_scan
        .map(|ts| format_datetime(ts, SHANGHAI_OFFSET))
        .unwrap_or_else(|| "未知".to_string());
    format!("> 价格数据：{scanned}")
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn change_keeps_the_sign_and_the_percentage() {
        assert_eq!(format_change(215, Some(0.4)), "+215（+0.40%）");
        assert_eq!(format_change(-13_169, Some(-27.38)), "-1.32万（-27.38%）");
        assert_eq!(format_change(0, None), "+0", "没有百分比就不印括号");
    }

    #[test]
    fn brackets_become_full_width() {
        assert_eq!(md_safe("M4A1 [测试]"), "M4A1 ［测试］");
    }

    #[test]
    fn images_need_a_public_url() {
        assert!(image_block("x", Some("https://a/b.webp")).is_some());
        assert!(image_block("x", Some("file:///a.png")).is_none(), "平台只能转存公网地址");
        assert!(image_block("x", None).is_none());
    }

    #[test]
    fn nothing_is_printed_for_an_empty_item() {
        // 上游什么都没有时不印空段 —— 一行「跳蚤 均价 -」比不印更糟。
        let item = TarkovItem::default();
        assert!(price_lines(&item).is_empty(), "{:?}", price_lines(&item));
        assert!(trader_line(&item).is_none());
        assert!(link_line(&item).is_none());
        assert!(labelled("跳蚤", &[]).is_none());
    }
}
