//! 会话状态 actor。
//!
//! 这是整套架构里最关键的一块：官方协议的三条硬约束
//! （`msg_seq` 递增、被动回复窗口、主动消息配额）都是**按键串行的状态机**，
//! 因此用「固定 N 个 shard actor + 按 key 哈希路由」来承载，做到零锁且天然有序。
//!
//! ⚠️ 注意：**不是**每个群一个 actor，而是固定 N 个 actor，每个内部持有
//! `HashMap<SessionKey, SessionState>`。

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use qqbot_api::{ApiClient, SendResult, Target};
use tokio::sync::{mpsc, oneshot};

use crate::error::CoreError;
use crate::message::SendRequest;

/// 被动回复窗口规则。官方文档：
/// - 单聊：60 分钟 / 4 次
/// - 群聊：5 分钟 / 5 次
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRules {
    pub ttl: Duration,
    pub max_replies: u8,
}

impl WindowRules {
    pub const C2C: Self = Self { ttl: Duration::from_secs(60 * 60), max_replies: 4 };
    pub const GROUP: Self = Self { ttl: Duration::from_secs(5 * 60), max_replies: 5 };

    pub fn for_target(target: &Target) -> Self {
        if target.is_group() {
            Self::GROUP
        } else {
            Self::C2C
        }
    }
}

/// 主动消息配额规则（单关系维度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaRules {
    pub per_minute: u32,
    pub per_day: u32,
}

impl QuotaRules {
    /// 官方：单关系维度 20 qpm，每日上限 1000 条/用户(群)。
    pub const DEFAULT: Self = Self { per_minute: 20, per_day: 1000 };
}

/// 主动消息配额账本。
#[derive(Debug)]
pub struct Quota {
    rules: QuotaRules,
    minute_used: u32,
    minute_reset: Instant,
    day_used: u32,
    day: u64,
}

impl Quota {
    /// `now` 由调用方显式注入。
    ///
    /// 若在这里用 `Instant::now()` 初始化窗口起点，会出现这样的错序：
    /// 调用方先取 `now`、再构造状态（窗口起点晚于 `now`），于是第一次不触发滚动，
    /// 第二次 `now >= minute_reset` 才触发——把计数清零，**每个窗口多放行一条消息**。
    pub fn new(rules: QuotaRules, now: Instant) -> Self {
        Self {
            rules,
            minute_used: 0,
            minute_reset: now + Duration::from_secs(60),
            day_used: 0,
            day: today(),
        }
    }

    /// 尝试消耗一次主动消息额度。
    pub fn try_consume(&mut self, now: Instant) -> bool {
        let day = today();
        if day != self.day {
            self.day = day;
            self.day_used = 0;
        }
        if now >= self.minute_reset {
            self.minute_used = 0;
            self.minute_reset = now + Duration::from_secs(60);
        }
        if self.minute_used >= self.rules.per_minute || self.day_used >= self.rules.per_day {
            return false;
        }
        self.minute_used += 1;
        self.day_used += 1;
        true
    }

    pub fn remaining_today(&self) -> u32 {
        self.rules.per_day.saturating_sub(self.day_used)
    }

    pub fn remaining_this_minute(&self) -> u32 {
        self.rules.per_minute.saturating_sub(self.minute_used)
    }
}

fn today() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

/// 被动回复窗口。
#[derive(Debug, Clone)]
pub struct PassiveWindow {
    pub msg_id: String,
    pub deadline: Instant,
    pub remaining: u8,
}

/// 单个会话的状态。
#[derive(Debug)]
pub struct SessionState {
    is_group: bool,
    rules: WindowRules,
    /// 被动回复序号，同一 `msg_id` 下必须递增，否则重复发送会被拒绝。
    msg_seq: u32,
    window: Option<PassiveWindow>,
    quota: Quota,
    last_active: Instant,
}

impl SessionState {
    pub fn new(is_group: bool, now: Instant) -> Self {
        let rules = if is_group { WindowRules::GROUP } else { WindowRules::C2C };
        Self {
            is_group,
            rules,
            msg_seq: 0,
            window: None,
            quota: Quota::new(QuotaRules::DEFAULT, now),
            last_active: now,
        }
    }

    pub fn rules(&self) -> WindowRules {
        self.rules
    }

    pub fn is_group(&self) -> bool {
        self.is_group
    }

    pub fn last_active(&self) -> Instant {
        self.last_active
    }

    pub fn msg_seq(&self) -> u32 {
        self.msg_seq
    }

    pub fn quota(&self) -> &Quota {
        &self.quota
    }

    /// 收到入站消息时登记被动回复窗口。
    ///
    /// 窗口从**收到消息**时开始计时，而不是从回复时开始，否则窗口会被错误放大。
    pub fn observe(&mut self, msg_id: &str, now: Instant) {
        self.last_active = now;
        self.window = Some(PassiveWindow {
            msg_id: msg_id.to_string(),
            deadline: now + self.rules.ttl,
            remaining: self.rules.max_replies,
        });
    }

    /// 尝试分配一个被动回复序号。
    ///
    /// 返回 `Some(msg_seq)` 表示可以被动回复；`None` 表示窗口不匹配/已过期/次数用尽。
    pub fn allocate_passive(&mut self, msg_id: &str, now: Instant) -> Option<u32> {
        let window = self.window.as_mut()?;
        if window.msg_id != msg_id || now >= window.deadline || window.remaining == 0 {
            return None;
        }
        window.remaining -= 1;
        self.msg_seq = self.msg_seq.wrapping_add(1);
        self.last_active = now;
        Some(self.msg_seq)
    }

    /// 尝试消耗一次主动消息额度。
    pub fn try_active(&mut self, now: Instant) -> bool {
        self.last_active = now;
        self.quota.try_consume(now)
    }

    /// 当前被动窗口剩余次数（0 表示不可被动回复）。
    pub fn passive_remaining(&self, now: Instant) -> u8 {
        match &self.window {
            Some(w) if now < w.deadline => w.remaining,
            _ => 0,
        }
    }

    /// 丢弃过期的被动窗口。
    pub fn expire_window(&mut self, now: Instant) {
        if let Some(w) = &self.window {
            if now >= w.deadline {
                self.window = None;
            }
        }
    }
}

/// 发往某个会话 shard 的消息。
pub enum SessionMsg {
    /// 登记被动回复窗口（收到入站消息时调用，fire-and-forget）。
    Observe { key: String, msg_id: String, is_group: bool },
    /// 发送消息。
    Send {
        key: String,
        request: SendRequest,
        ack: oneshot::Sender<Result<SendResult, CoreError>>,
    },
    /// 淘汰会话状态。
    Evict { key: String },
}

/// 会话 actor 注册表：按 key 哈希路由到固定数量的 shard。
pub struct SessionRegistry {
    shards: Vec<mpsc::Sender<SessionMsg>>,
    hasher: RandomState,
    timeout: Duration,
}

impl SessionRegistry {
    /// `shards` 建议取 CPU 核数 × 2；每个 shard 内部持有全部会话状态。
    pub fn new(api: ApiClient, shards: usize, queue_capacity: usize, timeout: Duration) -> Self {
        let shards = shards.max(1);
        // mpsc::channel(0) 会直接 assert panic；这是 pub 构造器，必须自己兜住。
        let queue_capacity = queue_capacity.max(1);
        let mut senders = Vec::with_capacity(shards);
        for index in 0..shards {
            let (tx, rx) = mpsc::channel(queue_capacity);
            let actor = SessionShard {
                index,
                inbox: rx,
                states: HashMap::new(),
                api: api.clone(),
            };
            tokio::spawn(actor.run());
            senders.push(tx);
        }
        tracing::info!(shards, "会话 actor 已启动");
        Self { shards: senders, hasher: RandomState::new(), timeout }
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    fn route(&self, key: &str) -> &mpsc::Sender<SessionMsg> {
        let idx = (self.hasher.hash_one(key) as usize) % self.shards.len();
        &self.shards[idx]
    }

    /// 登记被动回复窗口（失败仅记录日志，不影响主流程）。
    pub fn observe(&self, target: &Target, msg_id: &str) {
        let key = target.key();
        // 先把借用用完再移动 key：`Target::key()` 内部是 format!，
        // 重复调用等于每条入站消息白付一次分配 + 一次哈希。
        let shard = self.route(&key);
        let msg = SessionMsg::Observe {
            key,
            msg_id: msg_id.to_string(),
            is_group: target.is_group(),
        };
        // 用 key 路由到同一个 shard，保证与后续 Send 的顺序一致。
        if let Err(err) = shard.try_send(msg) {
            tracing::warn!(error = %err, "登记被动窗口失败");
        }
    }

    /// 发送消息（经由会话 actor 分配 `msg_seq` 并校验配额）。
    pub async fn send(&self, request: SendRequest) -> Result<SendResult, CoreError> {
        let key = request.target.key();
        let (ack_tx, ack_rx) = oneshot::channel();
        // ⚠️ 入队也必须受超时保护：shard 队列有界，而 actor 在 handle_send 里
        // 串行 await 一次 HTTP 调用。队列满时裸 `send().await` 会无限期挂起，
        // 而这段等待还占着 dispatch 的并发许可。
        match self
            .route(&key)
            .send_timeout(SessionMsg::Send { key, request, ack: ack_tx }, self.timeout)
            .await
        {
            Ok(()) => {}
            Err(mpsc::error::SendTimeoutError::Closed(_)) => return Err(CoreError::Closed),
            Err(mpsc::error::SendTimeoutError::Timeout(_)) => {
                tracing::warn!("会话 shard 队列已满，入队超时");
                return Err(CoreError::Canceled);
            }
        }

        match tokio::time::timeout(self.timeout, ack_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(CoreError::Canceled),
            Err(_) => Err(CoreError::Canceled),
        }
    }

}

struct SessionShard {
    index: usize,
    inbox: mpsc::Receiver<SessionMsg>,
    states: HashMap<String, SessionState>,
    api: ApiClient,
}

/// 空闲多久后回收会话状态。
///
/// 被动回复窗口最长 60 分钟（单聊），因此 2 小时足以保证不会误删仍在用的窗口。
/// 代价是：被回收的会话其「当日主动消息计数」会归零——服务端仍会做最终限流，
/// 且空闲 2 小时后又恰好发满 1000 条的场景可以忽略。
const SESSION_IDLE_TTL: Duration = Duration::from_secs(2 * 60 * 60);
/// 回收扫描周期。
const SESSION_SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);

impl SessionShard {
    async fn run(self) {
        // 解构 self：这样 select 的两条分支可以分别借用 inbox 与 states，
        // 不会互相冲突。
        let SessionShard { index, mut inbox, mut states, api } = self;

        let mut sweep = tokio::time::interval(SESSION_SWEEP_INTERVAL);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // interval 的首次 tick 立即就绪，先消费掉。
        sweep.tick().await;

        loop {
            tokio::select! {
                _ = sweep.tick() => {
                    evict_idle(&mut states, Instant::now(), index);
                }
                msg = inbox.recv() => {
                    let Some(msg) = msg else { break };
                    match msg {
                        SessionMsg::Observe { key, msg_id, is_group } => {
                            let now = Instant::now();
                            states
                                .entry(key)
                                .or_insert_with(|| SessionState::new(is_group, now))
                                .observe(&msg_id, now);
                        }
                        SessionMsg::Evict { key } => {
                            states.remove(&key);
                        }
                        SessionMsg::Send { key, request, ack } => {
                            let result = handle_send(&api, &mut states, &key, request).await;
                            let _ = ack.send(result);
                        }
                    }
                }
            }
        }
        tracing::debug!(shard = index, "会话 shard 退出");
    }
}

/// 回收长期空闲的会话状态，避免 `HashMap` 只增不减。
fn evict_idle(states: &mut HashMap<String, SessionState>, now: Instant, shard: usize) {
    let before = states.len();
    states.retain(|_, s| now.saturating_duration_since(s.last_active()) < SESSION_IDLE_TTL);
    let removed = before - states.len();
    if removed > 0 {
        metrics::counter!("qqbot_session_evicted_total").increment(removed as u64);
        tracing::debug!(shard, removed, remaining = states.len(), "回收空闲会话状态");
    }
}

async fn handle_send(
    api: &ApiClient,
    states: &mut HashMap<String, SessionState>,
    key: &str,
    request: SendRequest,
) -> Result<SendResult, CoreError> {
    let now = Instant::now();
    let is_group = request.target.is_group();

    // 1) 状态决策放在独立作用域内完成：&mut SessionState 绝不跨 await 持有。
    let (msg_id, msg_seq) = {
        let state = states
            .entry(key.to_string())
            .or_insert_with(|| SessionState::new(is_group, now));
        state.expire_window(now);

        let passive = request
            .reply_to
            .as_deref()
            .and_then(|id| state.allocate_passive(id, now).map(|seq| (id.to_string(), seq)));

        match passive {
            Some((id, seq)) => (Some(id), Some(seq)),
            None => {
                // 被动不可用（无凭证 / 窗口过期 / 次数用尽）→ 走主动消息，消耗配额
                if !state.try_active(now) {
                    metrics::counter!("qqbot_send_rejected_total", "reason" => "quota").increment(1);
                    return Err(CoreError::QuotaExceeded);
                }
                (None, None)
            }
        }
    };

    // 先算 mode，`msg_id` 就能直接 move 进 message，省一次堆分配。
    let mode = if msg_id.is_some() { "passive" } else { "active" };
    let mut message = request.body.to_out_message();
    message.msg_id = msg_id;
    message.msg_seq = msg_seq;
    message.event_id = request.event_id.clone();
    metrics::counter!("qqbot_send_total", "mode" => mode, "kind" => request.body.kind()).increment(1);

    let result = api.send_message(&request.target, &message).await;

    // 2) 服务端判定被动窗口已过期（err_code 40034005）：清掉本地窗口后改用主动消息重试一次。
    //    本地时钟与服务端存在偏差时，这一步能避免直接丢消息。
    if let Err(err) = &result {
        if err.is_passive_expired() && message.msg_id.is_some() {
            tracing::warn!(key, "被动回复窗口已被服务端判定过期，改用主动消息重试");

            let retry_now = Instant::now();
            let allowed = {
                let state = states
                    .entry(key.to_string())
                    .or_insert_with(|| SessionState::new(is_group, retry_now));
                state.expire_window(retry_now);
                state.try_active(retry_now)
            };
            if !allowed {
                metrics::counter!("qqbot_send_rejected_total", "reason" => "quota_after_expired").increment(1);
                return Err(CoreError::QuotaExceeded);
            }

            let mut retry = request.body.to_out_message();
            retry.event_id = request.event_id.clone();
            metrics::counter!("qqbot_send_total", "mode" => "active_retry", "kind" => request.body.kind())
                .increment(1);

            let retry_result = api.send_message(&request.target, &retry).await;
            if let Err(e) = &retry_result {
                tracing::warn!(key, error = %e, "主动消息重试仍失败");
            }
            return retry_result.map_err(CoreError::Api);
        }
    }

    match &result {
        Ok(_) => {
            // INFO 级：线上日志可直接看到每一次真实出站消息。
            tracing::info!(key, mode, msg_seq = ?msg_seq, "消息已发送");
        }
        Err(err) => {
            tracing::warn!(
                key,
                mode,
                error = %err,
                hint = err.hint().unwrap_or("-"),
                "消息发送失败"
            );
        }
    }

    result.map_err(CoreError::Api)
}

/// 供外部（如 Bot 运行时）复用的 shard 数量建议。
pub fn recommended_shards() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) * 2
}

/// 共享的注册表句柄。
pub type SharedRegistry = Arc<SessionRegistry>;

#[cfg(test)]
mod tests {
    use super::*;

    fn state_group() -> SessionState {
        SessionState::new(true, Instant::now())
    }

    #[test]
    fn group_window_allows_five_replies_then_blocks() {
        let mut s = state_group();
        let now = Instant::now();
        s.observe("M1", now);
        for expected in 1..=5u32 {
            assert_eq!(s.allocate_passive("M1", now), Some(expected));
        }
        assert_eq!(s.allocate_passive("M1", now), None, "第 6 次应被拒绝");
    }

    #[test]
    fn c2c_window_allows_four_replies() {
        let now = Instant::now();
        let mut s = SessionState::new(false, now);
        s.observe("M1", now);
        for expected in 1..=4u32 {
            assert_eq!(s.allocate_passive("M1", now), Some(expected));
        }
        assert_eq!(s.allocate_passive("M1", now), None);
    }

    #[test]
    fn group_window_expires_after_five_minutes() {
        let mut s = state_group();
        let now = Instant::now();
        s.observe("M1", now);
        let later = now + Duration::from_secs(5 * 60 + 1);
        assert_eq!(s.allocate_passive("M1", later), None, "群聊窗口 5 分钟");
    }

    #[test]
    fn c2c_window_still_valid_after_five_minutes() {
        let now = Instant::now();
        let mut s = SessionState::new(false, now);
        s.observe("M1", now);
        let later = now + Duration::from_secs(5 * 60 + 1);
        assert!(s.allocate_passive("M1", later).is_some(), "单聊窗口 60 分钟");
    }

    #[test]
    fn different_msg_id_resets_window() {
        let mut s = state_group();
        let now = Instant::now();
        s.observe("M1", now);
        s.allocate_passive("M1", now);
        s.observe("M2", now);
        assert_eq!(s.passive_remaining(now), 5);
        assert!(s.allocate_passive("M1", now).is_none(), "旧 msg_id 不再可用");
        assert!(s.allocate_passive("M2", now).is_some());
    }

    #[test]
    fn msg_seq_is_monotonic() {
        let mut s = state_group();
        let now = Instant::now();
        s.observe("M1", now);
        let a = s.allocate_passive("M1", now).unwrap();
        let b = s.allocate_passive("M1", now).unwrap();
        assert!(b > a);
    }

    /// 复现真实调用顺序：先取 now，再构造状态。窗口起点必须由 now 决定。
    #[test]
    fn quota_window_is_not_reset_by_a_stale_timestamp() {
        let now = Instant::now();
        let mut q = Quota::new(QuotaRules { per_minute: 3, per_day: 1000 }, now);
        assert!(q.try_consume(now), "第 1 次");
        assert!(q.try_consume(now + Duration::from_millis(1)), "第 2 次");
        assert!(q.try_consume(now + Duration::from_millis(2)), "第 3 次");
        assert!(
            !q.try_consume(now + Duration::from_millis(3)),
            "第 4 次必须被拒绝——窗口不应被陈旧时间戳错误重置"
        );
    }

    #[test]
    fn active_quota_limits_per_minute() {
        let now = Instant::now();
        let mut q = Quota::new(QuotaRules { per_minute: 3, per_day: 1000 }, now);
        assert!(q.try_consume(now));
        assert!(q.try_consume(now));
        assert!(q.try_consume(now));
        assert!(!q.try_consume(now), "超过 3/min 应被拒绝");
        // 一分钟后恢复
        assert!(q.try_consume(now + Duration::from_secs(61)));
    }

    #[test]
    fn active_quota_limits_per_day() {
        let now = Instant::now();
        let mut q = Quota::new(QuotaRules { per_minute: 1000, per_day: 2 }, now);
        assert!(q.try_consume(now));
        assert!(q.try_consume(now + Duration::from_secs(61)));
        assert!(!q.try_consume(now + Duration::from_secs(122)));
        assert_eq!(q.remaining_today(), 0);
    }

    #[test]
    fn window_rules_match_official_docs() {
        assert_eq!(WindowRules::C2C.ttl, Duration::from_secs(3600));
        assert_eq!(WindowRules::C2C.max_replies, 4);
        assert_eq!(WindowRules::GROUP.ttl, Duration::from_secs(300));
        assert_eq!(WindowRules::GROUP.max_replies, 5);
    }

    #[test]
    fn idle_sessions_are_evicted() {
        let mut states: HashMap<String, SessionState> = HashMap::new();
        let now = Instant::now();
        states.insert("group:A".into(), SessionState::new(true, now));
        states.insert("group:B".into(), SessionState::new(true, now));

        // A 最后活跃于 now；B 在 TTL 之后仍然活跃
        states.get_mut("group:A").unwrap().observe("M", now);
        let later = now + SESSION_IDLE_TTL + Duration::from_secs(1);
        states.get_mut("group:B").unwrap().observe("M", later);

        evict_idle(&mut states, later, 0);

        assert!(states.contains_key("group:B"), "活跃会话不应被回收");
        assert!(!states.contains_key("group:A"), "空闲超过 TTL 的会话应被回收");
    }

    #[test]
    fn idle_ttl_exceeds_longest_passive_window() {
        // 单聊被动窗口是 60 分钟；TTL 必须明显大于它，否则会误删仍在使用的窗口。
        assert!(SESSION_IDLE_TTL >= WindowRules::C2C.ttl * 2);
    }

    /// 回归：这个测试以前是**恒真**的 —— 断言两边是同一个表达式，永远不会失败，
    /// 而且它根本没调用 `route()`，却顶着「同一 key 路由稳定」的名字。
    /// 虚假信心比没有测试更糟。
    #[tokio::test]
    async fn route_is_stable_for_same_key() {
        let api =
            ApiClient::new(qqbot_api::ApiClientConfig::new("test-app", "test-secret")).unwrap();
        let reg = SessionRegistry::new(api, 8, 16, Duration::from_secs(5));

        let key = "group:G1";
        assert!(
            std::ptr::eq(reg.route(key), reg.route(key)),
            "同一个 key 必须路由到同一个 shard"
        );

        // 顺带确认哈希没退化成常数：64 个 key 应铺开到多个 shard。
        let distinct = (0..64)
            .map(|i| reg.route(&format!("group:G{i}")) as *const _)
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert!(distinct > 1, "路由应把 key 分散到多个 shard，实际只用到 {distinct} 个");
    }
}
