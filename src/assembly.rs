//! 服务栈装配。
//!
//! 产品路径（[`assemble`]）与测试路径（[`assemble_with`]）共用**同一段**
//! 装配代码。这不是为了少写几行：在此之前 e2e 把这里的装配手抄了一遍，很快就
//! 漂移了 —— 渲染超时、会话分片数、去重容量各写各的，而 `spawn_sweeper`
//! （保留期清理）在 e2e 里**一次都没被走到**。让两边走同一个函数，这类漂移
//! 就不可能再出现。
//!
//! **网关不在这里**：它要建立真实的 WebSocket 长连接，而 e2e 是直接把事件喂给
//! `Dispatcher` 的。`main.rs` 负责 `spawn_gateway` 与事件主循环。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use qqbot_api::ApiClient;
use qqbot_core::{DispatchConfig, Dispatcher, Router, Services, SessionRegistry};
use qqbot_media::MediaUploader;
use qqbot_plugins::{
    AmmoConfig, BaConfig, BangumiConfig, BiliConfig, DeltaConfig, MarketConfig, PluginsConfig,
    ResourcesConfig, TarkovConfig, TaskConfig,
};
use qqbot_render::RenderService;
use qqbot_store::MessageStore;

use crate::config::Config;

/// 装配好的服务栈（不含网关）。
pub struct App {
    /// 与 [`Services`] 内部**同一个** `ApiClient`（克隆共享同一个 token 缓存）。
    ///
    /// 网关（`main.rs` 的 `spawn_gateway`）用它，所以进程里只有一份 token 缓存。
    /// 它不在插件面上 —— 插件拿到的是 `Ctx`，看不到 `App`。
    pub api: ApiClient,
    /// 共享服务集合：发送链路、渲染、上传、会话分片。
    pub services: Arc<Services>,
    /// 事件分发器。网关事件喂给它，e2e 也直接喂它。
    pub dispatcher: Arc<Dispatcher>,
    /// 消息库。`None` 表示持久化未启用或打开失败（词云退化为内存语料）。
    pub store: Option<Arc<MessageStore>>,
    /// 资源库配置。`None` 表示资源管理命令不可用。
    pub resources: Option<ResourcesConfig>,
    /// 注册的路由条数（启动日志与帮助表都用它）。
    pub route_count: usize,
}

/// 产品路径：插件配置由 [`Config`] 推导（全部指向真实上游）。
///
/// # Errors
///
/// API 客户端构建失败（TLS 后端不可用）或插件初始化失败。
pub async fn assemble(cfg: &Config) -> Result<App> {
    assemble_with(cfg, plugins_from_config(cfg)).await
}

/// 测试路径：调用方给出指向 mock 的 [`PluginsConfig`]。
///
/// `plugins.resources` 会被**覆盖**成按 `cfg.store` 打开的实例 ——
/// 资源库怎么开只此一处实现，调用方给什么都一样。要断言资源库的用例
/// 从 [`App::resources`] 取回句柄。
///
/// # Errors
///
/// 同 [`assemble`]。
pub async fn assemble_with(cfg: &Config, mut plugins: PluginsConfig) -> Result<App> {
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
                    Some(ResourcesConfig {
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

    // 资源库由装配负责打开，覆盖调用方给的值 —— 否则「资源库怎么开」又会有
    // 第二份实现，而两份实现迟早会分叉。
    plugins.resources = resources.clone();

    let mut router = Router::new();
    qqbot_plugins::register(&mut router, store.clone(), &plugins)
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

    Ok(App {
        api,
        services,
        dispatcher,
        store,
        resources,
        route_count,
    })
}

/// 从配置推导插件配置。
///
/// `resources` 刻意留空：它由 [`assemble_with`] 按 `cfg.store` 打开后回填，
/// 这样「资源库怎么开」只有一处实现。
fn plugins_from_config(cfg: &Config) -> PluginsConfig {
    PluginsConfig {
        wordcloud_window: cfg.wordcloud_window,
        daily: cfg.daily.clone(),
        resources: None,
        tarkov: TarkovConfig {
            // 没配就不注册那个插件：这批图是固定的 19 个，
            // 没放图时注册了也只会报「读取失败」。
            images_dir: cfg.tarkov_images_dir.clone(),
            ..Default::default()
        },
        ba: BaConfig::default(),
        bili: BiliConfig::default(),
        bangumi: BangumiConfig::default(),
        ammo: AmmoConfig::default(),
        task: TaskConfig::default(),
        market: MarketConfig::default(),
        delta: DeltaConfig::default(),
    }
}
