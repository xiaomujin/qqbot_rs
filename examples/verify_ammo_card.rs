//! 真机验证工具：把**弹药列表 / 详情**的 Markdown 打到控制台，或发到指定会话。
//!
//! 用法（目标写成「场景:openid」，省略就只打印、不发送）：
//!
//!     cargo run --example verify_ammo_card -- "" m855
//!     cargo run --example verify_ammo_card -- group:<openid> 5.45 list
//!     cargo run --example verify_ammo_card -- group:<openid> 56dff2ced2720bb4668ec70c detail
//!
//! 为什么要这个工具：列表是「一发两行」的版式，而官方文档**没有**明确承诺
//! markdown 列表项内可以换行（只承诺了行末零宽空格强制换行）。真机上被压成一行时，
//! 改 `ammo::render_list` 一个函数就能切单行版 —— 但首先得看得见。
//!
//! ⚠️ 诊断工具，不是运行时的一部分：发送时绕过 SessionShard 的 msg_seq 分配与主动配额，
//! 直接打 OpenAPI。只发一条，别拿它当推送通道。

use anyhow::{Context, Result};
use qqbot_plugins::ammo::{parse_ammo, parse_item_lang, query_tokens};
use qqbot_plugins::http::{build_bulk_client, fetch_text};
use qqbot_plugins::md::is_item_id;
use qqbot_plugins::ammo::render_detail;
use qqbot_plugins::ammo::render_list;
use qqbot_plugins::tarkov_data::{parse_items, parse_trader_names};

/// 与 `ammo.rs` 的 `LIST_LIMIT` 保持一致（这里只做预览，不复用私有常量）。
const LIST_LIMIT: usize = 10;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let target_arg = args.first().cloned().unwrap_or_default();
    let query = args.get(1).cloned().unwrap_or_else(|| "5.45".to_string());
    let mode = args.get(2).cloned().unwrap_or_else(|| "auto".to_string());

    // 1) 拉数据（与刷新器同一批端点，但不写库）。
    let http = build_bulk_client()?;
    let get = |url: &str| {
        let http = http.clone();
        let url = url.to_string();
        async move { fetch_text(&http, &url).await.map_err(anyhow::Error::new) }
    };
    let (items, items_zh, traders, traders_zh) = tokio::try_join!(
        get("https://json.tarkov.dev/regular/items"),
        get("https://json.tarkov.dev/regular/items_zh"),
        get("https://json.tarkov.dev/regular/traders"),
        get("https://json.tarkov.dev/regular/traders_zh"),
    )?;
    let item_pack = parse_item_lang(&items_zh).map_err(anyhow::Error::msg)?;
    let trader_pack =
        qqbot_plugins::task::parse_lang_pack(&traders_zh).map_err(anyhow::Error::msg)?;
    let trader_names = parse_trader_names(&traders, &trader_pack);
    let ammo = parse_ammo(&items, &item_pack).map_err(anyhow::Error::msg)?;
    // 价格 / 图片 / 链接在物品表里，预览要把两边拼起来才看得出真实版式。
    let all = parse_items(&items, &item_pack, &trader_names).map_err(anyhow::Error::msg)?;
    println!("解析 {} 发弹药、{} 件物品", ammo.len(), all.len());

    // 2) 渲染：参数是 24 位 id 就出详情，否则按关键词过滤出列表。
    let markdown = if is_item_id(&query) || mode == "detail" {
        let one = ammo
            .iter()
            .find(|a| a.id == query || a.normalized_name.contains(&query) || a.display_name().contains(&query))
            .with_context(|| format!("没找到「{query}」"))?;
        let item = all.iter().find(|i| i.id == one.id);
        render_detail(one, item)
    } else {
        let tokens = query_tokens(&query);
        let hit: Vec<_> = ammo
            .iter()
            .filter(|a| {
                let slug = a.normalized_name.to_lowercase();
                let zh = a.display_name().to_lowercase();
                tokens.iter().all(|t| slug.contains(t) || zh.contains(t))
            })
            .take(LIST_LIMIT)
            .cloned()
            .collect();
        anyhow::ensure!(!hit.is_empty(), "没找到匹配「{query}」的弹药");
        let total: i64 = i64::try_from(hit.len()).unwrap_or(i64::MAX);
        if mode == "both" {
            // 一条消息里看全两套版式：跳蚤预览当时也是这么发的（省配额）。
            let first = hit.first().context("没有命中")?;
            let item = all.iter().find(|i| i.id == first.id);
            format!("{}\n\n***\n\n{}", render_list(&hit, total), render_detail(first, item))
        } else {
            render_list(&hit, total)
        }
    };
    println!("---- Markdown（{mode}）----\n{markdown}\n----");

    // 3) 给了目标才发。凭据在 config.toml 的顶层（与 verify_task_card 同一套约定）。
    let Some((scene, openid)) = target_arg.split_once(':') else {
        println!("（没给 c2c:/group: 目标，只打印不发送）");
        return Ok(());
    };
    use qqbot_api::{ApiClient, ApiClientConfig, OutMessage, Target};
    let target = match scene {
        "c2c" => Target::C2c { user_openid: openid.to_string() },
        "group" => Target::Group { group_openid: openid.to_string() },
        other => anyhow::bail!("未知场景 {other}"),
    };
    let raw = std::fs::read_to_string("config.toml").context("读 config.toml")?;
    let cfg: toml::Value = toml::from_str(&raw).context("解析 config.toml")?;
    let text = |key: &str| cfg.get(key).and_then(toml::Value::as_str).unwrap_or_default().to_string();
    let api = ApiClient::new(ApiClientConfig::new(text("app_id"), text("client_secret")))?;
    match api.send_message(&target, &OutMessage::markdown(markdown)).await {
        Ok(res) => println!("✓ 发送成功：{res:?}"),
        Err(err) => println!("✗ 发送失败：{err:?}"),
    }
    Ok(())
}
