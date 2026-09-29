use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler, RouteInfo, Scope};

/// 帮助插件：根据路由表自动生成命令列表。
///
/// 这是选择「显式路由表」而非注解扫描的直接收益——帮助文档永远与实现同步。
pub struct HelpPlugin {
    routes: Vec<RouteInfo>,
}

impl HelpPlugin {
    pub fn new(routes: Vec<RouteInfo>) -> Self {
        Self { routes }
    }

    /// 生成 Markdown 帮助文本。
    pub fn render_markdown(&self, bot_name: &str) -> String {
        let mut out = format!("**{} 指令表**\n\n", bot_name);
        let mut any = false;
        for scope in [Scope::Group, Scope::C2c, Scope::Any] {
            let items: Vec<&RouteInfo> = self
                .routes
                .iter()
                .filter(|r| r.scope == scope && r.name != "帮助" && !r.hidden)
                .collect();
            if items.is_empty() {
                continue;
            }
            any = true;
            out.push_str(&format!("【{}】\n", scope.label()));

            // 同一插件的多个别名（骰子 / roll / r）合并成一行，避免帮助列表被别名淹没。
            let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
            for item in items {
                match groups.iter_mut().find(|(name, _)| *name == item.name.as_str()) {
                    Some((_, matchers)) => matchers.push(item.matcher.as_str()),
                    None => groups.push((item.name.as_str(), vec![item.matcher.as_str()])),
                }
            }
            for (_, matchers) in groups {
                let line = matchers
                    .iter()
                    .map(|m| format!("`{m}`"))
                    .collect::<Vec<_>>()
                    .join(" / ");
                out.push_str("- ");
                out.push_str(&line);
                out.push('\n');
            }
            out.push('\n');
        }
        if !any {
            out.push_str("暂未注册任何指令。\n");
        }
        out.push_str("发送 `帮助` 查看本列表。");
        out
    }
}

#[async_trait]
impl Handler for HelpPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        let text = self.render_markdown(&ctx.services.bot_name());
        if let Err(err) = ctx.reply_markdown(text).await {
            tracing::warn!(error = %err, "帮助回复失败");
        }
        Handled::Consumed
    }

    fn name(&self) -> &'static str {
        "帮助"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(name: &str, scope: Scope, matcher: &str) -> RouteInfo {
        RouteInfo { name: name.into(), scope, matcher: matcher.into(), priority: 0, hidden: false }
    }

    #[test]
    fn groups_commands_by_scope_and_hides_self() {
        let p = HelpPlugin::new(vec![
            route("签到", Scope::Group, "签到"),
            route("骰子", Scope::Any, "骰子"),
            route("帮助", Scope::Any, "帮助"),
        ]);
        let md = p.render_markdown("黑猫Bot");
        assert!(md.contains("黑猫Bot 指令表"));
        assert!(md.contains("【群聊】"));
        assert!(md.contains("【全部】"));
        assert!(md.contains("- `签到`"), "缺少签到: {md}");
        assert!(md.contains("- `骰子`"), "缺少骰子: {md}");
        assert!(!md.contains("- `帮助`"), "帮助不应把自己列进去: {md}");
    }

    #[test]
    fn hidden_listeners_are_not_listed() {
        let mut routes = vec![route("签到", Scope::Group, "签到")];
        routes.push(RouteInfo {
            name: "词云".into(),
            scope: Scope::Any,
            matcher: "*".into(),
            priority: 0,
            hidden: true,
        });
        let md = HelpPlugin::new(routes).render_markdown("bot");
        assert!(md.contains("- `签到`"), "可见命令应保留: {md}");
        assert!(!md.contains("- `*`"), "隐藏的通配监听器不应出现: {md}");
    }

    #[test]
    fn aliases_of_the_same_plugin_share_one_line() {
        let p = HelpPlugin::new(vec![
            route("骰子", Scope::Any, "骰子"),
            route("骰子", Scope::Any, "roll"),
            route("骰子", Scope::Any, "r"),
        ]);
        let md = p.render_markdown("bot");
        assert!(
            md.contains("-`骰子` / `roll` / `r`") || md.contains("- `骰子` / `roll` / `r`"),
            "别名应合并成一行: {md}"
        );
    }

    #[test]
    fn empty_router_still_renders() {
        let p = HelpPlugin::new(vec![]);
        assert!(p.render_markdown("bot").contains("暂未注册任何指令"));
    }
}
