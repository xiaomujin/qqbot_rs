//! 业务插件与路由表注册。
//!
//! 注册顺序即责任链顺序（同优先级下），靠前的插件先执行。

pub mod dice;
pub mod help;
pub mod wordcloud;

pub use dice::{parse_spec, DicePlugin};
pub use help::HelpPlugin;
pub use wordcloud::{tokenize, WordCloudPlugin};

use std::sync::Arc;
use std::time::Duration;

use qqbot_core::{Matcher, Router};
use qqbot_store::MessageStore;

/// 把所有内置插件注册到路由表。
///
/// 帮助插件必须**最后**注册：它需要读取此前注册的全部路由来生成指令表。
///
/// `store` 为 `None` 时词云只用内存语料（进程重启即清空）。
pub fn register(router: &mut Router, store: Option<Arc<MessageStore>>, window: Duration) {
    router.on_any(Matcher::Command("ping".into()), PingPlugin);
    router.on_any(Matcher::Command("骰子".into()), DicePlugin);
    router.on_any(Matcher::Command("roll".into()), DicePlugin);
    router.on_any(Matcher::Command("r".into()), DicePlugin);

    // 词云：同一个实例既负责渲染命令，也负责静默累积语料。
    let wordcloud = WordCloudPlugin::with_store(store, window);
    router.on_any(Matcher::Command("词云".into()), wordcloud.clone());
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
