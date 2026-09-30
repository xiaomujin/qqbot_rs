//! 真机验证工具：把真实的塔科夫任务卡片发到指定会话，并把数据导入本地库。
//!
//! 用法（目标写成 「场景:openid」，模式 detail|list）：
//!
//!     cargo run --example verify_task_card -- c2c:<openid> 「惩罚者 - 1」
//!     cargo run --example verify_task_card -- group:<openid> 「惩罚者」 list
//!
//! 它做的事与「更新任务」完全一致（拉 6 个静态端点 → 套语言包 → 写库），
//! 额外把渲染好的 Markdown 发到指定会话，用来验证真机上
//! Markdown 与「可点击指令文本」的渲染效果。
//!
//! ⚠️ 这是**诊断工具**，不是机器人运行时的一部分：它绕过 SessionShard 的
//! msg_seq 分配与主动配额，直接打 OpenAPI。只发一条，别拿它当推送通道。

use std::collections::HashMap;

use anyhow::{Context, Result};
use qqbot_api::{ApiClient, ApiClientConfig, OutMessage, Target};
use qqbot_plugins::http::build_client;
use qqbot_plugins::task::{LangPacks, TaskConfig, parse_lang_pack, parse_tasks, render_detail, render_list};
use qqbot_store::TarkovTask;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let target_arg = args
        .first()
        .cloned()
        .context("用法：verify_task_card <c2c|group>:<openid> [任务名片段] [detail|list]")?;
    let query = args.get(1).cloned().unwrap_or_else(|| "惩罚者 - 1".to_string());
    let mode = args.get(2).cloned().unwrap_or_else(|| "detail".to_string());

    let (scene, openid) = target_arg
        .split_once(':')
        .context("目标要写成 c2c:<openid> 或 group:<openid>")?;
    let target = match scene {
        "c2c" => Target::C2c { user_openid: openid.to_string() },
        "group" => Target::Group { group_openid: openid.to_string() },
        other => anyhow::bail!("未知场景 {other}"),
    };

    // 凭据与库路径都在 config.toml 的顶层。
    let raw = std::fs::read_to_string("config.toml").context("读 config.toml")?;
    let cfg: toml::Value = toml::from_str(&raw).context("解析 config.toml")?;
    let text = |key: &str| cfg.get(key).and_then(toml::Value::as_str).unwrap_or_default().to_string();
    let app_id = text("app_id");
    let secret = text("client_secret");
    let db_path = match text("db_path") {
        p if p.is_empty() => "data/qqbot.db".to_string(),
        p => p,
    };

    // raw 模式：第 2 个参数是**文件路径**，文件内容原样发出去。
    // 用来探针式验证某个 Markdown 语法在真机上到底渲染成什么样 ——
    // 拉数据、入库全部跳过。
    if mode == "raw" {
        let body = std::fs::read_to_string(&query).with_context(|| format!("读 {query}"))?;
        let api = ApiClient::new(ApiClientConfig::new(app_id, secret))?;
        let msg = OutMessage::markdown(body);
        match api.send_message(&target, &msg).await {
            Ok(res) => println!("✓ 已原样发送：{res:?}"),
            Err(err) => println!("✗ 发送失败：{err:?}"),
        }
        return Ok(());
    }

    // 1) 拉真实数据（与「更新任务」同一批端点）。
    let task_cfg = TaskConfig::default();
    let http = build_client()?;
    let get = |url: &str| {
        let req = http.get(url).header("User-Agent", "Mozilla/5.0 qqbot-rs").send();
        async move { Ok::<String, anyhow::Error>(req.await?.text().await?) }
    };
    let (tasks, traders, tasks_zh, items_zh, traders_zh, maps_zh) = tokio::try_join!(
        get(&task_cfg.tasks_url),
        get(&task_cfg.traders_url),
        get(&task_cfg.tasks_zh_url),
        get(&task_cfg.items_zh_url),
        get(&task_cfg.traders_zh_url),
        get(&task_cfg.maps_zh_url),
    )?;

    let packs = LangPacks {
        tasks: parse_lang_pack(&tasks_zh).map_err(anyhow::Error::msg)?,
        items: parse_lang_pack(&items_zh).map_err(anyhow::Error::msg)?,
        traders: parse_lang_pack(&traders_zh).map_err(anyhow::Error::msg)?,
        maps: parse_lang_pack(&maps_zh).map_err(anyhow::Error::msg)?,
    };
    let (items, coverage) = parse_tasks(&tasks, &traders, &packs).map_err(anyhow::Error::msg)?;
    println!("解析 {} 条任务；{}", items.len(), coverage.summary());

    // 2) 渲染。
    let names: HashMap<String, String> = items
        .iter()
        .filter_map(|d| d.task.name_zh.clone().map(|n| (d.task.id.clone(), n)))
        .collect();
    let matched: Vec<_> = items
        .iter()
        .filter(|d| {
            d.task.name_zh.as_deref().is_some_and(|n| n.contains(&query))
                || d.task.normalized_name.contains(&query)
        })
        .collect();
    anyhow::ensure!(!matched.is_empty(), "没找到匹配「{query}」的任务");
    let markdown = if mode == "list" {
        let tasks: Vec<TarkovTask> = matched.iter().map(|d| d.task.clone()).collect();
        render_list(&query, &tasks, tasks.len())
    } else {
        render_detail(matched[0], &names)
    };
    println!("---- 卡片（{mode}，命中 {} 条）----\n{markdown}", matched.len());

    // 3) 入库（等价于管理员发「更新任务」）。
    let store = qqbot_store::open_resource_store(std::path::PathBuf::from(&db_path)).await?;
    let count = store.replace_tasks(items).await?;
    println!("已写入 {db_path}：{count} 条");

    // 4) 发到真机。
    let api = ApiClient::new(ApiClientConfig::new(app_id, secret))?;
    let msg = OutMessage::markdown(markdown);
    match api.send_message(&target, &msg).await {
        Ok(res) => println!("✓ 发送成功：{res:?}"),
        Err(err) => println!("✗ 发送失败：{err:?}"),
    }
    Ok(())
}
