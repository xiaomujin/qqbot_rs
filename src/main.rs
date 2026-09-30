//! QQ 机器人（官方 API v2）。
//!
//! 运行模式：
//! - `cargo run`（默认）   连接网关并开始服务
//! - `cargo run -- check`  只校验凭据与网关信息，不建立长连接
//! - `cargo run -- self-test` 离线渲染自检（不需要网络，不需要凭据）

mod config;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use qqbot_api::ApiClient;
use qqbot_core::{DispatchConfig, Dispatcher, Router, Services, SessionRegistry};
use qqbot_gateway::{spawn_gateway, GatewayConfig};
use qqbot_media::MediaUploader;
use qqbot_render::{build_wordcloud_svg, RenderConfig, RenderService, WordItem};
use qqbot_store::MessageStore;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

/// 高并发场景下 mimalloc 明显优于系统分配器。
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "run".to_string());

    match mode.as_str() {
        // 离线自检**不读配置**：没有 config.toml、没有任何凭据也必须能跑。
        "self-test" | "--self-test" => {
            init_tracing("debug");
            self_test().await
        }
        "help" | "--help" | "-h" => {
            println!("用法: qqbot [run|check|self-test]");
            Ok(())
        }
        // 其余两种模式都要先读配置，见 `boot()`。
        "check" | "--check" => check(boot()?).await,
        other => {
            if other != "run" {
                eprintln!("未知模式 {other:?}，回退到 run");
            }
            run(boot()?).await
        }
    }
}

/// 读配置 → 按配置里的级别初始化日志 → 记录配置来源。
///
/// `run` 与 `check` 共用，避免两条路径各自漂移。
/// 顺序不能反：日志级别本身来自配置，所以 `init_tracing` 必须排在 `load` 之后。
fn boot() -> Result<Config> {
    let cfg = Config::load()?;
    init_tracing(&cfg.log_level);
    tracing::info!(sources = %cfg.sources, "配置已加载");
    Ok(cfg)
}

/// 初始化日志。
///
/// 优先级：`RUST_LOG` > `configured`（来自 config.toml，默认 `info`）。
/// 非法指令**不静默生效**：回退到 info，并往 stderr 说清楚是哪个来源的值坏了。
fn init_tracing(configured: &str) {
    let explicit = std::env::var("RUST_LOG")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let (source, value) = match &explicit {
        Some(v) => ("RUST_LOG", v.as_str()),
        None => ("log_level", configured),
    };
    let filter = EnvFilter::try_new(value).unwrap_or_else(|err| {
        eprintln!("{source}={value:?} 不是合法的日志指令（{err}），回退到 info");
        EnvFilter::new("info")
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// 离线渲染自检：验证 resvg 链路、中文字体与缓存是否正常。
async fn self_test() -> Result<()> {
    let out_dir = std::path::Path::new("target/selftest");
    std::fs::create_dir_all(out_dir)?;

    let started = Instant::now();
    let render = RenderService::new(RenderConfig::default());
    println!("渲染服务就绪（初始化 {:?}）", started.elapsed());

    // ---- 1) 模板卡片 ----
    let t0 = Instant::now();
    let card = render
        .render_template(
            "card.svg",
            serde_json::json!({
                "title": "自检 · 模板渲染",
                "rows": [
                    {"label": "渲染后端", "value": "resvg (纯 Rust)"},
                    {"label": "外部依赖", "value": "无 Chromium / 无 Node"},
                    {"label": "中文字体", "value": "系统字体"},
                    {"label": "输出倍率", "value": "2.0x"}
                ],
                "width": 720,
                "height": 340
            }),
        )
        .await
        .context("模板渲染失败")?;
    let card_ms = t0.elapsed().as_secs_f64() * 1000.0;
    std::fs::write(out_dir.join("card.png"), &card.png)?;
    println!(
        "✓ card.svg   {}x{}  {} 字节  {:.1}ms",
        card.width,
        card.height,
        card.png.len(),
        card_ms
    );

    // ---- 2) 词云（算法布局，非模板） ----
    // 刻意用长尾分布：字号按 sqrt(词频) 缩放、透明度同维度，
    // 线性映射会让尾部词全部挤在最小字号上。
    let words = vec![
        WordItem::new("签到", 100),
        WordItem::new("词云", 55),
        WordItem::new("骰子", 30),
        WordItem::new("机器人", 18),
        WordItem::new("渲染", 10),
        WordItem::new("性能", 6),
        WordItem::new("rust", 4),
        WordItem::new("异步", 3),
        WordItem::new("actor", 2),
        WordItem::new("缓存", 1),
    ];
    let svg = build_wordcloud_svg(&words, 900, 640, "自检 · 词云");
    let t1 = Instant::now();
    let cloud = render.render_svg(svg).await.context("词云渲染失败")?;
    let cloud_ms = t1.elapsed().as_secs_f64() * 1000.0;
    std::fs::write(out_dir.join("wordcloud.png"), &cloud.png)?;
    println!(
        "✓ wordcloud  {}x{}  {} 字节  {:.1}ms",
        cloud.width,
        cloud.height,
        cloud.png.len(),
        cloud_ms
    );

    // ---- 3) 缓存命中 ----
    let t2 = Instant::now();
    let _ = render
        .render_template(
            "card.svg",
            serde_json::json!({
                "title": "自检 · 模板渲染",
                "rows": [
                    {"label": "渲染后端", "value": "resvg (纯 Rust)"},
                    {"label": "外部依赖", "value": "无 Chromium / 无 Node"},
                    {"label": "中文字体", "value": "系统字体"},
                    {"label": "输出倍率", "value": "2.0x"}
                ],
                "width": 720,
                "height": 340
            }),
        )
        .await?;
    println!("✓ 缓存命中   二次渲染 {:?}", t2.elapsed());

    let stats = render.stats();
    println!("渲染统计: {stats:?}");
    println!("产物目录: {}", out_dir.display());
    Ok(())
}

/// 只校验凭据，不建立长连接。
async fn check(cfg: Config) -> Result<()> {
    println!("AppID = {}（密钥长度 {}）", cfg.app_id, cfg.client_secret.len());

    let api = ApiClient::new(cfg.api_config())?;
    let token = api.token().await.context("获取 access_token 失败")?;
    println!("✓ access_token 获取成功（长度 {}）", token.len());

    let info = api.gateway().await.context("获取网关信息失败")?;
    println!(
        "✓ 网关地址 = {}，建议分片 = {}，剩余 session 额度 = {:?}",
        info.url,
        info.shards,
        info.session_start_limit.as_ref().map(|s| s.remaining)
    );
    Ok(())
}

/// 正常服务模式。
async fn run(cfg: Config) -> Result<()> {
    let api = ApiClient::new(cfg.api_config())?;

    let media = MediaUploader::new(api.clone());
    let render = RenderService::new(cfg.render.clone());
    let sessions = Arc::new(SessionRegistry::new(
        api.clone(),
        cfg.session_shards,
        256,
        Duration::from_secs(10),
    ));
    // 消息持久化。**打开失败不致命**：降级为内存语料，机器人照常服务。
    let store = match &cfg.store {
        Some(sc) => match MessageStore::open_async(sc.clone()).await {
            Ok(s) => {
                // 第一次 tick 立即触发 → 进程启动就会清理一次过期消息
                s.spawn_sweeper();
                tracing::info!(
                    path = %sc.path.display(),
                    retention_days = sc.retention_days(),
                    "消息持久化已启用"
                );
                Some(s)
            }
            Err(err) => {
                tracing::error!(error = %err, "消息存储打开失败，词云将退化为内存语料");
                None
            }
        },
        None => {
            tracing::warn!("未启用消息持久化（QQBOT_DB_PATH 为空），词云只用内存语料");
            None
        }
    };

    // 资源库与消息库**共用同一个文件**，但用独立连接：
    // 资源操作由管理命令触发，频率是人手级别，没必要挤进消息的批量写线程。
    // 打开失败不致命，降级为「不注册资源命令」。
    // 资源管理依赖**两个**库：资源映射，以及消息库（收录时要回查上一条带图片的消息）。
    // 所以只有消息库确实打开了才启用它。
    let resources = match (&cfg.store, &store) {
        (Some(sc), Some(messages)) => {
            match qqbot_store::open_resource_store(sc.path.clone()).await {
                Ok(rs) => {
                    tracing::info!(basepath = %cfg.resources_basepath.display(), "资源管理已启用");
                    Some(qqbot_plugins::ResourcesConfig {
                        store: Arc::new(rs),
                        messages: Arc::clone(messages),
                        basepath: cfg.resources_basepath.clone(),
                        controllers: cfg.system_controllers.clone(),
                    })
                }
                Err(err) => {
                    tracing::error!(error = %err, "资源库打开失败，资源管理命令不可用");
                    None
                }
            }
        }
        _ => {
            tracing::warn!("未启用消息持久化，资源管理命令不可用");
            None
        }
    };

    let mut services = Services::new(api.clone(), media, render, sessions);
    if let Some(s) = &store {
        services = services.with_store(Arc::clone(s));
    }
    let services = Arc::new(services);

    let mut router = Router::new();
    qqbot_plugins::register(
        &mut router,
        store.clone(),
        &qqbot_plugins::PluginsConfig {
            wordcloud_window: cfg.wordcloud_window,
            daily: cfg.daily.clone(),
            resources: resources.clone(),
            tarkov: qqbot_plugins::TarkovConfig::default(),
            ba: qqbot_plugins::BaConfig::default(),
            bili: qqbot_plugins::BiliConfig::default(),
            bangumi: qqbot_plugins::BangumiConfig::default(),
            ammo: qqbot_plugins::AmmoConfig::default(),
            task: qqbot_plugins::TaskConfig::default(),
            market: qqbot_plugins::MarketConfig::default(),
            delta: qqbot_plugins::DeltaConfig::default(),
        },
    )
    .await
    .context("初始化插件失败")?;
    let route_count = router.len();

    let dispatcher = Arc::new(Dispatcher::new(
        router,
        services.clone(),
        DispatchConfig {
            dedup_capacity: cfg.dedup_capacity,
            concurrency: cfg.dispatch_concurrency,
            ..DispatchConfig::default()
        },
    ));

    tracing::info!(
        app_id = %cfg.app_id,
        routes = route_count,
        session_shards = cfg.session_shards,
        "启动中"
    );

    let gateway = spawn_gateway(
        api,
        GatewayConfig {
            intents: cfg.intents,
            shards: cfg.shards,
            event_buffer: cfg.event_buffer,
            url_override: cfg.gateway_url.clone(),
        },
    )
    .await
    .context("建立网关连接失败")?;

    let mut gateway = gateway;
    tracing::info!(shards = gateway.shard_count(), "网关已连接，开始接收事件");

    // ⚠️ 事件处理必须**派生**出去，不能在 select 分支里直接 await：
    // 否则主循环要等 handle() 完全返回才去 poll gateway.recv()，
    // dispatch 里的 Semaphore(concurrency) 永远只能拿到 1 个许可，
    // 一条慢命令（如词云）会阻塞所有消息处理，还会把网关事件挤到丢弃。
    let mut inflight = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("收到退出信号");
                break;
            }
            event = gateway.recv() => {
                match event {
                    Some(event) => {
                        let dispatcher = Arc::clone(&dispatcher);
                        inflight.spawn(async move { dispatcher.handle(event).await });
                    }
                    None => {
                        tracing::warn!("事件通道已关闭");
                        break;
                    }
                }
            }
            // 回收已完成的任务，避免 JoinSet 无界增长。
            Some(_) = inflight.join_next(), if !inflight.is_empty() => {}
        }
    }

    // 给在途事件一个收尾窗口；超时则强制取消，不为了几个慢插件卡住退出。
    if tokio::time::timeout(Duration::from_secs(10), async {
        while inflight.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tracing::warn!(inflight = inflight.len(), "在途事件未在 10s 内处理完，强制退出");
        inflight.shutdown().await;
    }

    gateway.shutdown().await;
    tracing::info!("已退出");
    Ok(())
}
