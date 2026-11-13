//! 真机验证工具：把**跳蚤列表 / 详情**的 Markdown 发到指定会话，看真机版式。
//!
//! 用法（目标写成「场景:openid」）：
//!
//!     cargo run --example verify_market_card -- c2c:<openid> m4a1
//!     cargo run --example verify_market_card -- group:<openid> 5447a9cd4bdc2dbd208b4567
//!     cargo run --example verify_market_card -- group:<openid> 弹匣 list
//!
//! 为什么要这个工具：列表是「一件两行」的版式，而官方文档**没有**明确承诺
//! markdown 列表项内可以换行（只承诺了行末零宽空格强制换行）。
//! 真机上被压成一行时，改 `market::render_list` 一个函数就能切单行版 ——
//! 但首先得看得见。
//!
//! ⚠️ 诊断工具，不是运行时的一部分：它绕过 SessionShard 的 msg_seq 分配与主动配额，
//! 直接打 OpenAPI。只发一条，别拿它当推送通道。

use anyhow::{Context, Result};
use qqbot_api::{ApiClient, ApiClientConfig, OutMessage, Target};
use qqbot_plugins::ammo::{parse_item_lang, query_tokens};
use qqbot_plugins::http::build_bulk_client;
use qqbot_plugins::market::{render_detail, render_list};
use qqbot_plugins::md::is_item_id;
use qqbot_plugins::tarkov_data::{parse_items, parse_trader_names};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let target_arg = args
        .first()
        .cloned()
        .context("用法：verify_market_card <c2c|group>:<openid> [关键词|24位id] [list|detail]")?;
    let query = args.get(1).cloned().unwrap_or_else(|| "m4a1".to_string());
    let mode = args.get(2).cloned().unwrap_or_else(|| "auto".to_string());

    let (scene, openid) = target_arg
        .split_once(':')
        .context("目标要写成 c2c:<openid> 或 group:<openid>")?;
    let target = match scene {
        "c2c" => Target::C2c { user_openid: openid.to_string() },
        "group" => Target::Group { group_openid: openid.to_string() },
        other => anyhow::bail!("未知场景 {other}"),
    };

    // 凭据在 config.toml 的顶层（与 verify_task_card 同一套约定）。
    let raw = std::fs::read_to_string("config.toml").context("读 config.toml")?;
    let cfg: toml::Value = toml::from_str(&raw).context("解析 config.toml")?;
    let text = |key: &str| cfg.get(key).and_then(toml::Value::as_str).unwrap_or_default().to_string();
    let app_id = text("app_id");
    let secret = text("client_secret");

    // 1) 拉数据（与刷新器同一批端点，但不写库）。
    let http = build_bulk_client()?;
    let get = |url: &str| {
        let http = http.clone();
        let url = url.to_string();
        async move {
            qqbot_plugins::http::fetch_text(&http, &url)
                .await
                .map_err(anyhow::Error::new)
        }
    };
    let (items, items_zh, traders, traders_zh) = tokio::try_join!(
        get("https://json.tarkov.dev/regular/items"),
        get("https://json.tarkov.dev/regular/items_zh"),
        get("https://json.tarkov.dev/regular/traders"),
        get("https://json.tarkov.dev/regular/traders_zh"),
    )?;
    let item_pack = parse_item_lang(&items_zh).map_err(anyhow::Error::msg)?;
    let traders_pack = qqbot_plugins::task::parse_lang_pack(&traders_zh).map_err(anyhow::Error::msg)?;
    let trader_names = parse_trader_names(&traders, &traders_pack);
    let all = parse_items(&items, &item_pack, &trader_names).map_err(anyhow::Error::msg)?;
    println!("解析 {} 件物品", all.len());

    // 2) 渲染：参数是 24 位 id 就出详情，否则按关键词过滤出列表。
    let markdown = if is_item_id(&query) || mode == "detail" {
        let item = all
            .iter()
            .find(|i| i.id == query || i.normalized_name.contains(&query) || i.display_name().contains(&query))
            .with_context(|| format!("没找到「{query}」"))?;
        render_detail(item)
    } else {
        let tokens = query_tokens(&query);
        let hit: Vec<_> = all
            .iter()
            .filter(|i| {
                let slug = i.normalized_name.to_lowercase();
                let zh = i.display_name().to_lowercase();
                tokens.iter().all(|t| slug.contains(t) || zh.contains(t))
            })
            .take(10)
            .cloned()
            .collect();
        anyhow::ensure!(!hit.is_empty(), "没找到匹配「{query}」的物品");
        let total: i64 = i64::try_from(hit.len()).unwrap_or(i64::MAX);
        render_list(&hit, total)
    };
    println!("---- 卡片（{mode}）----\n{markdown}\n----");

    // 3) 发到真机。
    let api = ApiClient::new(ApiClientConfig::new(app_id, secret))?;
    match api.send_message(&target, &OutMessage::markdown(markdown)).await {
        Ok(res) => println!("✓ 发送成功：{res:?}"),
        Err(err) => println!("✗ 发送失败：{err:?}"),
    }
    Ok(())
}
