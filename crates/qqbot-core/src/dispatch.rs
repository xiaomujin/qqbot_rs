use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use moka::sync::Cache;
use qqbot_api::Event;
use qqbot_store::{now_unix, NewMessage, Scope};
use tokio::sync::Semaphore;

use crate::ctx::{Ctx, Services};
use crate::plugin::{Handled, Router};

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
    router: Router,
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
            router,
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
            other => {
                // 非消息事件（加群、被撤回、按钮回调等）在排障时同样需要可见
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
                created_at: event_unix(&message),
            });
        }

        let Some(ctx) = Ctx::new(self.services.clone(), message) else {
            return;
        };

        // 用 INFO：这是运维判断「事件到底有没有送达」的唯一依据。
        // 内容截断，避免把整篇消息灌进日志。
        tracing::info!(
            event = name,
            msg_id = %ctx.message_id(),
            target = %target.key(),
            content = %truncate_for_log(ctx.content(), 60),
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
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
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
