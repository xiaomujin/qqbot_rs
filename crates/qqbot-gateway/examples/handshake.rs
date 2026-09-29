//! P0 验证：真实连接 QQ 网关，验证 token 获取 / Identify / 心跳 / READY。
//!
//! 本示例**只接收**，不会主动发送任何消息。
//!
//! 运行（凭据从环境变量读，不再依赖 bot.txt）：
//! `$env:QQBOT_APP_ID='...'; $env:QQBOT_APP_SECRET='...'; cargo run -p qqbot-gateway --example handshake`

use std::time::Duration;

use qqbot_api::{ApiClient, ApiClientConfig, Event, Intents};
use qqbot_gateway::{spawn_gateway, GatewayConfig};

/// 从环境变量取凭据。
///
/// 示例刻意**不读 `config.toml`**：那要给这个 crate 加 `toml` + `serde` 两个
/// dev-dependency，而示例只用于手工验证握手，环境变量足够。
///
/// 错误用 `&'static str`：消息是字面量，不必为它分配一个 String，
/// 而 `?` 依然能把它转成 `Box<dyn Error>`。
fn credentials() -> Result<(String, String), &'static str> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    match (get("QQBOT_APP_ID"), get("QQBOT_APP_SECRET")) {
        (Some(a), Some(s)) => Ok((a, s)),
        _ => Err(
            "未找到凭据。请先设置环境变量 QQBOT_APP_ID / QQBOT_APP_SECRET：\n\
             $env:QQBOT_APP_ID='...'; $env:QQBOT_APP_SECRET='...'",
        ),
    }
}

fn summarize(ev: &Event) -> String {
    match ev {
        Event::Ready(r) => format!("session_id={} user={}", r.session_id, r.user.username),
        Event::Resumed => "事件补发完成".to_string(),
        Event::C2cMessage(m) | Event::GroupAtMessage(m) | Event::GroupMessage(m) => {
            format!("content={:?} group={:?}", m.content, m.group_openid)
        }
        other => other.name().to_string(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_target(false)
        .init();

    let (app_id, secret) = credentials()?;
    println!("[1/4] AppID = {app_id}，AppSecret 长度 = {}", secret.len());

    let api = ApiClient::new(ApiClientConfig::new(app_id, secret))?;

    let token = api.token().await?;
    println!("[2/4] access_token 获取成功（长度 {}）", token.len());

    let info = api.gateway().await?;
    println!(
        "[3/4] 网关地址 = {}，建议分片 = {}，剩余 session 启动额度 = {:?}",
        info.url,
        info.shards,
        info.session_start_limit.as_ref().map(|s| s.remaining)
    );

    let cfg = GatewayConfig::default().with_intents(Intents::GROUP_AND_C2C_EVENT);
    let mut gw = spawn_gateway(api, cfg).await?;
    let window: u64 = std::env::var("OBSERVE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(25);
    println!("[4/4] 已建立 {} 个分片连接，观察 {} 秒...", gw.shard_count(), window);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(window);
    let mut count = 0usize;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                println!("观察窗口结束");
                break;
            }
            ev = gw.recv() => {
                match ev {
                    Some(e) => {
                        count += 1;
                        println!("  [事件 #{count}] {} => {}", e.name(), summarize(&e));
                    }
                    None => { println!("事件通道关闭"); break; }
                }
            }
        }
    }

    gw.shutdown().await;
    println!("握手验证完成，共收到 {count} 个事件");
    Ok(())
}
