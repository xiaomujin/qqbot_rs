use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use regex::Regex;

use crate::ctx::Ctx;

/// 处理结果，对应 Shiro 的 `MESSAGE_BLOCK` / `MESSAGE_IGNORE`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// 已消费，终止责任链。
    Consumed,
    /// 继续交给下一个规则。
    Next,
}

/// 命令处理器。
#[async_trait]
pub trait Handler: Send + Sync + 'static {
    async fn handle(&self, ctx: &Ctx) -> Handled;

    /// 插件名，用于日志与指标。
    fn name(&self) -> &str {
        "handler"
    }
}

/// 闭包返回的装箱 Future。
///
/// Rust 的 `Fn(&Ctx) -> impl Future` 无法表达「返回的 Future 借用参数」这一约束
/// （即 `for<'a> Fn(&'a Ctx) -> Fut<'a>`），因此这里显式装箱。
pub type BoxedHandlerFuture<'a> = Pin<Box<dyn Future<Output = Handled> + Send + 'a>>;

/// 闭包形式的处理器，便于写简单插件与测试。
///
/// ```ignore
/// FnHandler::new("echo", |ctx| Box::pin(async move {
///     ctx.reply_text("hi").await.ok();
///     Handled::Consumed
/// }))
/// ```
pub struct FnHandler<F> {
    name: String,
    f: F,
}

impl<F> FnHandler<F>
where
    F: for<'a> Fn(&'a Ctx) -> BoxedHandlerFuture<'a> + Send + Sync + 'static,
{
    pub fn new(name: impl Into<String>, f: F) -> Self {
        Self { name: name.into(), f }
    }
}

#[async_trait]
impl<F> Handler for FnHandler<F>
where
    F: for<'a> Fn(&'a Ctx) -> BoxedHandlerFuture<'a> + Send + Sync + 'static,
{
    async fn handle(&self, ctx: &Ctx) -> Handled {
        (self.f)(ctx).await
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// 匹配器。全部针对**去掉首尾空白后的消息全文**匹配。
#[derive(Debug, Clone)]
pub enum Matcher {
    Any,
    /// 全文完全相等。
    Exact(String),
    /// 全文前缀。
    Prefix(String),
    /// 首个空格分隔的 token 相等（`"帮助 签到"` 也能命中 `Command("帮助")`）。
    Command(String),
    /// 正则。
    Regex(Regex),
}

impl Matcher {
    pub fn matches(&self, input: &str) -> bool {
        match self {
            Matcher::Any => true,
            Matcher::Exact(s) => input == s,
            Matcher::Prefix(p) => input.starts_with(p.as_str()),
            Matcher::Command(c) => input.split_whitespace().next() == Some(c.as_str()),
            Matcher::Regex(re) => re.is_match(input),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Matcher::Any => "*".to_string(),
            Matcher::Exact(s) => s.clone(),
            Matcher::Prefix(p) => format!("{p}*"),
            Matcher::Command(c) => c.clone(),
            Matcher::Regex(re) => format!("/{}/", re.as_str()),
        }
    }
}

/// 生效场景。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Group,
    C2c,
    Any,
}

impl Scope {
    pub fn accepts(self, is_group: bool) -> bool {
        match self {
            Scope::Any => true,
            Scope::Group => is_group,
            Scope::C2c => !is_group,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Scope::Group => "群聊",
            Scope::C2c => "单聊",
            Scope::Any => "全部",
        }
    }
}

/// 一条路由规则。
pub struct Rule {
    pub name: String,
    pub scope: Scope,
    pub matcher: Matcher,
    /// 数值越大越先执行。
    pub priority: i32,
    pub handler: Arc<dyn Handler>,
    /// 不在帮助里展示。用于 `Matcher::Any` 这类「监听器」——它们不是用户可见命令。
    pub hidden: bool,
}

/// 路由表信息（用于自动生成帮助）。
#[derive(Debug, Clone)]
pub struct RouteInfo {
    pub name: String,
    pub scope: Scope,
    pub matcher: String,
    pub priority: i32,
    pub hidden: bool,
}

/// 显式路由表 + 责任链。
///
/// 选择显式路由表而非过程宏注解，理由：
/// 1. 路由表是**数据**，可打印、可单测、可自动生成帮助
/// 2. Rust 过程宏的编译错误定位差，会显著拖慢迭代
/// 3. 注册发生在编译期，不需要运行期扫描
#[derive(Default)]
pub struct Router {
    rules: Vec<Rule>,
}

impl Router {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// 注册规则。插入后按 priority 稳定排序（同优先级保持注册顺序）。
    pub fn add(&mut self, rule: Rule) {
        self.rules.push(rule);
        self.rules.sort_by_key(|r| std::cmp::Reverse(r.priority));
    }

    pub fn on<H: Handler>(
        &mut self,
        scope: Scope,
        matcher: Matcher,
        priority: i32,
        handler: H,
    ) -> &mut Self {
        let name = handler.name().to_string();
        self.add(Rule { name, scope, matcher, priority, handler: Arc::new(handler), hidden: false });
        self
    }

    /// 注册一个**监听器**：它会看到所有消息（返回 [`Handled::Next`] 不打断责任链），
    /// 但不会出现在自动生成的帮助里。
    pub fn on_listener<H: Handler>(&mut self, matcher: Matcher, handler: H) -> &mut Self {
        let name = handler.name().to_string();
        self.add(Rule {
            name,
            scope: Scope::Any,
            matcher,
            priority: 0,
            handler: Arc::new(handler),
            hidden: true,
        });
        self
    }

    pub fn on_group<H: Handler>(&mut self, matcher: Matcher, handler: H) -> &mut Self {
        self.on(Scope::Group, matcher, 0, handler)
    }

    pub fn on_c2c<H: Handler>(&mut self, matcher: Matcher, handler: H) -> &mut Self {
        self.on(Scope::C2c, matcher, 0, handler)
    }

    pub fn on_any<H: Handler>(&mut self, matcher: Matcher, handler: H) -> &mut Self {
        self.on(Scope::Any, matcher, 0, handler)
    }

    pub fn routes(&self) -> Vec<RouteInfo> {
        self.rules
            .iter()
            .map(|r| RouteInfo {
                name: r.name.clone(),
                scope: r.scope,
                matcher: r.matcher.describe(),
                priority: r.priority,
                hidden: r.hidden,
            })
            .collect()
    }

    /// 按顺序执行责任链，直到某条规则返回 [`Handled::Consumed`]。
    pub async fn dispatch(&self, ctx: &Ctx) -> Handled {
        let is_group = ctx.is_group();
        for rule in &self.rules {
            if !rule.scope.accepts(is_group) {
                continue;
            }
            if !rule.matcher.matches(ctx.content()) {
                continue;
            }

            let started = Instant::now();
            let outcome = rule.handler.handle(ctx).await;
            metrics::histogram!("qqbot_plugin_duration_seconds", "plugin" => rule.name.clone())
                .record(started.elapsed().as_secs_f64());

            tracing::debug!(
                plugin = %rule.name,
                matcher = %rule.matcher.describe(),
                outcome = ?outcome,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "插件执行完成"
            );

            if outcome == Handled::Consumed {
                return Handled::Consumed;
            }
        }
        Handled::Next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matchers_behave_as_documented() {
        assert!(Matcher::Any.matches("任意内容"));
        assert!(Matcher::Exact("签到".into()).matches("签到"));
        assert!(!Matcher::Exact("签到".into()).matches("签到 2"));
        assert!(Matcher::Prefix("塔科夫 ".into()).matches("塔科夫 价格"));
        assert!(!Matcher::Prefix("塔科夫 ".into()).matches("塔科夫"));
        assert!(Matcher::Command("帮助".into()).matches("帮助 签到"));
        assert!(Matcher::Command("帮助".into()).matches("帮助"));
        assert!(!Matcher::Command("帮助".into()).matches("帮助我"));
        assert!(Matcher::Regex(Regex::new(r"^1[3-9]\d{9}$").unwrap()).matches("13800138000"));
    }

    #[test]
    fn scope_filters_by_chat_type() {
        assert!(Scope::Any.accepts(true));
        assert!(Scope::Any.accepts(false));
        assert!(Scope::Group.accepts(true));
        assert!(!Scope::Group.accepts(false));
        assert!(Scope::C2c.accepts(false));
        assert!(!Scope::C2c.accepts(true));
    }

    fn noop(name: &str) -> FnHandler<impl for<'a> Fn(&'a Ctx) -> BoxedHandlerFuture<'a> + Send + Sync + 'static> {
        let owned = name.to_string();
        FnHandler::new(owned, |_ctx| Box::pin(async { Handled::Next }))
    }

    #[test]
    fn rules_are_ordered_by_priority_then_insertion() {
        let mut r = Router::new();
        r.on(Scope::Any, Matcher::Exact("a".into()), 0, noop("low"));
        r.on(Scope::Any, Matcher::Exact("b".into()), 10, noop("high"));
        r.on(Scope::Any, Matcher::Exact("c".into()), 10, noop("high2"));

        let names: Vec<String> = r.routes().into_iter().map(|x| x.name).collect();
        assert_eq!(names, vec!["high", "high2", "low"]);
    }

    #[test]
    fn routes_are_describable_for_help() {
        let mut r = Router::new();
        r.on_group(Matcher::Exact("签到".into()), noop("签到"));
        let info = r.routes();
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].name, "签到");
        assert_eq!(info[0].matcher, "签到");
        assert_eq!(info[0].scope.label(), "群聊");
    }
}
