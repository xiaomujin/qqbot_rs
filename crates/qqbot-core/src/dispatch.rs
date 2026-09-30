use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use moka::sync::Cache;
use qqbot_api::{Event, InteractionResponse};
use qqbot_store::{now_unix, NewMessage, Scope};
use tokio::sync::Semaphore;

use crate::ctx::{Ctx, Services};
use crate::plugin::{Handled, InteractionCtx, Router};

#[derive(Debug, Clone)]
pub struct DispatchConfig {
    /// 事件去重缓存条数。
    pub dedup_capacity: u64,
    /// 去重保留时长。QQ 会重推相同 `msg_id`，必须幂等。
    pub dedup_ttl: Duration,
    /// 同时处理的插件链数量上限。
    pub concurrency: usize,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            dedup_capacity: 8192,
            dedup_ttl: Duration::from_secs(600),
            concurrency: 16,
        }
    }
}

/// 事件分发器：去重 → 登记被动窗口 → 路由。
pub struct Dispatcher {
    /// 用 `Arc` 持有：互动事件必须派生到独立任务里处理，
    /// 而派生要求 `'static`，不能借用 `&self`。
    router: Arc<Router>,
    services: Arc<Services>,
    dedup: Cache<String, ()>,
    sem: Arc<Semaphore>,
}

impl Dispatcher {
    pub fn new(router: Router, services: Arc<Services>, cfg: DispatchConfig) -> Self {
        let dedup = Cache::builder()
            .max_capacity(cfg.dedup_capacity)
            .time_to_live(cfg.dedup_ttl)
            .build();
        Self {
            router: Arc::new(router),
            services,
            dedup,
            sem: Arc::new(Semaphore::new(cfg.concurrency.max(1))),
        }
    }

    pub fn routes(&self) -> Vec<crate::plugin::RouteInfo> {
        self.router.routes()
    }

    pub fn services(&self) -> &Arc<Services> {
        &self.services
    }

    /// 处理一个网关事件。
    pub async fn handle(&self, event: Arc<Event>) {
        // 指标标签要求 'static，这里转成 owned String。
        metrics::counter!("qqbot_events_total", "name" => event.name().to_string()).increment(1);

        match event.as_ref() {
            Event::Ready(ready) => {
                self.services.set_bot_name(ready.user.username.clone());
                tracing::info!(
                    user = %ready.user.username,
                    session_id = %ready.session_id,
                    "机器人已就绪"
                );
            }
            Event::Resumed => {
                tracing::info!("事件补发完成（RESUMED）");
            }
            Event::C2cMessage(m) | Event::GroupAtMessage(m) | Event::GroupMessage(m) => {
                self.handle_message(event.name(), m.clone()).await;
            }
            Event::Interaction(i) => {
                // 1) 幂等：同一个 interaction_id 只能回应一次，重推直接丢弃。
                //    去重放在派生**之前** —— `dedup` 的 get/insert 不是原子操作，
                //    放进 spawn 里两个重推事件可能同时通过检查。
                let key = format!("{}:{}", event.name(), i.id);
                if self.dedup.get(&key).is_some() {
                    metrics::counter!("qqbot_events_dedup_total").increment(1);
                    tracing::debug!(interaction_id = %i.id, "重复互动事件，已忽略");
                    return;
                }
                self.dedup.insert(key, ());

                // 2) 官方给互动事件的回应超时只有 3 秒，这条路径要先走一次 HTTP。
                //    派生出去：事件循环立刻返回，也不占用 dispatch 的并发许可。
                let services = self.services.clone();
                let router = self.router.clone();
                let interaction = i.clone();
                tokio::spawn(async move {
                    handle_interaction(services, router, interaction).await;
                });
            }
            other => {
                // 其余事件（加群、被撤回、开关变更等）在排障时同样需要可见
                tracing::info!(name = other.name(), "收到非消息事件");
            }
        }
    }

    async fn handle_message(&self, name: &str, message: Arc<qqbot_api::MessageEvent>) {
        // 1) 幂等：相同 msg_id 可能被多次推送
        let key = format!("{name}:{}", message.id);
        if self.dedup.get(&key).is_some() {
            metrics::counter!("qqbot_events_dedup_total").increment(1);
            tracing::debug!(msg_id = %message.id, "重复事件，已忽略");
            return;
        }
        self.dedup.insert(key, ());

        let Some(target) = message.target() else {
            tracing::warn!(msg_id = %message.id, "无法推导发送目标，跳过");
            return;
        };

        // 2) 登记被动回复窗口。窗口从**收到消息**开始计时。
        self.services.sessions.observe(&target, &message.id);

        // 3) 入库。**非阻塞**：store.record 只做一次 try_send，
        //    队列满时丢弃并计数，绝不拖慢消息处理热路径。
        if let Some(store) = &self.services.store {
            store.record(NewMessage {
                id: message.id.clone(),
                scope: if target.is_group() { Scope::Group } else { Scope::C2c },
                target_id: target.id().to_string(),
                sender_id: message.sender_openid().map(str::to_string),
                sender_name: message.author.username.clone(),
                event_name: name.to_string(),
                content: message.trimmed().to_string(),
                // 原文一并入库：附件不在 content 里，只有它能还原完整消息。
                raw: message.raw.clone(),
                created_at: event_unix(&message),
            });
        }

        let Some(ctx) = Ctx::new(self.services.clone(), message) else {
            return;
        };

        // 用 INFO：这是运维判断「事件到底有没有送达」的唯一依据。
        // 正文截断，避免把整篇消息灌进日志；但**附件地址与元素个数不截断** ——
        // 「发了图却没被识别」这类问题只能靠它们定位。
        // 完整原始载荷见 `qqbot_api::event` 的 DEBUG 日志。
        tracing::info!(
            event = name,
            msg_id = %ctx.message_id(),
            target = %target.key(),
            content = %truncate_for_log(ctx.content(), 60),
            attachments = ctx.message.attachments.len(),
            attachment_url = %ctx
                .message
                .first_attachment()
                .and_then(|a| a.url.as_deref())
                .unwrap_or("-"),
            elements = ctx.message.msg_elements.len(),
            "收到消息"
        );

        // 3) 责任链。用 catch_unwind 隔离插件 panic，避免拖垮整个机器人。
        let _permit = match self.sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return,
        };

        let started = Instant::now();
        match AssertUnwindSafe(self.router.dispatch(&ctx)).catch_unwind().await {
            Ok(Handled::Consumed) => {
                metrics::counter!("qqbot_dispatch_total", "result" => "consumed").increment(1);
            }
            Ok(Handled::Next) => {
                metrics::counter!("qqbot_dispatch_total", "result" => "unhandled").increment(1);
                tracing::debug!(content = %truncate_for_log(ctx.content(), 60), "没有插件处理该消息");
            }
            Err(_) => {
                metrics::counter!("qqbot_plugin_panic_total").increment(1);
                // 与上面两处保持一致：完整正文会灌爆日志，也会把用户内容无截断落盘。
                tracing::error!(
                    content = %truncate_for_log(ctx.content(), 60),
                    "插件 panic 已被隔离"
                );
            }
        }
        metrics::histogram!("qqbot_dispatch_duration_seconds").record(started.elapsed().as_secs_f64());
    }
}

/// 处理按钮回调：先回应官方（3 秒超时），再交给插件责任链。
///
/// ⚠️ 调用方必须把它 `tokio::spawn` 出去 —— 这条路径有网络往返，
/// 在事件循环里 `await` 会拖住所有消息处理（AGENTS.md 的硬约束）。
async fn handle_interaction(
    services: Arc<Services>,
    router: Arc<Router>,
    event: Box<qqbot_api::event::notice::InteractionCreate>,
) {
    // 1) 官方要求 3 秒内回应，否则用户端一直转圈。失败只告警：
    //    回应失败不该中断后续的消息发送。
    if let Err(err) = services
        .api
        .respond_interaction(&event.id, InteractionResponse::default())
        .await
    {
        tracing::warn!(interaction_id = %event.id, error = %err, "回应互动事件失败");
    }

    // 2) 推导发送目标。
    let Some(target) = event.target() else {
        tracing::warn!(interaction_id = %event.id, "无法推导互动事件目标，跳过");
        return;
    };

    let ctx = InteractionCtx {
        services,
        target,
        sender_openid: event.sender_openid().map(str::to_string),
        event_id: event.event_id.clone(),
        // 取不到就退化成空串：插件按前缀匹配自然不会命中，
        // 但事件本身仍会走完责任链（便于日志排查）。
        button_data: event.button_data().unwrap_or_default(),
    };

    tracing::info!(
        interaction_id = %event.id,
        target = %ctx.target.key(),
        sender = ctx.sender_openid.as_deref().unwrap_or("-"),
        button_data = %truncate_for_log(&ctx.button_data, 60),
        "收到互动事件"
    );

    // 3) 责任链。用 catch_unwind 隔离插件 panic，与消息路径一致。
    match AssertUnwindSafe(router.dispatch_interaction(&ctx)).catch_unwind().await {
        Ok(Handled::Consumed) => {
            metrics::counter!("qqbot_dispatch_total", "result" => "consumed").increment(1);
        }
        Ok(Handled::Next) => {
            metrics::counter!("qqbot_dispatch_total", "result" => "unhandled").increment(1);
            tracing::debug!("没有插件处理该互动事件");
        }
        Err(_) => {
            metrics::counter!("qqbot_plugin_panic_total").increment(1);
            tracing::error!(
                button_data = %truncate_for_log(&ctx.button_data, 60),
                "插件 panic 已被隔离"
            );
        }
    }
}

/// 事件时间戳转 Unix 秒。
///
/// 线上 `timestamp` 有时是 Unix 整数、有时是 RFC3339 字符串，后者不引入日期库
/// 无法解析。异常值（解析失败、或与当前时间相差超过一天）一律回退到当前时间 ——
/// 否则一条时间戳异常的旧消息入库后会立刻被保留期清理掉。
fn event_unix(message: &qqbot_api::MessageEvent) -> i64 {
    let now = now_unix();
    match message.timestamp.as_deref().and_then(|s| s.parse::<i64>().ok()) {
        Some(ts) if is_plausible_ts(ts, now) => ts,
        _ => now,
    }
}

/// 服务端时间戳是否可信：与当前时间相差不超过一天。
///
/// 用 `abs_diff` 而不是 `(ts - now).abs()`：`ts` 来自服务端下发的任意字符串，
/// 可能是 `i64::MIN`，裸减法会溢出（debug panic / release 回绕）。
fn is_plausible_ts(ts: i64, now: i64) -> bool {
    ts.abs_diff(now) < 86_400
}

/// 日志里截断过长内容，避免把整篇消息灌进日志。
fn truncate_for_log(s: &str, max: usize) -> String {
    // 单遍完成。原实现先 `take(max).collect()` 再 `count()`，是两遍扫描，
    // 而这个函数对**每条消息**都会调用一次。
    let mut out = String::with_capacity(s.len().min(max.saturating_mul(4).saturating_add(3)));
    for (i, c) in s.chars().enumerate() {
        if i == max {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{FnHandler, Matcher, Scope};
    use qqbot_api::Event;
    use serde_json::value::RawValue;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn services() -> Arc<Services> {
        use crate::session::SessionRegistry;
        use qqbot_api::{ApiClient, ApiClientConfig};
        use qqbot_media::MediaUploader;
        use qqbot_render::{RenderConfig, RenderService};

        let api = ApiClient::new(ApiClientConfig::new("test-app", "test-secret")).unwrap();
        let media = MediaUploader::new(api.clone());
        let render = RenderService::new(RenderConfig::default());
        let sessions = Arc::new(SessionRegistry::new(api.clone(), 2, 16, Duration::from_secs(5)));
        Arc::new(Services::new(api, media, render, sessions))
    }

    fn event(name: &str, d: &str) -> Arc<Event> {
        let raw: Box<RawValue> = serde_json::from_str(d).unwrap();
        Arc::new(Event::parse(Some(name), Some(&raw), None).unwrap().unwrap())
    }

    #[test]
    fn implausible_timestamps_are_rejected_without_overflow() {
        let now = 1_700_000_000i64;
        assert!(is_plausible_ts(now, now));
        assert!(is_plausible_ts(now - 3_600, now));
        assert!(!is_plausible_ts(now - 86_400, now));
        // 这两个极端值正是修复前 `(ts - now).abs()` 会溢出的输入。
        assert!(!is_plausible_ts(i64::MIN, now));
        assert!(!is_plausible_ts(i64::MAX, now));
    }

    #[test]
    fn truncates_long_content_for_logs() {
        assert_eq!(truncate_for_log("短消息", 60), "短消息");
        let long = "字".repeat(100);
        let cut = truncate_for_log(&long, 60);
        assert_eq!(cut.chars().count(), 61, "60 个字 + 一个省略号");
        assert!(cut.ends_with('…'));
        // 多字节字符不能被截断成半个
        assert_eq!(truncate_for_log("中文内容测试", 3), "中文内…");
    }

    #[tokio::test]
    async fn dedup_prevents_double_handling() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();

        let mut router = Router::new();
        router.on_any(
            Matcher::Any,
            FnHandler::new("count", move |_ctx| {
                let c = counter.clone();
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Handled::Consumed
                })
            }),
        );

        let d = Dispatcher::new(router, services(), DispatchConfig::default());
        let e = event("GROUP_AT_MESSAGE_CREATE", r#"{"id":"M1","author":{"member_openid":"U"},"content":"x","group_openid":"G"}"#);

        d.handle(e.clone()).await;
        d.handle(e).await; // 同一个 msg_id，应被去重

        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn router_chain_stops_on_consumed() {
        let order = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let a = order.clone();
        let b = order.clone();

        let mut router = Router::new();
        router.on(Scope::Any, Matcher::Any, 10, FnHandler::new("first", move |_ctx| {
            let o = a.clone();
            Box::pin(async move {
                o.lock().unwrap().push("first");
                Handled::Consumed
            })
        }));
        router.on(Scope::Any, Matcher::Any, 0, FnHandler::new("second", move |_ctx| {
            let o = b.clone();
            Box::pin(async move {
                o.lock().unwrap().push("second");
                Handled::Next
            })
        }));

        let d = Dispatcher::new(router, services(), DispatchConfig::default());
        d.handle(event("C2C_MESSAGE_CREATE", r#"{"id":"M9","author":{"user_openid":"U"},"content":"hi"}"#)).await;

        assert_eq!(*order.lock().unwrap(), vec!["first"]);
    }

    #[tokio::test]
    async fn plugin_panic_is_isolated() {
        let mut router = Router::new();
        router.on_any(Matcher::Any, FnHandler::new("boom", |_ctx| {
            Box::pin(async {
                panic!("插件内部 panic");
            })
        }));

        let d = Dispatcher::new(router, services(), DispatchConfig::default());
        // 不应把测试进程带崩
        d.handle(event("C2C_MESSAGE_CREATE", r#"{"id":"M7","author":{"user_openid":"U"},"content":"x"}"#)).await;
    }

    /// 互动事件按注册顺序走 `on_interaction`，遇到 Consumed 立刻停下。
    #[tokio::test]
    async fn interaction_chain_stops_on_consumed() {
        use crate::plugin::InteractionCtx;
        use qqbot_api::Target;

        struct Probe {
            name: &'static str,
            seen: Arc<std::sync::Mutex<Vec<&'static str>>>,
            consume: bool,
        }

        #[async_trait::async_trait]
        impl crate::plugin::Handler for Probe {
            async fn handle(&self, _ctx: &Ctx) -> Handled {
                Handled::Next
            }

            async fn on_interaction(&self, ctx: &InteractionCtx) -> Handled {
                assert_eq!(ctx.button_data, "task:x:page:2");
                assert_eq!(ctx.event_id.as_deref(), Some("EV"));
                self.seen.lock().unwrap().push(self.name);
                if self.consume { Handled::Consumed } else { Handled::Next }
            }

            fn name(&self) -> &str {
                self.name
            }
        }

        let seen = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let mut router = Router::new();
        router.on_any(Matcher::Any, Probe { name: "first", seen: seen.clone(), consume: true });
        router.on_any(Matcher::Any, Probe { name: "second", seen: seen.clone(), consume: false });

        let ctx = InteractionCtx {
            services: services(),
            target: Target::group("G1"),
            sender_openid: Some("U1".into()),
            event_id: Some("EV".into()),
            button_data: "task:x:page:2".into(),
        };
        assert_eq!(router.dispatch_interaction(&ctx).await, Handled::Consumed);
        assert_eq!(*seen.lock().unwrap(), vec!["first"]);
    }

    #[tokio::test]
    async fn ready_sets_bot_name() {
        let d = Dispatcher::new(Router::new(), services(), DispatchConfig::default());
        d.handle(event(
            "READY",
            r#"{"version":1,"session_id":"s1","user":{"id":"1","username":"黑猫Bot","bot":true},"shard":[0,1]}"#,
        ))
        .await;
        assert_eq!(d.services().bot_name(), "黑猫Bot");
    }
}
