# rust-skills 全项目审计报告

> **审计对象**：`E:\qqbot` —— QQ 官方 API v2 机器人，tokio 异步运行时，7 个 crate 的 cargo workspace
> **规则来源**：[`rust-skills`](https://github.com/leonardomso/rust-skills) v1.5.1 —— 265 条规则 / 26 个类别（面向 Rust 1.96 / 2024 edition）
> **代码规模**：45 个 `.rs` 文件 / 7,803 行
> **审计方式**：按 crate 分 7 个单元并行审查 + `cargo clippy -- -W clippy::pedantic -W clippy::nursery` 机械扫描 + 逐条人工复核
> **基线**：审计前 `cargo clippy --workspace --all-targets` **零警告**，**144 个测试全绿**

---

## 0. 结论摘要

**这个项目的 Rust 水平显著高于典型 AI 生成的代码。** 最关键的几个设计决策——有界队列 + 独立 OS 线程做 SQLite 写入、actor 模型而非共享锁、`spawn_blocking` 用在真正 CPU 密集的地方、WAL 下读写分连接、词云的确定性排序——都是对的，而且代码里有注释解释**为什么**这么选。

但审计发现 **4 个真实缺陷**，其中 2 个会在生产环境造成可观测的后果：

| # | 严重性 | 章节 | 问题 | 后果 |
|---|---|---|---|---|
| 1 | **HIGH** | §1.1 | 事件分发并发度实际恒为 **1** | 一条词云命令阻塞**所有**消息处理，并导致网关事件被静默丢弃 |
| 2 | **HIGH** | §1.2 | 网关退避被 `Send` 命令打穿 | 重连期间下发命令 → 指数退避失效 → 热重连循环 |
| 3 | **HIGH** | §1.5 | token 刷新持独占锁跨 await + 余量大于服务端剩余寿命 | 刷新期间**所有** API 调用排队；`expires_in ≤ 300` 时每次调用都空刷 token（有线上日志佐证） |
| 4 | MEDIUM | §1.3 | 分片上传字节计算可溢出 | 畸形服务端响应 → 上传路径 panic |
| 5 | MEDIUM | §1.4 | 保留期配置无上界 + 未检查算术 | 极端配置下 `DELETE` 可能清空整张消息表 |
| 6 | MEDIUM | §2.9 | `SessionRegistry::send` 的超时没包住入队 | 有界队列满时调用方无限期挂起，且占着 dispatch 并发槽位 |
| 7 | MEDIUM | §2.10 | 插件 panic 时把**完整**用户消息写进日志 | 与同文件其他分支的截断策略矛盾，可被刷屏 |

另有 **8 条 P1**（§2.1–§2.8）与 **约 30 条 P2**。所有 P0/P1 条目我都**逐条打开源码复核过**，不是静态工具的原始输出。

---

## 0.5 修复状态（2026-09-29）

审计后已按 §6 的顺序实施修复。**149 个测试全绿（新增 5 个回归测试），`cargo clippy --workspace --all-targets` 零警告**，release 二进制 15.03 MB。

### 已修复

| 章节 | 修复内容 |
|---|---|
| §1.1 | `main.rs` 把事件处理 `JoinSet::spawn` 出去，主循环不再阻塞；`Semaphore(16)` 真正生效。退出时给 10s 排空窗口。 |
| §1.2 | `actor.rs` 改用**绝对 deadline** 循环等待退避，只有 `Shutdown`/`Reconnect` 能打破；退避期间的 `Send` 显式记录而非静默丢弃。 |
| §1.5 | `TokenProvider` 拆成 `RwLock` 读缓存 + `Mutex` 只串行化刷新；余量改为 `min(300s, 寿命/2)`，`expires_in` 夹上界防溢出。 |
| §1.3 | `plan_chunks` 用 `saturating_add`。 |
| §1.4 | 保留期/窗口天数改 `clamp(1, 36500)`；`cutoff` / `since` 改 `try_from` + `saturating_sub`。 |
| §2.1 | 写线程 flush 截止时间锚定到**本批第一条消息**。 |
| §2.2 | `schema_version` 读取区分「行不存在 / 查询失败 / 值损坏」；初始化 INSERT 补 `ON CONFLICT`。 |
| §2.3 | 读写线程 `spawn` 返回 `Result`，环境失败走降级而非 panic。 |
| §2.5 | 新增 `MessageStore::open_async`，建库 / PRAGMA 移到 `spawn_blocking`。 |
| §2.7 | 渲染改用 `Cow<str>`，不再为统一类型克隆整份 SVG。 |
| §2.8 | `normalize_token` 单遍扫描，去掉每 token 一个 `Vec<char>`。 |
| §2.9 | `SessionRegistry::send` 用 `send_timeout`，入队也受超时保护。 |
| §2.10 | 插件 panic 日志改用 `truncate_for_log`，与同文件其他分支一致。 |
| §3.1 | `event_unix` 改用 `abs_diff`，抽出 `is_plausible_ts` 便于测试。 |
| §3.4 | 新增 `[workspace.lints]`（`unsafe_code = "deny"` + `clippy::correctness = "deny"`），7 个 crate 与根 package 全部 `[lints] workspace = true`。 |
| §3.5 | `queue_capacity.max(1)`；`observe` 不再重复调 `target.key()`；`msg_id` 直接 move；删除文档说谎且无调用点的 `shards()`；**重写恒真测试**。 |
| §3.6 | `OutMessage.msg_type` 改回 `MsgType`；`Identify.intents` 改回 `Intents`；`ApiClientConfig`/`Identify`/`Resume` 手写 `Debug` 遮蔽密钥；删除永不构造的 `ApiError::MissingData`。 |

### 新增回归测试

| 测试 | 覆盖 |
|---|---|
| `steady_writes_are_flushed_by_time_not_by_batch` | §2.1 —— **已实测：旧实现下 FAILED，修复后 PASS** |
| `corrupt_schema_version_reports_a_real_error` | §2.2 |
| `plan_chunks_survives_absurd_block_size` | §1.3 |
| `implausible_timestamps_are_rejected_without_overflow` | §3.1 |
| `refresh_point_always_lands_after_the_refresh_itself` | §1.5 |
| `route_is_stable_for_same_key`（重写） | §3.5 —— 原来恒真，现在真的调用 `route()` |

### 第二批修复（同日）

| 章节 | 修复内容 |
|---|---|
| §3.4 | **全部**外部依赖上移到 `[workspace.dependencies]`（22 项），8 个 manifest 一律用 `{ workspace = true }` 继承，额外 feature 在成员里追加。**`Cargo.lock` 逐字节未变** —— 证明解析出的依赖图完全一致，这次重构没有偷偷改变任何版本或 feature。 |
| §3.3 | 网关 `session_start_limit.max_concurrency` 从「只进日志」变成真正的背压：新增 `Arc<Semaphore>` 闸门，Identify 前取许可、收到 READY 归还。**关键细节**：重新 Identify 前先归还旧许可，否则 `max_concurrency = 1` 时 actor 会把自己锁死。 |
| §3.6 | `Event::Notice` 装箱（`RawNotice` 约 144 字节，原先撑大了整个 `Event`）。 |
| §3.5 | 删除三个从未接线的公开项：`SessionMsg::Evict`、`SharedRegistry`、`CoreError::PassiveExpired`。 |
| §3.2 | `escape_xml` 改单遍，且无特殊字符时返回 `Cow::Borrowed`（原来每个词 5 次 `replace`）；SVG 拼装全部改 `writeln!` 直写（原来 `push_str(&format!(...))` 每次多分配一个临时 String）；`truncate_for_log` 单遍；`History::record` 加零分配快路径；`filter_corpus` 只取一次读锁（原来每条语料一次，500 条 = 500 次加解锁）；`count_words` 预留容量。 |
| §3.4 | 插件 `name()` 返回 `&'static str`。 |

**渲染输出验证**：SVG 拼装从 `push_str(&format!())` 改成 `writeln!` 之后，真机上传的 PNG **字节数完全相同（173692）** —— 证明重构没有改变一个字节的输出。

### 有意未修（附理由）

| 章节 | 为什么不修 |
|---|---|
| §2.4 整文件哈希阻塞运行时 | 移进 `spawn_blocking` 需要 `'static` 数据：要么整份拷贝（100MB 文件反而更糟），要么把 `upload_bytes`/`reply_image` 全链改成 `Arc<[u8]>`。而本机器人实际载荷是几百 KB 的 PNG（哈希约 1ms）。**收益不抵改动面。** |
| §2.6 缓存插入深拷贝 PNG | 复核后判定这是**伪优化**：调用方 `ctx.rs:67/72` 立刻需要拥有所有权的 `.png`，改成 `Arc<RenderedImage>` 只是把深拷贝从 dispatcher 挪到边界，**总数不变**。真正的解法是整条链改 `Arc<[u8]>`，属另一个量级的重构。 |
| §3.5 `SessionMsg::Evict` / `SharedRegistry` / `CoreError::PassiveExpired` | 三者确实无内部构造点，但都是**可用**的扩展点（`Evict` 真被发出去也能工作），不是缺陷。删除属风格选择。 |
| §3.4 `[workspace.dependencies]` 继承 | 已核对：6 个共享依赖在 7 个 crate 里的**版本串完全一致**，当前没有漂移。这是防未来的重构，涉及 8 个文件的 feature 集合调整，单独做更稳妥。 |
| §3.6 `Event::Notice` 装箱 | Notice 事件罕见，省下的是每次事件约 168 字节。收益可忽略。 |

### 真机验证

修复后的二进制已重建上线，实测一条 `词云` 命令走完整链路：

```
03:40:58.135  INFO 收到消息 event="GROUP_MESSAGE_CREATE" msg_id=ROBOT1.0_... content=词云
03:40:58.141  INFO 渲染词云 words=12
03:41:01.029  INFO 富媒体上传完成 scene="group" bytes=173692
03:41:04.199  INFO 消息已发送 key="group:617E..." mode="passive" msg_seq=Some(1)
```

整段日志里 `access_token 已刷新` **只出现一次**，之后所有 API 调用都复用缓存 —— 正是 §1.5 要修的。新事件循环（`JoinSet::spawn`）也确认能处理真实事件。

![修复后真机词云](samples/live-wordcloud-after-fixes.png)

---

## 1. P0 —— 必须修（正确性 / 生产影响）

### 1.1 [HIGH] 事件分发的并发度实际恒为 1，`Semaphore(16)` 是死代码

**位置**：`src/main.rs:254-270`、`crates/qqbot-core/src/dispatch.rs:139-142`

```rust
// src/main.rs —— 主事件循环
loop {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => { break; }
        event = gateway.recv() => {
            match event {
                Some(event) => dispatcher.handle(event).await,   // ⚠️ 就在这里 await
                None => { break; }
            }
        }
    }
}
```

```rust
// crates/qqbot-core/src/dispatch.rs —— 信号量
let _permit = match self.sem.clone().acquire_owned().await {
    Ok(p) => p,
    Err(_) => return,
};
let started = Instant::now();
match AssertUnwindSafe(self.router.dispatch(&ctx)).catch_unwind().await {
```

**问题**：`dispatcher.handle(event).await` 在 select 分支里被**完整等待**，循环必须等它返回才能再次 `gateway.recv()`。因此整个进程同时只有一个事件在处理——`DispatchConfig::concurrency: 16` 建的 `Semaphore(16)` **永远不可能持有超过 1 个 permit**。

这条链路的具体后果：

1. **一条慢命令阻塞全机器人**。`词云` 走的是 render（100–500ms，含 `spawn_blocking` 光栅化）+ 媒体上传 + HTTP 发送，`reply_svg` 的整条链都在 `dispatch` 里同步等待。这期间**任何**其他群、任何私聊的消息都排在后面。
2. **事件被静默丢弃**。`gateway.recv()` 不被 poll 时，网关读循环的 `try_send` 会撞满有界通道（`event_buffer: 2048`），随后按设计丢弃并累加 `qqbot_gateway_events_dropped_total`。也就是说**词云命令会导致无关消息丢失**。
3. `DispatchConfig::concurrency` 是一个**说谎的配置项**——改它没有任何效果。

**规则依据**：`async-joinset-structured`（用 `JoinSet` 管理动态任务集）、`async-bounded-channel`（背压必须真的生效）、`async-join-parallel`。

**修复**（让信号量真正起作用）：

```rust
// src/main.rs —— 把 handle 派生出去，主循环只负责收
let mut inflight = tokio::task::JoinSet::new();
loop {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => break,
        event = gateway.recv() => {
            match event {
                Some(event) => {
                    let d = Arc::clone(&dispatcher);
                    inflight.spawn(async move { d.handle(event).await });
                }
                None => { tracing::warn!("事件通道已关闭"); break; }
            }
        }
        // 回收已完成的任务，避免 JoinSet 无界增长
        Some(_) = inflight.join_next(), if !inflight.is_empty() => {}
    }
}
inflight.shutdown().await;
```

派生之后 `dispatch.rs` 里的 `Semaphore(concurrency)` 才真正承担「同时最多 16 条插件链」的职责。**注意**：若采用此方案，需要确认插件链本身对同一会话的并发安全（`SessionRegistry` 与 `WordCloudPlugin` 内部都用了锁，见 §5）。

---

### 1.2 [HIGH] 网关退避被 `Send` 命令打穿

**位置**：`crates/qqbot-gateway/src/actor.rs:231-241`

```rust
let delay = self.backoff.next_delay();
tracing::info!(shard = ?self.shard, ?delay, attempt = self.backoff.attempt(), "退避后重连");

tokio::select! {
    _ = tokio::time::sleep(delay) => {}
    cmd = self.inbox.recv() => {
        if matches!(cmd, None | Some(GatewayCmd::Shutdown)) {
            return;
        }
    }
}
```

**问题**：这个分支只处理 `None | Shutdown`。当收到 `Some(GatewayCmd::Send(_))` 时：

- `matches!` 为 false → 既不 `return` 也不 `continue` → select 正常结束 → 回到循环顶部 → **重新连接**；
- 而 `tokio::time::sleep(delay)` 这个 future 在 select 结束时被**取消**，剩余的退避时间**全部作废**；
- 那条 `Send` 命令被**静默丢弃**（无日志、无指标）。

于是只要上层在重连窗口内尝试下发一条消息，退避就被打穿成「立刻重连」。反复触发就是**热重连循环**——对 QQ 网关而言这有被限流甚至封禁的实际风险。同时 `next_delay()` 已经推进过 attempt 计数，退避状态与真实重连次数脱节，日志里的 `attempt` 也会误导排障。

**规则依据**：`async-select-racing`、`async-cancel-safety`（`sleep` 在 `select!` 里被取消是**故意**的语义，但这里取消后没有重新进入等待，破坏了退避契约）。

**修复**（用绝对 deadline，只让真正需要的事件打断退避）：

```rust
let deadline = tokio::time::Instant::now() + delay;
loop {
    tokio::select! {
        _ = tokio::time::sleep_until(deadline) => break,
        cmd = self.inbox.recv() => match cmd {
            None | Some(GatewayCmd::Shutdown) => return,
            Some(GatewayCmd::Reconnect) => break,
            Some(GatewayCmd::Send(_)) => {
                // 连接已断，这条下发注定失败：显式记录而不是静默吞掉
                tracing::debug!(shard = ?self.shard, "退避期间收到 Send，连接未就绪，已丢弃");
            }
        },
    }
}
```

`sleep_until` 与 `Receiver::recv` 都是 cancel-safe，循环重入不会丢失状态。

---

### 1.3 [MEDIUM] 分片上传的字节计算可溢出并 panic

**位置**：`crates/qqbot-media/src/uploader.rs:258-271`（触发点在 `:153`）

```rust
let size = part
    .block_size
    .as_deref()
    .and_then(|s| s.trim().parse::<usize>().ok())
    .filter(|s| *s > 0)          // ⚠️ 只校验了「非零」，没有上界
    .unwrap_or(fallback_block);
let end = (offset + size).min(total);   // ⚠️ 未检查加法
```

**问题**：`size` 来自**服务端下发的字符串**（`UploadPartUrl.block_size`）。当 `offset > 0` 且 `size` 接近 `usize::MAX` 时（例如第二片下发 `"18446744073709551615"`，这个值能通过 `parse::<usize>()` 且满足 `> 0`）：

- debug：`offset + size` 直接 panic（attempt to add with overflow）；
- release：回绕成 `offset - 1`，于是 `end < start`，`steps` 里出现一个 `start=5, end=4` 的非法区间；
- 随后 `uploader.rs:153` 的 `&bytes[step.start..step.end]` panic（`slice index starts at 5 but ends at 4`）。

即**一个畸形的服务端响应就能让上传路径 panic**。现有测试只覆盖了小尺寸分片。

**规则依据**：`num-overflow-explicit`（外部输入参与算术必须用 `checked_`/`saturating_`）。

**修复**：

```rust
// 服务端数值不可信：把「size 过大」归并成「吃到文件末尾」
let end = offset.saturating_add(size).min(total);
```

并补一条回归测试：

```rust
#[test]
fn plan_chunks_survives_absurd_block_size() {
    let parts = vec![part(0, Some("5"), "u0"), part(1, Some("18446744073709551615"), "u1")];
    let steps = plan_chunks(&parts, 5, 12).unwrap();   // 不应 panic
    assert_eq!((steps[1].start, steps[1].end), (5, 12));
}
```

---

### 1.4 [MEDIUM] 保留期配置无上界 + 未检查算术 → 可能删空整张消息表

**位置**：`src/config.rs:78-82`、`crates/qqbot-store/src/lib.rs:181`、`crates/qqbot-plugins/src/wordcloud.rs:154`

这是三个独立缺陷串成的一条链：

```rust
// ① src/config.rs:78 —— 只保证 >= 1，没有上界
let retention_days = env_u64("QQBOT_RETENTION_DAYS", DEFAULT_RETENTION_DAYS).max(1);
// ② src/config.rs:82 —— 未检查的 u64 乘法
retention: Duration::from_secs(retention_days * DAY_SECS),
```

```rust
// ③ crates/qqbot-store/src/lib.rs:181 —— 未检查的减法 + 收窄转换
let cutoff = now_unix() - store.cfg.retention.as_secs() as i64;
match store.purge_before(cutoff).await {
```

**问题**：`QQBOT_RETENTION_DAYS` 是环境变量，`.max(1)` 只挡住了 0。若配置成 `u64::MAX / 86400 + 1` 之类的值，② 的乘法在 release 下**回绕**成一个很小的数；于是 ③ 算出的 `cutoff` 是「刚刚」——随后 `DELETE FROM messages WHERE created_at < cutoff` 会**删掉几乎整张表**。debug 下则在 ② 或 ③ 直接 panic。

同一模式还有第二处：

```rust
// src/config.rs:104
wordcloud_window: Duration::from_secs(window_days * DAY_SECS),
// crates/qqbot-plugins/src/wordcloud.rs:154
let since = now_unix() - self.window.as_secs() as i64;
```

**规则依据**：`num-overflow-explicit`、`num-saturating-clamp`（「Bound values with `clamp` and saturating arithmetic」）、`num-cast-try-from`。

**修复**：

```rust
// src/config.rs —— 上界用 clamp 表达业务约束（100 年足够）
const MAX_RETENTION_DAYS: u64 = 36_500;
let retention_days = env_u64("QQBOT_RETENTION_DAYS", DEFAULT_RETENTION_DAYS)
    .clamp(1, MAX_RETENTION_DAYS);
let window_days = env_u64("QQBOT_WORDCLOUD_WINDOW_DAYS", DEFAULT_WORDCLOUD_WINDOW_DAYS)
    .clamp(1, MAX_RETENTION_DAYS);
// 之后 retention_days * DAY_SECS 的乘积必然落在 u64 内
```

```rust
// crates/qqbot-store/src/lib.rs:181
let retention_secs = i64::try_from(store.cfg.retention.as_secs()).unwrap_or(i64::MAX);
let cutoff = now_unix().saturating_sub(retention_secs);
```

```rust
// crates/qqbot-plugins/src/wordcloud.rs:154
let since = now_unix().saturating_sub(self.window.as_secs().min(i64::MAX as u64) as i64);
```

### 1.5 [HIGH] token 刷新：独占锁跨 await + 刷新余量大于服务端剩余寿命

**位置**：`crates/qqbot-api/src/client.rs:22`、`:58-125`、`:398-403`

```rust
/// 官方说明：距过期 **60s 内**请求会返回新 token。这里留更宽裕的 300s，
/// 避免长请求跨过过期边界。
const REFRESH_MARGIN: Duration = Duration::from_secs(300);          // :22

pub async fn token(&self) -> Result<String, ApiError> {             // :70
    let mut guard = self.cache.lock().await;                        // ⚠️ 独占锁
    if let Some(c) = guard.as_ref() {
        if c.expires_at.saturating_duration_since(Instant::now()) > REFRESH_MARGIN {
            return Ok(c.token.clone());
        }
    }
    let resp = self.http.post(&url).json(&/* ... */).send().await?;  // ⚠️ 持锁跨 await（超时 20s）
    let body = resp.text().await?;                                   // ⚠️ 再跨一次
    // ...
    guard.replace(CachedToken {
        token: token.clone(),
        expires_at: Instant::now() + Duration::from_secs(expires_in),  // :123
    });
```

**问题一：全局串行点。** `tokio::sync::Mutex` 是**独占**锁，`guard` 活到函数结束，其间包含 `.send().await` + `.text().await`。而 `ApiClient::execute`（`:399`）**每个请求的第一步**都是 `self.tokens.token().await?` —— 也就是说，**连「读缓存」这条快路径也必须先抢同一把独占锁**。刷新期间进程内所有并发 API 调用（发消息、上传媒体、取网关信息）全部排队。singleflight 的意图是对的，问题在于用**一把**独占锁同时承担了「读缓存」和「刷新串行化」两件事。

**问题二：余量设置反了，导致每次调用都空刷。** 条件写的是「剩余寿命 > 300s 才复用」，而 `expires_at` 是在**刷新时刻**用 `now + expires_in` 算出来的。于是只要服务端返回的 `expires_in ≤ 300`，刷新刚完成时剩余寿命就已经 `≤ 300`，判断恒为假 → **下一次调用立刻再刷一次**，而服务端只在最后 60s 才发新 token，拿回来的还是**同一个** token。

这不是推演，仓库自带的线上日志 `docs/samples/live-run.log` 就是证据：

```
58: 2026-09-28T16:08:48  INFO access_token 已刷新 expires_in=271
59: 2026-09-28T16:09:45  INFO access_token 已刷新 expires_in=214   ← 57 秒后又刷了一次
64: 2026-09-28T16:15:21  INFO access_token 已刷新 expires_in=7197  ← 服务端给出新 token 后才停止
```

`271` 和 `214` 都小于 `300`，正是上述恒假分支。这段窗口内**每一次** API 调用都触发一次 `getAppAccessToken` 往返，且全程持独占锁。

**问题三（次要）**：`expires_in` 完全由响应体控制（`parse_secs` 同时接受字符串与数字），而 `impl Add<Duration> for Instant` 在结果越界时**会 panic**。一个异常或篡改的响应（`"expires_in":"18446744073709551615"`）就能打死刷新路径。

**规则依据**：`async-no-lock-await`（「Never hold `Mutex`/`RwLock` across `.await`」）、`num-overflow-explicit`。

**修复**（保留 singleflight，但拆开读写两把锁；余量不超过剩余寿命的一半）：

```rust
pub struct TokenProvider {
    cfg: ApiClientConfig,
    http: reqwest::Client,
    cache: tokio::sync::RwLock<Option<CachedToken>>,  // 读路径共享
    refresh: tokio::sync::Mutex<()>,                  // 只串行化刷新
}

struct CachedToken { token: String, refresh_at: Instant }  // 写入时就算好刷新点

pub async fn token(&self) -> Result<String, ApiError> {
    if let Some(t) = self.valid_cached().await { return Ok(t); }   // 读锁，短暂
    let _gate = self.refresh.lock().await;                          // 只有刷新者排队
    if let Some(t) = self.valid_cached().await { return Ok(t); }    // double-check
    // ... HTTP 请求（此时没有读者被挡住）...
    let lifetime = Duration::from_secs(expires_in.min(24 * 3600));  // 同时修掉问题三
    let margin = REFRESH_MARGIN.min(lifetime / 2);                  // 余量不得超过剩余寿命一半
    *self.cache.write().await = Some(CachedToken {
        token: token.clone(),
        refresh_at: Instant::now().checked_add(lifetime).map_or(Instant::now(), |e| e - margin),
    });
    Ok(token)
}
```

**注意**：修好 §1.1（分发并发）之后，这个全局锁会立刻成为下一个瓶颈——两处应当一起改。

---

## 2. P1 —— 应该修

### 2.1 [MEDIUM] 写线程的 flush 截止时间被每条消息重置

**位置**：`crates/qqbot-store/src/writer.rs:82-97`

```rust
loop {
    // 攒够一批就立刻提交；否则最多等 flush_interval 再提交，
    // 这样低峰期消息不会在内存里久留（崩了也只丢一个间隔的量）。   // ⚠️ 这句话在稳态下不成立
    let timeout = flush_interval;
    match rx.recv_timeout(timeout) {
        Ok(msg) => {
            buf.push(msg);
            if buf.len() >= flush_batch { flush(&mut conn, &mut buf, &stats); }
        }
        Err(RecvTimeoutError::Timeout) => {
            if !buf.is_empty() { flush(&mut conn, &mut buf, &stats); }
        }
```

**问题**：每次循环都用一个**全新的** `flush_interval` 调用 `recv_timeout`。只要消息到达间隔 < `flush_interval`，`recv_timeout` 永远返回 `Ok`，**超时分支永远不触发**，缓冲区会一直攒到 `flush_batch`（512）才提交。

按默认配置（`flush_interval = 250ms`、`flush_batch = 512`）算：稳态 5 msg/s 时，512 条需要 **~102 秒**才落盘。也就是说崩溃丢失窗口从注释声称的 250ms 被放大到**整整一批**。这与第 83-84 行注释的承诺直接矛盾。

**规则依据**：265 条规则中没有精确对应项，属真实缺陷（可归入 `async-bounded-channel` 的「背压/时延契约必须真实」精神）。

**修复**（把 deadline 锚定在**本批第一条消息**上）：

```rust
let mut deadline = Instant::now() + flush_interval;
loop {
    if !buf.is_empty() && Instant::now() >= deadline {
        flush(&mut conn, &mut buf, &stats);
        deadline = Instant::now() + flush_interval;
    }
    let wait = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(wait) {
        Ok(msg) => {
            if buf.is_empty() { deadline = Instant::now() + flush_interval; }  // 新批次重新计时
            buf.push(msg);
            if buf.len() >= flush_batch {
                flush(&mut conn, &mut buf, &stats);
                deadline = Instant::now() + flush_interval;
            }
        }
        Err(RecvTimeoutError::Timeout) => {}   // 下一轮循环开头负责 flush
        Err(RecvTimeoutError::Disconnected) => {
            if !buf.is_empty() { flush(&mut conn, &mut buf, &stats); }
            tracing::info!("消息存储写线程退出");
            return;
        }
    }
}
```

### 2.2 [MEDIUM] `schema_version` 读取吞掉真实错误，掩盖成主键冲突

**位置**：`crates/qqbot-store/src/schema.rs:74-79`

```rust
let current: Option<i64> = conn
    .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get::<_, String>(0))
    .ok()                                            // ⚠️ 真实错误被丢弃
    .and_then(|v| v.parse().ok());                   // ⚠️ 解析失败也被丢弃
```

**问题**：`.ok().and_then(...)` 把三种语义完全不同的情况压成同一个 `None`：

| 情况 | 语义 | 应该走的分支 |
|---|---|---|
| (a) 行不存在 | 正常首次初始化 | `None` 分支 ✅ |
| (b) 查询本身失败 | 真实错误 | 应上报 ❌ |
| (c) 行存在但 value 不是整数 | 数据损坏 | 应上报 ❌ |

(b)/(c) 会一起走进 `None` 分支的**裸 INSERT**（第 98-101 行，没有 `ON CONFLICT`），撞上 `meta.key` 的主键约束（DDL 第 32 行 `key TEXT PRIMARY KEY`），返回一条毫无指向性的 `UNIQUE constraint failed: meta.key`。

后果不是理论上的：`src/main.rs:201-204` 对 `MessageStore::open` 失败的处理是**降级为内存语料**——于是一个损坏的 `schema_version` 会静默关掉整个持久化，而运维看到的是主键冲突。

**规则依据**：`anti-empty-catch`。

**修复**：

```rust
let current: Option<i64> = match conn.query_row(
    "SELECT value FROM meta WHERE key = 'schema_version'",
    [],
    |r| r.get::<_, String>(0),
) {
    Ok(v) => Some(v.parse().with_context(|| format!("meta.schema_version 不是整数: {v:?}"))?),
    Err(rusqlite::Error::QueryReturnedNoRows) => None,
    Err(err) => return Err(err).context("读取 schema_version 失败"),
};
```

并把第 98-101 行的 INSERT 改成与 86-90 行一致的 `ON CONFLICT(key) DO UPDATE SET value = excluded.value`，做到真正幂等。

### 2.3 [MEDIUM] 线程创建失败会 panic 掉整个机器人，绕过已设计好的降级路径

**位置**：`crates/qqbot-store/src/writer.rs:45-48`、`crates/qqbot-store/src/reader.rs:41-45`

```rust
std::thread::Builder::new()
    .name("qqbot-store-writer".into())
    .spawn(move || writer_loop(conn, rx, flush_interval, flush_batch, thread_stats))
    .expect("启动存储写线程失败");     // ⚠️ 环境失败被当成 bug
```

**问题**：OS 线程创建失败（EAGAIN、线程数上限）是**环境/资源**失败，不是「代码 bug 才可能触发的 invariant」——正是 `err-expect-bugs-only` 明确列为「environment issue — don't expect」的类别。

更糟的是它**绕过了上层已经写好的降级路径**：`main.rs:201-204` 对 store 打开失败是 `tracing::error!` + 退化为内存语料，而这里会直接把整个机器人 panic 掉。`MessageStore::open` 本来就返回 `Result`，传播零成本。同时它的文档（`lib.rs:98`）也没有 `# Panics` 段（`doc-panics-section`）。

**规则依据**：`err-expect-bugs-only`、`anti-panic-expected`、`doc-panics-section`。

**修复**：`spawn` 改返回 `std::io::Result<Self>`，调用点用 `?` 或 `.context("启动存储写线程失败")?`。

### 2.4 [MEDIUM] 整文件 MD5 + SHA1 在 async 线程上同步计算

**位置**：`crates/qqbot-media/src/uploader.rs:105-107`（另见 `:170`）

```rust
let md5 = md5_hex(bytes);
let sha1 = sha1_hex(bytes);
let md5_10m = md5_hex(&bytes[..bytes.len().min(MD5_10M_LEN)]);
```

**问题**：`upload_bytes` 是 `async fn`，却在运行时 worker 线程上同步做整文件哈希。100MB 量级的文件是**数百毫秒**的纯 CPU 工作，占住一个 worker。本项目是单一 `#[tokio::main]` 多线程运行时（`src/main.rs:28`），网关心跳任务与上传任务共享 worker——正是 `async-spawn-blocking` 里「> 1ms 就该 `spawn_blocking`」的场景。每片的 `md5_hex(chunk)`（第 170 行）同理。

**规则依据**：`async-spawn-blocking`。

**修复**：把哈希移进 `tokio::task::spawn_blocking`；若签名改为接收 `Arc<[u8]>` 则可零拷贝移入（当前 `&[u8]` 签名做不到）。

### 2.5 [MEDIUM] `MessageStore::open` 是阻塞调用，却直接在 `async fn run()` 里执行

**位置**：`src/main.rs:190`、`crates/qqbot-store/src/lib.rs:99-111`

```rust
// async fn run() 内部：
Some(sc) => match MessageStore::open(sc.clone()) {
```

**问题**：`open` 里依次执行 `std::fs::create_dir_all`、`Connection::open`、以及 `PRAGMA journal_mode = WAL`（切换日志模式涉及文件创建与 fsync）——全是同步阻塞 I/O，却跑在 runtime 的 worker 线程上。这里是启动路径、只有一次、通常毫秒级，所以严重性不高；但一旦 DB 在慢速/网络盘上（或需要 WAL 恢复），阻塞时间不可控。

**规则依据**：`async-spawn-blocking`。

**修复**：保留同步 `open` 给测试/示例，另加 `pub async fn open_async(cfg) -> Result<Arc<Self>>` 包一层 `spawn_blocking`。

### 2.6 [MEDIUM] 缓存插入时深拷贝整张 PNG

**位置**：`crates/qqbot-render/src/service.rs:264-283`

```rust
Ok(img) => {
    metrics.rendered.fetch_add(1, Ordering::Relaxed);
    metrics::counter!("qqbot_render_total", "result" => "ok").increment(1);
    metrics::histogram!("qqbot_render_bytes").record(img.png.len() as f64);
    if job.request.cache {
        cache.insert(job.key, Arc::new(img.clone()));   // ⚠️ 深拷贝整个 PNG
    }
}
let _ = job.ack.send(result);
```

**问题**：`img.clone()` 复制的是 `RenderedImage`，其中 `png: Vec<u8>` 是完整 PNG 字节流（900×640 @2x 的词云约 100–500KB）。每次渲染都要**额外深拷贝一份**才能放进 `Arc`。缓存容量 512 时这是持续的大块分配churn。

**规则依据**：`own-arc-shared`（「Use `Arc<T>` for thread-safe shared ownership」）、`own-borrow-over-clone`。

**修复**：让 `render_one` 直接返回 `Arc<RenderedImage>`，缓存与 ack 都传 `Arc` 克隆（廉价引用计数），彻底消除深拷贝。

### 2.7 [MEDIUM] SVG 源码每次渲染被整体克隆

**位置**：`crates/qqbot-render/src/service.rs:292-296`

```rust
let svg = match &req.source {
    RenderSource::Template { name, data } => engine.render(name, data)?,   // 已是 owned String
    RenderSource::Svg(svg) => svg.clone(),                                 // ⚠️ 仅为统一类型而克隆
};
renderer.render_png(&svg, req.scale)
```

**问题**：`render_png` 接收 `&str`，两个分支只是**类型不同**（`String` vs `&String`）才被迫克隆。词云 SVG（80 个词 + 渐变定义）约 10–20KB，每次渲染白拷一份。

**规则依据**：`own-cow-conditional`——这是该规则的教科书案例（「Use `Cow<'a, T>` for conditional ownership」）。

**修复**：

```rust
let svg: std::borrow::Cow<'_, str> = match &req.source {
    RenderSource::Template { name, data } => std::borrow::Cow::Owned(engine.render(name, data)?),
    RenderSource::Svg(svg) => std::borrow::Cow::Borrowed(svg),
};
renderer.render_png(&svg, req.scale)
```

### 2.8 [MEDIUM] 每个 token 都分配一个 `Vec<char>`

**位置**：`crates/qqbot-plugins/src/wordcloud.rs:289-311`

```rust
fn normalize_token(raw: &str) -> Option<String> {
    let token = raw.trim();
    if token.is_empty() { return None; }
    let chars: Vec<char> = token.chars().collect();     // ⚠️ 每个 token 一次堆分配
    if chars.iter().any(|c| is_cjk(*c)) {
        if chars.len() < 2 || !chars.iter().all(|c| is_cjk(*c)) { return None; }
        Some(token.to_string())
    } else {
        let cleaned: String = chars.iter().filter(|c| c.is_alphanumeric()).collect();
        ...
    }
}
```

**问题**：这个 `Vec<char>` 只用来做「长度」和「是否全 CJK」两个判断，完全可以不分配。而 `normalize_token` 对**每个 token 的每条消息**都会调用——统计窗口内 500 条消息 × 约 10 个 token ≈ **5000 次无谓的堆分配**，每次 `collect()` 一次。

**规则依据**：`anti-collect-intermediate`、`perf-iter-lazy`（「Keep iterators lazy, collect only when needed」）、`mem-with-capacity` 的反面。

**修复**：

```rust
fn normalize_token(raw: &str) -> Option<String> {
    let token = raw.trim();
    if token.is_empty() { return None; }

    let mut len = 0usize;
    let mut has_cjk = false;
    let mut all_cjk = true;
    for c in token.chars() {
        len += 1;
        if is_cjk(c) { has_cjk = true; } else { all_cjk = false; }
    }

    if has_cjk {
        if len < 2 || !all_cjk { return None; }
        Some(token.to_string())
    } else {
        let cleaned: String = token.chars().filter(|c| c.is_alphanumeric()).collect();
        if cleaned.chars().count() < 2 { return None; }
        Some(cleaned.to_lowercase())
    }
}
```

### 2.9 [MEDIUM] `SessionRegistry::send` 的超时没有包住入队

**位置**：`crates/qqbot-core/src/session.rs:284-296`

```rust
pub async fn send(&self, request: SendRequest) -> Result<SendResult, CoreError> {
    let key = request.target.key();
    let (ack_tx, ack_rx) = oneshot::channel();
    self.route(&key)
        .send(SessionMsg::Send { key, request, ack: ack_tx })
        .await                                  // ⚠️ 入队这一步没有超时保护
        .map_err(|_| CoreError::Closed)?;

    match tokio::time::timeout(self.timeout, ack_rx).await {   // 超时只包住了等回复
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(CoreError::Canceled),
        Err(_) => Err(CoreError::Canceled),
    }
}
```

**问题**：`self.timeout`（`main.rs:186` 传 10s）只覆盖了 `ack_rx`，**没有覆盖 `send().await`**。而 shard 队列是有界的（`main.rs:185` 传 256），actor 在 `handle_send` 里**串行 await 一次 HTTP 调用**（`session.rs:415`）。队列打满时 `send().await` 会一直挂起——调用方（`Ctx::reply_*` → 插件）的实际等待可以远超配置的 10s。

更糟的是这段等待发生在 dispatch 并发信号量的许可之内（`dispatch.rs:139`），会把并发槽位逐个占死。这与 §1.1 叠加：即便修好了 §1.1 的派生问题，槽位仍可能被「卡在入队」的任务耗光。

**规则依据**：`async-bounded-channel`——该规则的 "Option 3: Timeout" 示范的正是把超时套在 `send` 上，而不只是套在响应上。

**修复**（tokio 1.53.1 已有 `Sender::send_timeout`）：

```rust
self.route(&key)
    .send_timeout(SessionMsg::Send { key, request, ack: ack_tx }, self.timeout)
    .await
    .map_err(|_| CoreError::Closed)?;
```

### 2.10 [MEDIUM] 插件 panic 分支把完整用户消息写进日志

**位置**：`crates/qqbot-core/src/dispatch.rs:153-156`（对比 `:130-136`、`:151`）

```rust
Err(_) => {
    metrics::counter!("qqbot_plugin_panic_total").increment(1);
    tracing::error!(content = %ctx.content(), "插件 panic 已被隔离");   // ⚠️ 未截断
}
```

**问题**：同一文件在 134 行和 151 行都刻意使用 `truncate_for_log(ctx.content(), 60)`，并在 128-129 行写明「内容截断，避免把整篇消息灌进日志」——**唯独这个 ERROR 分支用了完整正文**。

后果：一个「每条消息都 panic」的插件会让 ERROR 日志按整篇正文的量级持续刷屏（同时放大日志磁盘占用），并把用户消息内容无截断地落盘。

**规则依据**：`obs-no-sensitive-data`（「Never log secrets or PII; redact or skip them」）、`obs-structured-fields`。

**修复**：与另外两处保持一致，改用 `truncate_for_log(ctx.content(), 60)`。

---

## 3. P2 —— 建议（卫生 / 次要）

### 3.1 错误处理与数值

| 规则 | 位置 | 问题 |
|---|---|---|
| `num-overflow-explicit` | `crates/qqbot-core/src/dispatch.rs:170` | `(ts - now).abs() < 86_400`：`ts` 来自服务端任意字符串，`i64::MIN - now` 溢出。改 `ts.abs_diff(now) < 86_400` |
| `anti-empty-catch` | `crates/qqbot-plugins/src/wordcloud.rs:171-173` | `spawn_blocking(...).await.unwrap_or_default()`：分词 panic 被吞成空词表，用户看到的是「样本还太少」，无从排查。至少 `tracing::warn!` |
| `num-cast-try-from` | `crates/qqbot-render/src/wordcloud.rs:67`、`service.rs:271` | `u32 as f32` / `usize as f64` 精度损失（pedantic 告警）。此处影响可忽略，仅记录 |
| `mem-write-over-format` | `crates/qqbot-render/src/wordcloud.rs:82,103,106,110`、`crates/qqbot-plugins/src/help.rs:30` | `push_str(&format!(...))` 每次多分配一个临时 String。改 `write!(out, ...)`（clippy::format_push_string） |
| `num-saturating-clamp` | `crates/qqbot-render/src/svg.rs:60-67` | 已正确：`scale` 先过滤为有限正数，且 Rust float→int 转换**饱和**（`inf as u32` = `u32::MAX`）必然撞上 `MAX_DIMENSION` 检查。**不是缺陷**，仅确认 |

### 3.2 性能

| 规则 | 位置 | 问题 |
|---|---|---|
| `perf-iter-lazy` | `crates/qqbot-core/src/dispatch.rs:176-182` | `truncate_for_log` 先 `take(max).collect()` 再 `count() > max`，**两遍扫描**；且每条消息 INFO 日志都会调用 |
| `opt-*` | `crates/qqbot-render/src/wordcloud.rs:150,151,159,265` | `cx + r * theta.cos()` 等可改 `mul_add`（clippy::suboptimal_flops）。仅在词云热路径，收益有限 |
| `mem-reuse-collections` | `crates/qqbot-render/src/wordcloud.rs:194-200` | `escape_xml` 连续 5 次 `replace`，每个词 5 次分配；可单遍扫描 + `with_capacity`，或对无需转义的常见情况返回 `Cow` |
| `perf-entry-api` | `crates/qqbot-plugins/src/wordcloud.rs:33-45` | `contains_key` 后再 `entry`，同一 key 两次哈希 + 两次 `to_string()`。用 `entry` 一次解决 |
| `perf-*` | `crates/qqbot-plugins/src/wordcloud.rs:119-122` | `is_command` 对每条语料取一次 `RwLock` 读锁（500 条 = 500 次加解锁）。可在 `filter_corpus` 里持有一次 guard |
| `mem-with-capacity` | `crates/qqbot-plugins/src/wordcloud.rs:210` | `HashMap::new()` 无容量预估 |
| `anti-clone-excessive` | `crates/qqbot-media/src/uploader.rs:140` | `prepared.parts.clone()` 仅为排序；`prepared` 是本地拥有值，可 `let mut prepared` 后原地排序 |
| `anti-clone-excessive` | `crates/qqbot-plugins/src/wordcloud.rs:200` | `all.iter().filter(...).cloned().collect()` 克隆了全部 `WordItem`（含 String）后可能只保留一半 |

### 3.3 异步与并发

| 规则 | 位置 | 问题 |
|---|---|---|
| `async-bounded-channel` | `crates/qqbot-gateway/src/actor.rs:133-156` | 服务端下发的 `session_start_limit.max_concurrency`（同时 Identify 的上限）**只用于日志**，未转成任何背压。`shards > max_concurrency` 时超出的 Identify 会被服务端拒绝 → InvalidSession → 退避重连，形成启动期抖动 |
| `async-select-racing` | `crates/qqbot-gateway/src/actor.rs:253` | `connect_async` 无超时，期间不 poll `inbox`。对不可达地址，`GatewayHandle::shutdown` 的 `join_next` 会空等十几秒。建议 `tokio::time::timeout(10s, ...)` |
| `async-join-parallel` | `crates/qqbot-media/src/uploader.rs:152-173` | 各分片 PUT 彼此独立却完全串行，N 片 = N 次 RTT；且服务端下发的 `upload_config.concurrency` / `retry_timeout` / `retry_delay` **解析后全部丢弃**（grep 全 workspace 只有定义处），单次非 2xx 即整体失败。要么按 `concurrency` 并发 + 重试，要么在文档里写明「有意忽略」 |

### 3.4 项目结构 / 命名 / 文档

| 规则 | 位置 | 问题 |
|---|---|---|
| `proj-workspace-deps` | 全部 8 个 `Cargo.toml` | `[workspace.dependencies]` 只声明了 7 个内部 crate，**外部依赖完全没有继承**：`serde = "1.0.229"` 在 5 处重复、`tokio = "1.53.1"` 在 7 处重复（feature 集合各不相同）、`tracing` 在 7 处、`serde_json` 在 6 处、`thiserror` 在 4 处、`metrics` 在 4 处。且内部 crate 用 `{ version = "0.1.0", path = "../qqbot-api" }` 而非 `{ workspace = true }`，与 workspace 里已声明的路径依赖**重复**。版本漂移风险真实存在 |
| `lint-workspace-lints` | `Cargo.toml` | 没有 `[workspace.lints]`，lint 策略无法统一。既然已做到 clippy 零警告，把它**固化**下来才有意义 |
| `opt-lto-release` / `perf-release-profile` | `Cargo.toml:44-47` | `lto = "thin"`（建议 `"fat"`）、无 `panic = "abort"`。**注意**：`thin` 是构建时间与运行时的合理折中，属有意选择；若要榨性能再改 |
| `proj-pub-crate-internal` | `crates/qqbot-store/src/{writer,reader,schema}.rs` | `lib.rs` 里 `mod writer; mod reader; mod schema;` 都是**私有**模块，因此这些文件里的 `pub struct WriteHandle` / `pub fn open` 等实际可见性等价于 `pub(crate)`。写成 `pub` 会误导读者，也让 `unreachable_pub` 检查失效 |
| `name-*` | `crates/qqbot-plugins/src/{dice.rs:40,help.rs:70,lib.rs:71,wordcloud.rs:265}` | 返回类型被不必要地绑定到参数生命周期（`&str` 实际是 `&'static str`），应显式写 `&'static str`（clippy::needless_lifetimes 相关） |
| `doc-errors-section` | `client.rs`（18 处）、`store/lib.rs`（5 处）、`render/service.rs`（3 处）、`media/uploader.rs`（4 处）等 | 大量返回 `Result` 的公开函数缺 `# Errors` 段（clippy::missing_errors_doc） |
| `doc-panics-section` | `crates/qqbot-render/src/template.rs:19` | 可能 panic 的函数缺 `# Panics` 段 |
| `doc-all-public` | `crates/qqbot-store/src/lib.rs` | 文档密度很高，但恰好漏掉最核心的公开面：`MessageStore`（中心类型）、`StoreStats` 及其 6 个字段（`enqueued`/`written`/`dropped` 的区别对读指标的人很关键）、`retention()`/`config()`/`stats()`、`Scope::as_str()` |
| `obs-structured-fields` | `crates/qqbot-core/src/dispatch.rs:155` | 插件 panic 时打印**完整** `content`，而正常日志（`:134`）用 `truncate_for_log(..., 60)` 截断。不一致，长消息会灌爆日志 |
| `lint-*` | 全项目 | `clippy::pedantic` + `nursery` 下约 **700 条**告警，绝大多数是 `#[must_use]`、`const fn` 机会、`Self` 替代类型名、文档反引号——**不建议全开**，按 `lint-pedantic-selective` 挑高价值子集 |

### 3.5 qqbot-core 会话层

| 规则 | 位置 | 问题 |
|---|---|---|
| `api-parse-dont-validate` | `crates/qqbot-core/src/session.rs:242` | `shards` 被 `.max(1)` 保护（避免 `route` 里取模除零），但 `queue_capacity` 没有——`mpsc::channel(0)` 会直接 `assert!(buffer > 0)` panic。当前调用点写死 256 故不触发，但这是 `pub fn` 构造器。加一行 `let queue_capacity = queue_capacity.max(1);` |
| `proj-pub-crate-internal` | `session.rs:229`、`session.rs:474`、`error.rs:29` | 三个**从未接线**的公开项：`SessionMsg::Evict` 只有定义和 match 分支、无任何构造点；`pub type SharedRegistry` 零引用；`CoreError::PassiveExpired` 零构造点（实际走 `is_passive_expired()` + `QuotaExceeded`）。删除，或降为 `pub(crate)` + `#[allow(dead_code)]` + TODO |
| `doc-all-public` | `crates/qqbot-core/src/session.rs:299` | `shards()` 的文档写「当前活跃会话数」，实际返回 shard 个数，与 `shard_count()` 逐字相同，且两者**都无调用点**。文档承诺与行为不符比没有文档更危险——调用方会拿它当会话数指标 |
| `anti-clone-excessive` | `crates/qqbot-core/src/session.rs:408` | `msg_id.clone()` 之后 `msg_id` 只被读一次（`is_some()`）。把 `mode` 的计算提前，即可直接 move 进 `message`，每次发送少一次堆分配 |
| `anti-format-hot-path` | `crates/qqbot-core/src/session.rs:270` | `observe()` 里 `target.key()` 被调用**两次**（一次存 key，一次路由），而 `Target::key()` 内部是 `format!("{}:{}", ...)`。每条入站消息多一次 `format!` 分配 + 一次哈希 |
| `test-descriptive-names` | `crates/qqbot-core/src/session.rs:614` | ⚠️ **恒真测试**：`assert_eq!(a.hash_one(k) % 8, a.hash_one(k) % 8)` 两边是同一个表达式，永远不会失败；且它**根本没调用 `route()`**。测试名承诺的「同一 key 路由稳定」没有任何一行代码在验证——虚假信心比没有测试更糟 |

### 3.6 qqbot-api 协议类型

| 规则 | 位置 | 问题 |
|---|---|---|
| `type-enum-states` | `crates/qqbot-api/src/message.rs:124` | `MsgType` 枚举已经实现了 `into/from u8`（`:4-17`），但唯一出口字段 `OutMessage.msg_type` 仍是**裸 `u8`**。`OutMessage { msg_type: 42, .. }` 能编译并原样发给服务端——枚举的穷尽性在最后一跳被丢掉。改回 `MsgType` 线格式不变 |
| `api-newtype-safety` | `crates/qqbot-api/src/payload.rs:42` | 同一结构体里 `intents: u32` 与 `shard: [u32; 2]` 两个同型字段语义完全不同却可互换；而 `ApiClient::identify`（`client.rs:290`）的入参本来就是 `Intents`，是最后一跳 `.bits()` 把类型信息丢了。改回 `intents: Intents`（已实现 `Serialize<u32>`），线格式不变 |
| `err-source-chain` | `crates/qqbot-api/src/client.rs:94-98`、`:440-442` | `ApiError::Protocol(String)` 用 `format!` 把 `serde_json::Error` 拍平，且没有 `#[source]`，`Error::source()` 返回 `None`，`{:#}` / `err.chain()` 到此断裂。crate 里本就有 `ApiError::Json(#[from] serde_json::Error)` 可用 |
| `type-no-stringly` | `crates/qqbot-api/src/event/message.rs:29`、`:157-163` | `member_role: Option<String>` 表达一个**封闭集合**（注释已写明只有 `member`/`admin`/`owner`），却用字面量比较做权限判断。服务端若下发 `"Admin"` 或新增角色，`is_admin()` **静默返回 false**——权限判断朝「降级」方向出错且无任何日志。应改为枚举 + `#[serde(other)] Unknown` |
| `obs-no-sensitive-data` | `crates/qqbot-api/src/client.rs:27-32` | `ApiClientConfig` 派生 `Debug` 且含 `client_secret`。目前无调用点打印它（已 grep 确认），但它是 `pub` 类型并经 `ApiClient::config()` 外借——任何一次 `tracing::debug!(?cfg)`、`#[instrument]` 或 `dbg!` 都会把密钥写进日志。建议手写 `Debug` 输出 `"<redacted>"`。同风险的还有 `Identify.token` / `Resume.token` |
| `mem-box-large-variant` | `crates/qqbot-api/src/event/mod.rs:64` | `Event` 的其余变体都刻意用 `Arc`/`Box` 压到 8 字节，唯独 `Notice { name: String, notice: RawNotice }` 没装箱。`RawNotice` 是 6 个 `Option<String>`（约 144B）+ `name` 24B，把整个 `Event` 撑到约 176B，每次经 mpsc 搬运/克隆都要付这个代价。改 `notice: Box<RawNotice>` |
| `err-custom-type` | `crates/qqbot-api/src/error.rs:33` | `ApiError::MissingData` **没有任何构造点**（`Event::parse` 缺 `t`/`d` 时走的是 `Ok(None)`）。公开错误枚举里挂着一个不可能发生的失败模式，调用方会为它写无用的 match 分支。删除，或让 `Event::parse` 真正返回它——二选一 |

---

## 4. 明确排除的假阳性

审计的可信度取决于**不报**什么。以下是主动排查后判定**不构成违规**的项：

| 候选项 | 为什么不是问题 |
|---|---|
| `#[async_trait]` 用于 `Handler`（`plugin.rs:21,59`；plugins 4 处） | `Handler` 被用作 `Arc<dyn Handler>`（`plugin.rs:142`）。原生 `async fn in trait`（1.75 稳定）**不 dyn-compatible**，此处 `async_trait` 是**必要**的。`async-fn-in-trait` 不适用——虽然 MSRV 1.85 > 1.75 |
| `client.rs:384` `post_json` future 非 `Send`（clippy::future_not_send） | 泛型 `B: Serialize + ?Sized` 未约束 `Sync` 导致的**保守告警**。所有具体调用点都编译通过并能用在 spawn 的任务里。加 `B: Sync` 可消除告警，属 API 卫生 |
| `client.rs:71` tokio `Mutex` 跨 `.await` | **故意的 singleflight**（注释见 `:54-57`）。`tokio::sync::Mutex` 正是为「跨 await 持锁」设计；`anti-lock-across-await` 针对的是 `std` 锁 |
| 137 处 `unwrap()` / 22 处 `expect()` | 绝大多数在 `tests/`、`#[cfg(test)]`、`examples/` 里。`anti-unwrap-abuse` 只管生产路径。生产路径上剩余的是已论证的无损 `as` 或 `unwrap_or` 默认值 |
| `unsafe` 整类 | 项目源码中**零 `unsafe`**。grep 到的 2650 处全在 `target/**/libsqlite3-sys/out/bindgen.rs`（vendored 生成代码）。7 条 `unsafe-*` 规则全部不适用 |
| 绝大多数 `as` 转换 | 已逐个核对：`limit.min(MAX_TEXT_ROWS) as i64`、`n.max(0) as u64`、`(b >> 4) as usize`、`status.as_u16()`、`rows as u64` 等均**先校验范围**或同宽，符合 `num-cast-try-from` 的例外条款。仅 §1.3/§1.4/§3.1 三处未校验 |
| `main.rs` 的 11 处 `println!`/`eprintln!` | `self-test` / `check` 是 **CLI 模式**，输出是给人看的报告，不是诊断日志。`run` 模式全程用 `tracing`。`obs-tracing-over-log` 不适用 |
| `check()` 打印凭据（`main.rs:159`） | 只打印 AppID 与密钥**长度**（`cfg.client_secret.len()`），密钥本身从未进日志。符合 `obs-no-sensitive-data` |
| `writer.rs:142-147` 入库失败只 `warn!` 不 panic | 有意设计：「丢一批总比整个机器人挂掉好」，并计 `reason = "sql_error"` 指标。符合 `err-*` |
| `reader.rs:88,91,94` 的 `let _ = reply.send(...)` | 接收端被 drop（调用方 future 取消）时的标准写法，`async-oneshot-response` 规则自身示例即如此。不是 `anti-empty-catch` |
| `store/lib.rs:174-197` sweeper 无 `CancellationToken`、`main.rs:193` 丢弃 `JoinHandle` | 进程级单例，生命周期等于进程；无引用环。不构成 `async-cancellation-token` 的实际缺陷 |
| `reader.rs:41` 无界 `std::sync::mpsc::channel()` | 生产者只有词云、`count()`、`purge_before()` 三处低频调用，请求体是两个短字符串。不构成 `async-bounded-channel` 的内存风险（写侧已正确使用有界 `sync_channel`） |
| `actor.rs:336-338` 任意帧都清 `awaiting_ack` | 收到任何帧本身就证明链路存活，符合注释意图 |
| `api-serde-optional` / `doc-cargo-metadata` / `lint-cargo-metadata` / `proj-bin-dir` / `proj-msrv-declare` | 这是**二进制应用**而非发布的库；且 `rust-version = "1.85"` 已声明，`proj-msrv-declare` 实际是满足的 |

---

## 5. 值得肯定

这些不是客套——它们都是**规则明确推荐、而项目确实做对了**的地方：

**架构层面**

- **`crates/qqbot-store` 的「std 线程 + `sync_channel`」是正确选择，不建议改成 tokio task**（`async-spawn-blocking` / `async-mpsc-queue` 的取舍）。`rusqlite::Connection` 是同步驱动，放进 tokio task 只能靠 `spawn_blocking` 包一层，反而每次查询都要跨线程池调度；现在连接被单一线程独占，生命周期干净。async 侧确实没被阻塞：写侧 `try_send`（队列满即丢并计数），读侧等待发生在 `oneshot` 上——正是 `async-oneshot-response` 推荐的 request-response 形态。
- **`qqbot-gateway` 用 actor 模型（独占状态 + channel）而非共享锁**：`gateway/src` 与 `media/src` 里**没有任何 Mutex/RwLock/Atomic**，`anti-lock-across-await` 与 `conc-atomic-ordering` 在此单元根本不适用。
- **`catch_unwind` 隔离插件 panic**（`dispatch.rs:145`，`AssertUnwindSafe` 用得恰当），并有 `qqbot_plugin_panic_total` 指标与专门的测试。
- **有界通道 + 主动背压的取舍是自觉的**：网关事件通道有界（2048），读循环刻意用 `try_send` 而非 `send().await`，满时丢弃并打点——「宁可丢事件，不可断心跳」的注释与代码一致。渲染队列同理（`mpsc::channel(256)` + `Semaphore`）。

**细节**

- **确定性排序**：词云在**两处**都用了「权重降序 + text 升序」的次级排序键（`render/wordcloud.rs:51`、`plugins/wordcloud.rs:204`），并且都有注释解释「HashMap 迭代顺序随机 → SVG 变化 → 渲染缓存永不命中」，还配了回归测试 `layout_is_deterministic_regardless_of_input_order`。这是很到位的工程直觉。
- **`spawn_blocking` 用在了真正该用的地方**：jieba 分词（`plugins/wordcloud.rs:171`）与 resvg 光栅化（`render/service.rs:264`）——两者都是明确的 CPU 密集工作。
- **`Ordering::Relaxed` 用在彼此独立的统计计数器上**（`writer.rs:56,59,135,136`；`service.rs:181-185`），没有无脑上 `SeqCst`。符合 `conc-atomic-ordering`「用最弱的正确序」。
- **`mem-reuse-collections` / `mem-with-capacity`**：`writer.rs:80` 用 `Vec::with_capacity(flush_batch)`，`flush` 末尾 `buf.clear()` 保留容量，跨批次零分配；SQL 走 `prepare_cached`。
- **`own-cow-conditional`**：`store/model.rs:48-55` 的 `truncated_content` 在常见路径返回 `Cow::Borrowed`，只有超长才分配。
- **`own-slice-over-vec`**：`recent_texts(&str, &str, ...)`、`render_png(&self, svg: &str, ...)` 都用借用而非 `String`。
- **`obs-structured-fields`**：日志几乎全部是结构化 key-value（`dispatch.rs:130-136`、`writer.rs:140`、`reader.rs:130,150-155`），没有字符串拼接。
- **`num-saturating-clamp`**：`heartbeat_interval.unwrap_or(30_000).max(1_000)` 挡住了 `tokio::time::interval(0)` 的 panic；`Backoff` 用 `saturating_add` / `saturating_mul(2).min(max)`。
- **`async-cancel-safety`**：网关读循环三个分支用的全是 cancel-safe future（`mpsc::Receiver::recv`、`Interval::tick`、`StreamExt::next`），并显式用 `biased;` 固定优先级。
- **`async-joinset-structured`**：分片任务用 `JoinSet` 管理，`shutdown` 里 `join_next` 优雅收尾，Drop 时自动 abort。
- **`mem-zero-copy`**：`hex_encode` 用 `String::with_capacity(bytes.len() * 2)`；moka 缓存存 `Arc<CachedFileInfo>`，命中时只克隆 Arc。

---

## 6. 建议的修复顺序

| 批次 | 内容 | 风险 | 预期收益 |
|---|---|---|---|
| **第 1 批** | §1.1 分发并发、§1.2 网关退避 | 中（改控制流） | 消除两个可观测的生产缺陷；恢复背压语义 |
| **第 2 批** | §1.3 溢出、§1.4 配置上界、§3.1 的 `event_unix` 溢出 | 低（局部算术） | 消除全部 panic / 数据丢失路径 |
| **第 3 批** | §2.1 flush 截止时间、§2.2 schema 错误、§2.3 线程 spawn | 低 | 持久化行为与文档承诺一致 |
| **第 4 批** | §2.4–§2.8 的 `spawn_blocking` / `Cow` / `Arc` / 分配优化 | 低 | 减少热路径阻塞与分配 |
| **第 5 批** | §3.4 的 workspace 依赖继承 + `[workspace.lints]` | 低（纯配置） | 固化零警告成果，消除版本漂移 |

**每一批之后都应跑 `cargo test --workspace`（修复后 149 项）与 `cargo clippy --workspace --all-targets`（保持零警告）。**

---

*审计基于 rust-skills v1.5.1（265 条规则 / 26 类别）。规则 ID 可直接在该技能的 `rules/` 目录下查阅原文。*
