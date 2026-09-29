//! 业务插件与路由表注册。
//!
//! 注册顺序即责任链顺序（同优先级下），靠前的插件先执行。

pub mod daily;
pub mod dice;
pub mod help;
pub mod http;
pub mod resource;
pub mod timewin;
pub mod wordcloud;

pub use daily::{DailyConfig, DailyPlugin};
pub use dice::{parse_spec, DicePlugin};
pub use help::HelpPlugin;
pub use resource::{register_resources, ResourcesConfig};
pub use wordcloud::{tokenize, WordCloudPlugin};

use std::sync::Arc;
use std::time::Duration;

use qqbot_core::{Matcher, Router};
use qqbot_store::MessageStore;

/// 插件注册所需的配置。
///
/// 做成结构体而不是继续加参数：迁移中的插件数量还会增长，
/// 每加一个功能就改一次 `register` 签名会让调用点反复变动。
// 不派生 Debug：ResourcesConfig 里的连接池没有有意义的 Debug 表示。
#[derive(Clone, Default)]
pub struct PluginsConfig {
    /// 词云统计窗口。
    pub wordcloud_window: Duration,
    /// 日报配置。`None` 表示未配置 token，**该命令不会被注册**。
    pub daily: Option<DailyConfig>,
    /// 资源管理配置。`None` 表示未启用持久化，相关命令不会被注册。
    pub resources: Option<ResourcesConfig>,
}

/// 把所有内置插件注册到路由表。
///
/// 帮助插件必须**最后**注册：它需要读取此前注册的全部路由来生成指令表。
///
/// `store` 为 `None` 时词云只用内存语料（进程重启即清空）。
///
/// # Errors
///
/// - 共享 HTTP 客户端构建失败（TLS 后端不可用）；
/// - 资源库读取失败。
///
/// 两者都应当在启动阶段暴露，而不是留到用户第一次触发命令。
pub async fn register(
    router: &mut Router,
    store: Option<Arc<MessageStore>>,
    cfg: &PluginsConfig,
) -> anyhow::Result<()> {
    // 统一超时与连接池。即使当前没有插件用到也建一个：成本可忽略，
    // 而省掉了「以后新增网络插件时忘记加超时」这类问题。
    let http = http::build_client()?;

    router.on_any(Matcher::Command("ping".into()), PingPlugin);
    router.on_any(Matcher::Command("骰子".into()), DicePlugin);
    router.on_any(Matcher::Command("roll".into()), DicePlugin);
    router.on_any(Matcher::Command("r".into()), DicePlugin);

    // 日报：精确匹配。全量模式下群消息都会到达，宽匹配会频繁误触发。
    if let Some(daily) = &cfg.daily {
        router.on_any(Matcher::Exact("日报".into()), DailyPlugin::new(daily.clone(), http.clone()));
    }

    // 资源管理：注册一条最低优先级的通配监听器 + 若干显式命令。
    if let Some(res) = &cfg.resources {
        register_resources(router, res.clone(), http.clone()).await?;
    }

    // 词云：同一个实例既负责渲染命令，也负责静默累积语料。
    let wordcloud = WordCloudPlugin::with_store(store, cfg.wordcloud_window);
    router.on_any(Matcher::Command("词云".into()), wordcloud.clone());
    // cq-bot 的 8 种组合。**整串精确匹配** —— 全量模式下机器人能看到所有群消息，
    // 用前缀匹配会让「本群今日词云好看吗」这类闲聊也触发一次渲染。
    for keyword in [
        "我的今日词云",
        "我的本周词云",
        "我的本月词云",
        "我的本年词云",
        "本群今日词云",
        "本群本周词云",
        "本群本月词云",
        "本群本年词云",
    ] {
        router.on_any(Matcher::Exact(keyword.into()), wordcloud.clone());
    }
    // 通配监听器：只用于累积语料，不作为用户可见命令出现在帮助里。
    router.on_listener(Matcher::Any, wordcloud.clone());

    let routes = router.routes();
    router.on_any(Matcher::Command("帮助".into()), HelpPlugin::new(routes));

    // 回填命令表：词云需要它把「命令调用」从语料里剔除。
    // Matcher::Command 的 describe() 就是命令名本身，因此可以直接推导，
    // 不必在这里重复维护一份命令列表。
    let commands: Vec<String> = router
        .routes()
        .into_iter()
        .filter(|r| {
            !r.matcher.is_empty()
                && r.matcher != "*"
                && !r.matcher.starts_with('/')
                && !r.matcher.ends_with('*')
        })
        .map(|r| r.matcher)
        .collect();
    wordcloud.set_commands(commands);

    tracing::info!(count = router.len(), "插件已注册");
    Ok(())
}

/// 存活探测。
pub struct PingPlugin;

#[async_trait::async_trait]
impl qqbot_core::Handler for PingPlugin {
    async fn handle(&self, ctx: &qqbot_core::Ctx) -> qqbot_core::Handled {
        let text = format!("pong · {}", ctx.services.bot_name());
        if let Err(err) = ctx.reply_text(text).await {
            tracing::warn!(error = %err, "ping 回复失败");
        }
        qqbot_core::Handled::Consumed
    }

    fn name(&self) -> &'static str {
        "ping"
    }
}
