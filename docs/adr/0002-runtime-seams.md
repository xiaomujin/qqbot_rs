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
