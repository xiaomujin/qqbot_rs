# ADR 0002：运行时接缝（B 簇）

- 状态：已接受
- 范围：存储写时序、附件回查、装配、TokenProvider、`SessionShard` 发送边缘、三处注入绕过
- 与 [ADR 0001](0001-plugin-outbound-refactor.md) 的关系：0001 管「插件往外发的入口」（A 簇），本篇管「运行时内部那几条接缝」（B 簇）

## 决定

1. **写入可见性用屏障，不用凭据**：`MessageStore::flush()` 返回表示「此前已入队的写入都已提交」。`record` 保持 fire-and-forget（队列满就丢、绝不阻塞热路径）—— 给每条消息配一个 oneshot 会把分配放到消息热路径上，收益不成比例。
2. **附件回查是查询，不是扫描**：新增 `message_attachments` 表（PK(msg_id, ordinal)，索引 scope+target_id+created_at），入库时把本条与被引用消息（`message_type = 103`）的附件一并落表。`recent_raw` 整条链删除；`resolve_pending` 的三级回退全部类型化。
3. **协议知识回到协议类型旁边**：`QUOTED_MESSAGE_TYPE` 与 `MessageEvent::quoted_attachment()` 在 `qqbot-api`；插件不再啃 raw JSON。
4. **旧库不再迁移**：`SCHEMA_VERSION` 12 → 13，`upgrade()` 整个删除（含三个辅助函数与跨 9 个版本的 ALTER/DROP）；版本不等时**在建表之前**报错，文案给出「删掉 data/qqbot.db 后重启即可重建」。这是 ADR 0001 第 11 条的落地。
5. **纯函数与注入缝不被绕过**：`discover(env, cwd)` 的 cwd 是参数（`Config::load` 只取一次）；日志级别的优先级只在 `Config::resolve` 里算一次、连来源名一起带出，`init_tracing` 只做校验与回退。
6. **同一份知识只解一遍**：系统控制者名单成为 `ResourceStore` 的领域概念（`system_controllers` / `set_system_controllers` / `is_system_controller`），键名与 JSON 形状不再漏到业务插件。
7. **装配只有一份**：根 package 变成 lib + bin，`src/assembly.rs` 的 `assemble` / `assemble_with` 同时服务产品与 e2e。`App` 带出**同一个** `ApiClient`（克隆共享 token 缓存），网关与 `Services` 用它，进程里不会有两个 token 缓存。
8. **actor 内部的 IO 边缘可以替换**：`SendApi` 单方法 trait，`Arc<dyn SendApi>`（不用泛型 —— 泛型会传染到 `Services` / `Ctx`）。这不是新增 actor。
9. **接缝本身就是观察点**：`SessionMsg::Send` 的 ack 不携带「本次是主动还是被动」—— 脚本化 adapter 已经能直接看见每条 `OutMessage` 带不带 `msg_id` / `msg_seq`，给产品接口加一个没人读的字段没有意义。

## 明确不做（避免后续重提）

10. **不给 `parse_token_response` 加状态码闸门**：它收 `status`，但只用在校验失败的消息里 —— 非 2xx 只有在 body 不是 JSON 时才会报错。这是改造前就有的行为，本次逐字保留；要改是一次**有意的行为变更**，不是重构。
11. **`PluginsConfig.resources` 由装配覆盖**：资源库怎么开只此一处实现，否则又会分叉。要断言资源库的用例从 `App::resources` 取句柄。
12. **`Config::default()` 的 `store` 是 `Some(data/qqbot.db)`**（忠实照抄内置默认层）。这是脚枪，测试必须显式换路径或设 `None`。
13. **网关不进 `assemble`**：它要建真实 WebSocket，而 e2e 是直接喂 `Dispatcher` 的。e2e 覆盖装配的其余全部，含 `spawn_sweeper`（此前 e2e 一次都没走到）。
14. **不拆 `SessionRegistry`、不新增 actor**：只给已有 actor 的 IO 边缘加一个可替换点。
15. **`messages.raw` 保留**：附件已落表，但类型里没声明的字段会丢，留原文以便将来重新提取。

## 后果

- 用户可见：**无**（B 簇不改任何命令、文案或卡片）。唯一的一次性代价是**数据库必须重建**（旧库会明确报错而不是静默半残）。
- 测试面：新增/改写约 30 条进程内用例；`flush` 让 e2e 不再 sleep 轮询，5 处被压小的 `flush_interval` 全部恢复默认。
- 测试基线 496 → 514。

## 修订：评审修复轮（v14）

一次对 A/B 两簇的只读代码评审提出 11 条问题，其中 6 条是本次重构引入或退化的。
逐条处置如下。被推翻的决定不直接改写正文 —— 那会把「为什么当初这么定、后来为什么改」
的因果丢掉。

1. **第 2 条被推翻：附件不再落表，回查整级删除**（`SCHEMA_VERSION` 13 → 14）。
   原决定把「回查」做成查询，理由是「扫原文要 50 次反序列化」；但评审发现它的排序键
   `(created_at DESC, ordinal DESC)` 恰好取到**被引用**的那张（入库顺序是「本条在前」），
   而 `created_at` 只有秒级 —— 同一秒的两条消息还会取到更旧那条。
   追问之后更要紧的一点是：这一级服务的场景（先发图、再发命令）虽然写在失败文案里，
   但**它不是唯一可行的交互** —— 「回复那张图」同样自然，而且那一级不依赖任何落库状态。
   于是删掉第 3 级与 `message_attachments` 表：收录只认「本条消息自带的附件」与
   「被引用消息的附件」，**两级**。代价是「先发图、再发命令」不再被支持；
   收益是少一张没有别的读者的表，也少一条建立在秒级时间戳上的排序规则。
   顺带修掉「只取 `.first()`、而入库侧会过滤空 url」的不一致：
   `MessageEvent::quoted_attachment()` 改成返回迭代器的 `quoted_attachments()`，
   并把「空 url 不算数」定义在 `MessageAttachment::usable_url()` 上。
2. **第 1 条补强：屏障不再说谎**。`Flush` 的 ack 从 `()` 改成 `Result<(), String>`，
   写线程丢批时把错误带回调用方。计数器拆开：`dropped` 只表示「从未入队」，
   SQL 失败另计 `failed_rows` —— 原来同一批既算 `enqueued` 又算 `dropped`，
   只有 `written + failed_rows == enqueued` 才看得出来「写入到底有没有落库」。
   入队重试从 `yield_now()` 改成 `sleep(1ms)`：写线程是独立 OS 线程，
   空转让不出任何东西给它。
3. **`expires_in = 0` 的下界**：原实现只夹上界，`0` 会算出 `lifetime = 0` →
   `refresh_at == now` → `usable_at` 恒为假 → **每次调用都重取 token**，
   而重取持有刷新互斥锁，并发调用因此退化成串行。现在 `0` 当成「没给」，回落 7200。
   这不影响第 10 条：仍然不按状态码分支。
4. **版本判定补一个洞**：`meta` 表在、`schema_version` 行不在，原来被当成**新库** ——
   `CREATE TABLE IF NOT EXISTS` 修不了已存在表的形状，版本却被写成当前值，
   于是库看起来正常，直到运行期报 `no such column`（而写线程对 SQL 失败只 warn 后丢批，
   症状是「消息一条不落库，日志里只有一行 warn」）。现在明确报错要求删库；
   并且建表 + 写版本 + 播种收进**同一个事务**，让这个状态不再由崩溃产生。
5. **删掉 `auto_vacuum` / `incremental_vacuum`**：实测（sqlite 3.53.1）在
   `journal_mode=WAL` 之后设 `auto_vacuum` 会被**静默忽略**（`PRAGMA auto_vacuum` 读回 0）；
   而真实库里 `freelist_count` 恒为 0 —— 保留期删掉的页立刻被新消息复用，
   本来就没有空闲页可回收；即使设对了，一次 `PRAGMA incremental_vacuum` 也只回收 1 页。
   缩容手段是手动 `VACUUM`，或按 ADR 0001 第 11 条删库重建。
6. **第 15 条不变**：`messages.raw` 仍然保留 —— 附件表没了之后，
   它是唯一能事后重新提取的结构，价值反而更高。

- 测试基线 514 → **518**：删掉 5 条（2 条 `pending_attachment` 单测、2 条附件表单测、
  1 条与引用收录完全重复的 e2e），
  新增 9 条（`usable_url`、`quoted_attachments` 越过空 url、`send_raw` 保住 403、
  `Content-Length` 超限提前退出、`expires_in = 0` 回落默认寿命、屏障把丢批报成 `Err`、
  `meta` 有表无版本行报错、`raw` 仍然入库、同一条消息里的附件收录）。
