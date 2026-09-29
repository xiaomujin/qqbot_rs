# QQ 机器人架构设计方案（Rust + 官方 API v2）

> 项目：`qqbot` — 基于 QQ 开放平台官方 API 的群聊 / 单聊机器人
> 语言：Rust　|　核心能力：收发消息、HTML 渲染截图、高性能
> 文档状态：设计定稿（P0 待开工）

---

## 0. 摘要

**语言选型**：Rust。核心动机是「渲染是长期瓶颈，而 Rust 拥有 `resvg` / `tiny-skia` / `usvg` 这一整套纯 Rust 光栅化栈」，可把渲染从 Chromium（200MB / 50–150ms）降到单二进制（5MB / 5–20ms），且无 Node、无 Chromium 依赖。

**架构结论**：**Actor 模型用，但只用三处、两种粒度**。

| # | Actor | 粒度 | 数量 | 解决什么问题 |
|---|---|---|---|---|
| 1 | `GatewayActor` | per shard | = 分片数（很小） | session_id / seq / 心跳 / Resume 状态机 |
| 2 | `SessionShard` | **per-key 分片** | = CPU 核数 × 2 | msg_seq 递增、被动窗口、主动配额、顺序保证 |
| 3 | `RenderActor` | per 浏览器实例 | = 实例数 | 渲染隔离、有界背压、崩溃监督重启 |

其余全部是 **`async fn` + 责任链 + 显式路由表**，不做 actor 化。

**一句话准则**：
> 有**独占的、跨消息保持的、必须串行访问**的状态 → Actor。
> 无状态的纯计算或 IO → `async fn`。

---

## 0.5 实施状态（2026-09-28）

本方案已落地为可运行代码，`cargo test --workspace` **209 项全绿**，`cargo clippy` 零警告。
**已在真实 QQ 群内完成端到端往返验证**（详见下方「线上实测」）。

| 阶段 | 内容 | 状态 |
|---|---|---|
| P0 | `qqbot-api` 协议层 + `qqbot-gateway` 网关 | ✅ 已对**真实网关**验证 |
| P1 | `SessionShard` 会话 actor + 发送层 + 富媒体分片上传 | ✅ 已完成 |
| P2 | `resvg` 渲染服务 + SVG 模板 + 词云布局 | ✅ 已完成 |
| P3 | 路由表 + 插件（帮助 / 骰子 / 词云） | ✅ 已完成 |
| P4 | 消息持久化（嵌入式 SQLite）+ 保留期清理 + 词云频次映射 | ✅ 已对**真实群聊**验证 |

### 真实环境验证结果

| 验证项 | 结果 |
|---|---|
| `access_token` 获取 | ✅ 返回长度 76，`expires_in` 为**字符串** `"7125"` |
| `/gateway/bot` | ✅ `wss://api.sgroup.qq.com/websocket`，`shards = 1` |
| op 10 Hello | ✅ `heartbeat_interval = 41250` ms |
| op 2 Identify → READY | ✅ 收到 `session_id` 与机器人昵称 |
| op 1 心跳 → op 11 ACK | ✅ 发出后约 42ms 收到 ACK（连续两次） |
| 断线退避重连 | ✅ 指数退避 + 抖动生效 |
| 完整二进制连续运行 | ✅ 75s / 180s 两轮，零错误、零 stderr |
| **线上真实往返（文本）** | ✅ 群内 @ 机器人发 `帮助` / `骰子 3d6`，机器人正确回复 |
| **线上真实往返（图片）** | ✅ 群内 @ 机器人发 `词云`，机器人回复**渲染出的词云图片**。日志三段完整：`渲染词云 words=6` → `富媒体上传完成 bytes=139887` → `消息已发送 mode="passive"` |
| **线上 Markdown 渲染** | ✅ 骰子回复的 `**12**` 在客户端正常加粗（修复前 `**11**` 是字面量） |
| **线上单聊（C2C）** | ✅ 真机被动回复成功（日志 `key="c2c:5CF47107..." mode="passive"`），补上了原先只有 wire 级验证的缺口 |
| **线上全量群消息** | ✅ 群主授权后，机器人收到 `GROUP_MESSAGE_CREATE`（**不含 @ 的普通消息**）：`收到消息 event="GROUP_MESSAGE_CREATE" content=群里的大家早上好呀` |
| **线上 Resume** | ✅ 服务端每小时下发 op 7，客户端连续 **5 次**成功 Resume（`seq` 1→6），无重复 Identify、无事件丢失 |
| **线上消息入库** | ✅ 6 条群消息入库（`共 7 行`）；重启机器人后**内存语料已清空**，词云仍渲染出 10 个词 —— 证明语料来自数据库 |
| **线上保留期清理** | ✅ 塞入一条 400 天前的消息后重启，日志 `保留期清理完成 deleted=1 cutoff=1759113235`，行数 9 → 8 |
| **线上词云频次映射** | ✅ `今天`(6 次) 最大最实，单次词最小最淡；字号按 `sqrt(词频)`、透明度同维度 |
| 常驻内存 | ✅ 空载约 15.7 MB；加载 jieba 词典后约 50 MB；再加载 SQLite 后约 50 MB |
| 离线构建 | ✅ 独立 target 目录冷启动 `cargo build --offline --release` 成功（2m23s），构建期不联网 |
| release 模式测试 | ✅ `cargo test --release --workspace` 全绿（不依赖 debug 断言与溢出检查） |

### 网关协议验证（`crates/qqbot-gateway/tests/gateway_protocol.rs`）

用本地 mock WebSocket 网关（`tokio_tungstenite::accept_async`）驱动真实的 `GatewayActor`，
覆盖 P0 里**光靠线上观察无法构造**的分支：

| 用例 | 断言内容 |
|---|---|
| Identify / READY / 心跳 | `shard=[0,1]`；**token 带 `QQBot ` 前缀**；`intents` 与配置一致；心跳 `d` = 最近收到的 `s` |
| **Resume** | 服务端在 READY 后强制断开 → 客户端重连必须发 **op 6 Resume**，携带上次 `session_id` 与最新 `seq`；且**不得重复 Identify** |
| **分片** | `shards=2` → 建立 2 条连接，分别携带 `[0,2]` 与 `[1,2]` |
| **op 9 InvalidSession** | 服务端回 op 9 → 客户端清空 session 并**重新 Identify**，全程**不出现 Resume** |

> Resume 是最容易被忽略、也最危险的路径：写错不会报错，只会在断线期间**静默丢事件**。
> 这条测试是本次补充中价值最高的一个。

### 真机验收（2026-09-29，群聊）

在真实 QQ 群内完成全链路验收，证据见 [`docs/samples/live-wordcloud-reply.png`](docs/samples/live-wordcloud-reply.png)
与 [`docs/samples/live-run.log`](docs/samples/live-run.log)。

| 步骤 | 结果 |
|---|---|
| `@黑猫Bot 帮助` | ✅ 返回 Markdown 指令表（列表正常渲染） |
| `@黑猫Bot 骰子 3d6` | ✅ 返回 `3d6 = **12**`，**加粗在客户端生效** |
| `@黑猫Bot 词云`（语料为空） | ✅ 返回可操作的引导文案（见下） |
| `@黑猫Bot 今天天气真不错` | ✅ 语料被记录 |
| `@黑猫Bot 词云`（有语料） | ✅ **返回渲染出的词云图片** |

图片链路的服务端日志（三段齐全）：

```
01:30:47  INFO 渲染词云 words=6
01:30:50  INFO 富媒体上传完成 scene="group" bytes=139887
01:30:53  INFO 消息已发送 key="group:617E53BC..." mode="passive" msg_seq=Some(1)
```

**验收中发现并修复的最后一个问题**：公开机器人只订阅 `GROUP_AND_C2C_EVENT`，
**只能收到 @ 它的消息**，看不到群里的日常聊天（那需要 `GROUP_MESSAGE_CREATE` 全量群消息，仅私域机器人可用）。
因此词云的「多聊几句就有语料」这个假设是错的。原先的提示语「至少需要 3 个高频词」会误导用户；
现改为明确说明「必须 @ 我说自然语言」。

### 发送链路端到端验证（`tests/end_to_end.rs`）

由于「不主动发消息」是既定边界，且观察窗口内没有真人 @ 机器人，
发送链路改用**本地 mock HTTP 服务器**做完整验证：真实跑 `ApiClient` + `SessionRegistry`
+ `Dispatcher` + 插件 + `RenderService`，只把网络出口指向 127.0.0.1，然后断言
**mock 实际收到的 HTTP 请求**。这覆盖了序列化、鉴权头、分片规划、`msg_seq` 分配等真实细节。

| 用例 | 断言内容 |
|---|---|
| 文本回复 | `Authorization: QQBot MOCK_TOKEN`；`msg_type=0`；携带 `msg_id`；`msg_seq=1` |
| 幂等 + 递增 | 相同 `msg_id` 的事件被去重（只发 1 条）；换 `msg_id` 后 `msg_seq` 严格递增 `[1,2]` |
| 单聊路由 | 发往 `/v2/users/{openid}/messages`，而非群聊端点 |
| **图片全链路** | 收到「词云」→ 渲染 PNG → `upload_prepare`（校验 `md5`=32 位、`sha1`=40 位、含 `md5_10m`）→ **PUT 预签名 URL 且不带 Authorization** → `upload_part_finish` → 合并拿 `file_info` → 以 `msg_type=7` 发送 |
| 秒传 | 相同图片二次上传不再触发 `upload_prepare`，直接复用 `file_info` |
| 帮助 | `msg_type=2`，内容自动包含已注册指令 |
| 未知事件 | 不 panic、不产生任何 HTTP 调用 |
| **被动过期降级** | mock 首次返回 `err_code 40034005` → 客户端自动改走主动消息重试一次（重试体**不带** `msg_id`/`msg_seq`） |
| **配额前置拦截** | 连续 25 次主动发送：前 20 次放行、后 5 次本地拒绝，且**被拒的请求不产生任何 HTTP 调用** |

### 实测性能（release，Windows，12 核）

| 项目 | 耗时 |
|---|---|
| 渲染服务初始化（加载 386 个系统字体） | 22 ms |
| 卡片图 1440×680（模板 + 中文） | 35 ms |
| 词云 1800×1280（算法布局 + 中文） | 59 ms |
| 缓存命中 | **62 µs** |

> 对照：Chromium 方案同等图片约 50–150 ms，常驻内存约 200 MB；
> 本方案渲染服务常驻内存约几 MB，且**无 Chromium、无 Node 依赖**。

### 与原方案的偏差

| 原方案 | 实际实现 | 原因 |
|---|---|---|
| `RenderActor` per 浏览器实例 | `RenderService`：有界队列 + `Semaphore` 并发上限 + `spawn_blocking` | resvg 是无状态纯 CPU 计算，不需要「每实例独占状态」；语义等价（背压 + 隔离 + 超时） |
| `qqbot-api` 零 IO | `types/event/message` 零 IO；HTTP 客户端集中在 `client` 子模块 | 避免为传输层单开一个 crate，同时保住「协议类型可纯单测」这一核心收益 |

### 实测发现的九个关键 Bug（已在代码中修复并加注释）

1. **Identify 的 `token` 必须带 `QQBot ` 前缀**。漏掉时服务端不报错，而是回 op 9 InvalidSession，
   表现为「连上了、Hello 收到了、但永远等不到 READY，然后无限重连」。
2. **SVG `<text y>` 是基线而非盒子顶边**。词云布局用盒子顶边当基线输出，会让文字整体下沉一个字高，
   造成肉眼可见的重叠。已补几何测试（`placed_boxes_do_not_overlap`）防止回归。
3. **词云输出不确定**。词表来自 `HashMap` 迭代，只按权重排序时同权重词的顺序每次运行都不同
   （Rust 默认哈希器带随机种子）→ SVG 每次都变 → **渲染缓存与秒传缓存永不命中**。
   修复为 `(weight desc, text asc)` 的确定性排序，并补测试
   （`layout_is_deterministic_regardless_of_input_order`）。
   这个 Bug 是端到端测试「秒传断言」逼出来的——单测看不出来。
4. **主动消息配额每个窗口多放行 1 条**。`Quota::new` 用 `Instant::now()` 初始化窗口起点，
   而调用方的 `now` 更早（先取时间、后建状态），于是第一次不触发窗口滚动、
   第二次 `now >= minute_reset` 才触发并把计数清零——凭空多出一条额度。
   修复为把 `now` 显式注入 `Quota::new` / `SessionState::new`，让窗口起点完全由调用方决定。
   同样是端到端测试（25 次发送断言 20 放行）逼出来的。
5. **骰子用纯文本发送，`**加粗**` 在客户端原样显示**。`reply_text` 走 `msg_type=0`，
   QQ 不会对其做 Markdown 渲染；用户实测反馈「md 没有被渲染」。
   修复为 `reply_markdown`（`msg_type=2`），并补了两个用例分别锁定文本路径与 Markdown 路径。
   附带修掉：词云的通配监听器（`Matcher::Any`）会以 `*` 出现在帮助列表里，
   现通过 `Rule.hidden` 标记为「监听器」，不参与帮助生成。
6. **`file_info` 的 `ttl=0` 被误当成「立即过期」**。官方文档明确 `ttl` 为 0 表示
   **可长期使用**，但代码里 `Duration::from_secs(0)` 会让缓存立刻失效——
   恰恰在最该复用的场景（服务端说长期有效）把秒传关掉了。修复为 `resolve_ttl`：
   `0` 与「未下发」都回退到本地上限，其余取 `min(服务端 ttl, 本地上限)`。

### 另外补上的防御性细节

**分片合并请求显式传 `srv_send_msg: false`**。官方文档把该字段标为「可选」但未说明默认值；
若不传而默认恰为 `true`，就会变成「上传即发送」并**占用主动消息频次**。已显式声明并加断言锁定。

**事件类被动回复的凭据不再丢失**。官方规定「被动消息（响应事件）携带 `event_id`」，
凭据是 payload **外层** 的 `id`（不在 `d` 里）。原先解析时这个字段被直接丢弃，
导致 `SendRequest::responding_to_event` 这条路径**根本无从发起**。
现在 `Event::parse` 接收并透传 `event_id`，`RawNotice` 携带它，
并补了三个用例：保留外层 id、缺失时不报错、以及事件回复的请求体形状
（带 `event_id`、**不带** `msg_id`/`msg_seq`）。

> 取舍说明：事件类回复当前仍会消耗主动消息配额（保守策略）。官方把它归为「被动消息」，
> 但未给出对应的时间窗口规则；宁可少发，也不冒超配额被服务端拒绝的风险。

**错误码 → 可操作建议**。`ApiError::hint()` 把官方错误码翻译成排查动作，并透传到
`MediaError` / `CoreError`。真机联调时日志里会直接出现：

```
WARN 消息发送失败 error=业务错误 11253: ... hint="机器人未获得该接口权限 —— 请到 QQ 开放平台管理端申请「富媒体/上传」等对应权限"
```

而不是只丢一个裸错误码。

7. **词云分词用的是「二元组滑窗」，产出的全是伪词**。原先把 CJK 连续串切成相邻二字组，
   「今天天气真不错」→ 今天 / **天天** / 天气 / **气真** / **真不** / 不错——
   加粗的三个都不是词，词云看起来就是乱码（用户实测截图反馈）。
   现改用 **jieba 精确模式**（真实词典 + HMM），得到 今天天气 / 真不错；
   「群里的大家早上好呀」→ 群里 / 大家 / 早上好。
   *注意*：不能用 `cut_for_search`（搜索引擎模式）——它会把长词再切出子词，
   反而重新引入「天天」「真不」这类垃圾。
   代价：二进制 10.4 MB → 13.2 MB（内置词典）。

8. **线上某些事件的 `timestamp` 是 Unix 整数，不是文档写的 RFC3339 字符串**。
   严格按 `String` 解析会直接导致**网关断连**：
   `JSON 解析错误: invalid type: integer `1790645533`, expected a string`。
   修复：`de_lenient_opt_string` 同时接受字符串与数字。

9. **单个事件解析失败会拖垮整条长连接**。这是比第 8 条更根本的问题——
   一个字段的类型差异就导致断线重连、期间事件全部丢失。
   现在解析失败只**跳过该事件**并计数（`qqbot_gateway_event_parse_error_total`），
   连接保持。补了回归测试：mock 先推一个坏事件再推一个好事件，好事件必须仍能送达。

### 可观测性：为什么补了「收到消息」日志

排障时最怕「不知道事件到底有没有送达」。原先收到消息只打 DEBUG，
线上跑 INFO 时完全看不见，导致排查「词云没反应」时只能靠猜。
现在收到消息、非消息事件都会打 INFO：

```
INFO 收到消息 event="GROUP_MESSAGE_CREATE" target=group:617E53BC... content=群里的大家早上好呀
INFO 渲染词云 words=3
INFO 富媒体上传完成 scene="group" bytes=125244
INFO 消息已发送 key="group:617E53BC..." mode="passive" msg_seq=Some(1)
```

消息内容按 60 字符截断，避免把整篇消息灌进日志。

### 内存有界性（长跑不涨内存）

长生命周期服务最容易忽略的是「只增不减的容器」。当前所有状态都有明确上限：

| 状态 | 上限机制 |
|---|---|
| `SessionShard.states` | 每 5 分钟扫描一次，回收空闲 > 2 小时的会话（TTL 刻意大于单聊 60 分钟被动窗口，避免误删在用窗口） |
| `WordCloudPlugin` 内存语料 | 最多跟踪 2048 个会话（FIFO 淘汰），单会话最多 500 条消息（仅未启用存储时使用） |
| 存储写队列 | 有界 4096；满则丢弃并计数，**绝不阻塞热路径** |
| 词云查询 | 单次最多 20000 行 / 800 万字符，两道闸门防超大群吃满内存 |
| 单条消息正文 | 入库前截断到 2000 字符，保证单行体积有界 |
| 数据库文件 | 保留期清理 + `incremental_vacuum` 归还空间，不会只增不减 |
| 事件去重 `moka` 缓存 | `max_capacity` + `time_to_live` |
| 渲染结果缓存 | `max_capacity` |
| `file_info` 缓存 | `max_capacity` + 服务端 ttl 与本地上限取较小值 |

实测：机器人连续在线，常驻内存稳定在 **15.6 MB**。

> 回收会话状态会重置该会话的「当日主动消息计数」。这是有意的取舍——
> 服务端仍会做最终限流，且「空闲 2 小时后恰好再发满 1000 条」的场景可以忽略。

### 另外补上的协议健壮性

**被动窗口被服务端判定过期时自动降级为主动消息**。官方错误码 `40034005`（回复消息 msg_id 已过期）
说明本地时钟与服务端可能存在偏差；原先直接返回错误丢消息，现在会清空本地窗口、
消耗一次主动配额后重试一次（`active_retry` 指标单独计数）。

### 接口覆盖补齐（2026-09-29）

`qqbot-api` 原先只覆盖「消息收发 + 富媒体上传」两条链路。本轮把官方文档
**服务端接口**里除**频道（Guild / Channel）**以外的部分全部补齐：
新增 **29 个端点**、**2 个纯类型模块**（[`bot.rs`](crates/qqbot-api/src/bot.rs) /
[`group.rs`](crates/qqbot-api/src/group.rs)），测试从 149 项增至 **209 项**。

| 模块 | 覆盖范围 | 新增端点 |
|---|---|---|
| `message.rs` | 流式单聊消息、互动事件回调、引用回复、完整键盘 | 2 |
| `bot.rs` | 机器人详情、分享链接、全局自定义菜单、指令面板 CRUD + 关联对象 | 10 |
| `group.rs` | 群信息 / 机器人状态 / 入群申请 / 审批 / 禁言 / 成员 / 黑名单 / 入群自动审批策略 | 17 |

**有意不实现**：频道（Guild / Channel / 身份组 / 论坛 / 音频 / 小程序）相关的全部接口，
以及 `GET /users/@me/guilds`（获取机器人频道列表）—— 那是另一套产品面，与群机器人无关。
端点全清单见 [`docs/qq-bot-api-v2-protocol.md`](docs/qq-bot-api-v2-protocol.md) 第 11 节，
逐接口的约束要点见第 12 节。

**顺带修掉的协议细节**（都写进了注释）：

- `Keyboard` 之前只有 `content`，**短形式（只传平台模板 `id`）根本表达不出来**；
  现在 `id` 与 `content` 二选一并在文档里标明互斥。
- `ButtonAction::permission` 原先是个裸 `serde_json::Value`，现在是有类型的 `Permission`，
  并补上 `enter` / `reply` / `anchor` / `modal` / `click_limit` 等此前缺失的字段。
- `OutMessage` 补上 `message_reference`（引用回复）；`SendResult` 补上 `ext_info` ——
  它的 `ref_idx` 正是**引用机器人自己发的消息**时要填的值。
- 「响应为空」的接口（撤回 / 互动回调 / 审批 / 删除策略 …）**不能用 `()` 接**：
  官方示例给的是 `{}`，而 serde 的 `()` 只吃 `null`。统一用 `EmptyResponse`
  （`#[serde(from = "Option<Value>")]`，`null` 与 `{}` 都收得住）。
- 分页查询串改由 **serde 驱动**（reqwest 的 `query` 特性），不再手写百分号编码 ——
  手写版本每加一个查询字段都要同步改一处，迟早漂移。

### rust-skills 审计与修复（2026-09-29）

用 [rust-skills](https://github.com/leonardomso/rust-skills)（265 条规则 / 26 类别）对整个 workspace 做了一轮逐 crate 审计，
完整报告见 [`docs/rust-skills-audit.md`](docs/rust-skills-audit.md)。共 7 项必修（3 HIGH / 4 MEDIUM）+ 8 项 P1 + 约 30 项 P2，
**已全部修复或给出不修的理由**（见报告 §0.5）。

其中三项 HIGH 是**架构级**的，值得记在这里：

1. **事件分发的并发度曾经恒为 1**。`main.rs` 原先在 `select!` 分支里直接
   `dispatcher.handle(event).await`，主循环必须等它返回才去 poll `gateway.recv()`，
   于是 `DispatchConfig::concurrency: 16` 建的 `Semaphore` 永远只能拿到 1 个许可 ——
   一条词云命令会阻塞**所有**消息处理，还会把网关事件挤到有界通道满而被丢弃。
   现在改为 `JoinSet::spawn`，主循环只负责收事件，退出时给 10s 排空窗口。
2. **网关退避曾被 `Send` 命令打穿**。退避原先写成 `select! { sleep(delay), inbox.recv() }`，
   任何一条 `Send` 都会让 select 提前结束、把剩余的退避时间取消掉，指数退避变成热重连。
   现在用**绝对 deadline** 循环，只有 `Shutdown`/`Reconnect` 能打破退避。
3. **token 刷新的余量设置反了**。余量固定 300s，而服务端在 `expires_in <= 300` 时
   （线上日志见过 271 / 214）会让「剩余寿命 > 余量」恒为假 —— 于是**每次 API 调用都重新
   刷新一遍**，拿回来的还是同一个 token，且全程持独占锁。现在拆成 `RwLock` 读缓存 +
   `Mutex` 只串行化刷新，余量改为 `min(300s, 寿命/2)`。

另外把 lint 策略固化成了 `[workspace.lints]`：`unsafe_code = "deny"` +
`clippy::correctness = "deny"`，全部 crate 继承。项目至今**零 `unsafe`**，这条保证它不会悄悄退化。

---

## 1. 需求与约束

### 1.1 功能需求

| 需求 | 说明 |
|---|---|
| 接收消息 | 群聊（@机器人 / 全量）、单聊 |
| 发送消息 | 文本、Markdown、按钮、富媒体（图片/视频/语音/文件） |
| 图片渲染 | HTML 模板 → 截图 → 发送；需高性能 |
| 插件体系 | 命令路由、权限、限流、可扩展（参考 Shiro 的责任链模型） |
| 主动推送 | 订阅类推送（B站动态、定时任务） |
| 运维 | 可观测、可水平扩容、断线自恢复 |

### 1.2 官方 API 硬约束（**已核对官方文档**）

| 约束 | 值 | 影响 |
|---|---|---|
| access_token 有效期 | 7200s，**过期前 60s 内请求会返回新 token** | TokenProvider 必须提前刷新 + singleflight |
| 错误返回形式 | **HTTP 200 + `err_code`** | ⚠️ 不能只看 HTTP 状态码判成败 |
| 限频 | HTTP 429 | 指数退避 + 本地配额前置拦截 |
| 被动回复窗口 | 单聊 **60min / 4 次**；群聊 **5min / 5 次**；频道 5min | 会话级状态，必须精确实现 |
| msg_seq | 相同 `msg_id + msg_seq` 重复发送会失败，**需递增** | 会话级计数器 |
| 事件重推 | 相同 `msg_id` 可能多次推送 | 幂等去重 |
| 主动消息频控 | 单聊 10qps / 20qpm；群 60qpm；单关系 20qpm；每日 1000 条/用户(群) | TokenBucket + 每日账本 |
| 互动召回 | `is_wakeup=true`，4 个周期各 1 条 | 可选能力 |
| 撤回时限 | 发送后 **2 分钟内** | 延迟任务 |
| file_info | **有 TTL**，过期需重传；单聊/群聊**接口隔离** | 缓存 key 必须带场景 |
| 秒传 | `md5_10m` = 文件前 10002432 字节的 MD5 | 上传前查缓存 |
| 分片上传 | 默认 5MB；4 步：prepare → PUT 预签名 → part_finish → 合并 | 超时建议 ≥ 5s |

> 完整协议速查见 [`docs/qq-bot-api-v2-protocol.md`](docs/qq-bot-api-v2-protocol.md)。

---

## 2. 技术选型

### 2.1 为什么是 Rust

| 维度 | 说明 |
|---|---|
| **渲染栈** | `resvg` / `usvg` / `tiny-skia` / `fontdb` 均为纯 Rust，可做到**零 Chromium、零 Node、单静态二进制** |
| 内存 | 渲染器常驻 ~5MB（对比 Chromium ~200MB） |
| 冷启动 | < 1ms（对比 Chromium 300–800ms） |
| 并发安全 | 编译期杜绝数据竞争，适合长生命周期无人值守服务 |
| 部署 | 单一静态二进制 |

**代价**：无官方 QQ SDK，协议层需自研（约 2000–3000 行）；迭代速度慢于 Go（编译时间）。

### 2.2 crate 选型

| 领域 | 选型 | 备注 |
|---|---|---|
| 异步运行时 | `tokio` (full) | |
| WebSocket | `tokio-tungstenite` / `fastwebsockets` | 后者更快 |
| HTTP 客户端 | `reqwest` (hyper 1.x) | 连接池复用 |
| JSON | `serde` + `serde_json` + `RawValue` | **两阶段解析是性能关键** |
| 错误 | `thiserror`（库）/ `anyhow`（应用） | |
| 日志追踪 | `tracing` + `tracing-subscriber` | 取代 Java AOP |
| 指标 | `metrics` + `metrics-exporter-prometheus` | |
| Redis | `fred` | 性能优于 redis-rs |
| 数据库 | `sqlx` | 编译期 SQL 校验 |
| 本地缓存 | `moka` | 渲染结果 / file_info |
| HTML 模板 | `minijinja`（复用现有模板）/ `askama`（编译期最快） | |
| Chromium 驱动 | `chromiumoxide` | 渲染慢路 |
| SVG → PNG | `resvg` + `usvg` + `tiny-skia` | **渲染快路** |
| 字体 | `fontdb` + `ttf-parser` | 中文需子集化 |
| 命令路由 | `matchit` 或自写 trie | |
| 分配器 | `mimalloc` / `tikv-jemallocator` | 高并发下明显优于系统分配器 |
| Actor | **手写**（tokio + mpsc + oneshot）；备选 `ractor` | **禁用 `actix`** |

---

## 3. 总体架构

```
                    QQ 开放平台
        (api.bot.qq.com  +  WSS Gateway)
                     │
     ┌───────────────┴────────────────┐
     │  ① GatewayActor (per shard)     │
     │  Identify / Heartbeat / Resume  │
     │  session_id · seq · 重连退避     │
     └───────────────┬────────────────┘
                     │ Arc<Event>  (try_send，绝不阻塞 WS)
     ┌───────────────▼────────────────┐
     │  Dispatcher：中间件管道          │
     │  去重 → 限流 → 路由 → 权限       │
     └───┬───────────────────┬────────┘
         │                   │
 ┌───────▼────────┐  ┌───────▼──────────────┐
 │ ② SessionShard │  │ ③ RenderActor 池      │
 │  (固定 N 个)    │  │  resvg 快路 /         │
 │  msg_seq        │  │  chromium 慢路        │
 │  被动窗口        │  │  有界 mailbox = 背压   │
 │  主动配额        │  └───────┬──────────────┘
 └───────┬────────┘          │ PNG
         │                   │
         │            ┌──────▼───────┐
         │            │  Media 子系统 │
         │            │  分片上传/秒传 │
         │            │  file_info缓存│
         │            └──────┬───────┘
         └──────────┬────────┘
                    ▼
             发送 OpenAPI (msg_type 0/2/7)
```

**分层原则**：

1. `qqbot-api` 的 `types` / `event` / `message` / `payload` 是**纯类型 + 纯函数，零 IO** —— 编译快、可单测；
   HTTP 传输集中在 `client` 子模块，gateway 与 sender 共用。
2. 渲染、媒体、存储都是**旁路**，崩溃不影响长连接。
3. 所有对外 IO 必须有**超时 + 降级**。

---

## 4. Workspace 结构

```
qqbot/
├─ Cargo.toml                      # workspace
├─ crates/
│  ├─ qqbot-api/                   # 协议类型（无 IO）+ HTTP 客户端
│  │   ├─ payload.rs               #   Payload { id, op, d, s, t }
│  │   ├─ opcode.rs                #   0/1/2/6/7/9/10/11/12/13
│  │   ├─ intents.rs               #   bitflags
│  │   ├─ event/                   #   每个事件一个 struct
│  │   ├─ message.rs               #   OutMessage / MsgType / Target / 键盘 / 流式消息 / 互动回调
│  │   ├─ bot.rs                   #   机器人详情 / 分享链接 / 自定义菜单 / 指令面板
│  │   ├─ group.rs                 #   群信息 / 成员 / 禁言 / 入群审批 / 黑名单
│  │   ├─ client.rs                #   ★ 唯一带 IO 的模块（HTTP + access_token）
│  │   └─ error.rs                 #   err_code 枚举 + 可重试判定
│  │
│  ├─ qqbot-gateway/               # ★ Actor #1
│  │   ├─ actor.rs                 #   per-shard 连接 actor
│  │   ├─ session.rs               #   session_id / seq / resume 状态机
│  │   ├─ heartbeat.rs
│  │   └─ shard.rs                 #   /gateway/bot 获取建议分片数
│  │
│  ├─ qqbot-core/                  # ★ Actor #2
│  │   ├─ session_actor.rs         #   sharded session actor
│  │   ├─ dispatch.rs              #   中间件管道
│  │   ├─ router.rs                #   命令路由表
│  │   ├─ plugin.rs                #   Plugin trait + Handled
│  │   └─ sender.rs                #   发送层
│  │
│  ├─ qqbot-media/                 # 富媒体
│  │   ├─ uploader.rs              #   分片上传 4 步 + md5_10m 秒传
│  │   └─ error.rs
│  │
│  ├─ qqbot-render/                # ★ Actor #3
│  │   ├─ service.rs               #   有界队列 + Semaphore + spawn_blocking
│  │   ├─ svg.rs                   #   快路 (resvg，无 Chromium)
│  │   ├─ template.rs              #   minijinja 模板
│  │   └─ wordcloud.rs             #   螺旋布局 + 词频→字号/透明度
│  │
│  ├─ qqbot-store/                 # 消息持久化（嵌入式 SQLite）
│  │   ├─ schema.rs                #   建表 / PRAGMA / 版本迁移
│  │   ├─ writer.rs                #   写线程：有界队列 + 批量事务
│  │   ├─ reader.rs                #   读线程：独立连接，WAL 读不阻塞写
│  │   └─ model.rs                 #   NewMessage / Scope
│  │
│  └─ qqbot-plugins/               # 业务插件（帮助 / 骰子 / 词云）
│
└─ src/main.rs                     # run / check / self-test
```

---

## 5. Actor 模型设计

### 5.1 判定准则

> **有独占的、跨消息保持的、必须串行访问的状态 → Actor。**
> **无状态的纯计算或 IO → `async fn`。**

| 场景 | 用 Actor？ | 理由 |
|---|---|---|
| 每 shard 一条 WS 连接 | ✅ 强烈推荐 | 独占状态，消息必须串行 |
| 会话状态（msg_seq / 窗口 / 配额） | ✅ 强烈推荐 | 协议强制的按键串行状态机 |
| 渲染工作池 | ✅ 推荐 | 有界 mailbox = 背压；崩溃可监督重启 |
| 插件链 / 命令路由 | ❌ 不要 | 无状态纯计算 |
| 数据查询 / HTTP 调用 | ❌ 不要 | 直接 `async fn` |
| 全系统 actor 化 | ❌ 不要 | 会退化成 `Box<dyn Any>` + 巨型 enum |

**为什么会话状态特别适合 actor**：官方文档原文规定「相同 msg_id + msg_seq 重复发送会失败，可递增 msg_seq 实现对同一消息的多次回复」。这是一个**每会话递增计数器 + 窗口倒计时 + 配额桶**的经典状态机。用 `Mutex<HashMap<_, State>>` 会带来锁竞争、死锁风险、锁粒度难调；用 actor 按 key 路由则零锁、天然顺序、状态局部性好。

### 5.2 ① GatewayActor（per shard）

**状态**：`session_id`、`last_seq`、`heartbeat_interval`、连接句柄、重连退避。

```rust
// qqbot-gateway/src/actor.rs
pub enum GatewayCmd { Send(Box<RawValue>), Reconnect, Shutdown }

pub struct GatewayActor {
    shard: [u32; 2],                        // [i, n]
    api: Arc<ApiClient>,
    events: mpsc::Sender<Arc<Event>>,       // 交给 Dispatcher
    inbox: mpsc::Receiver<GatewayCmd>,
    // ---- 独占状态，只有本 actor 能碰 ----
    session: Option<Session>,               // { session_id, last_seq }
    backoff: ExponentialBackoff,
}

impl GatewayActor {
    pub async fn run(mut self) {
        loop {
            match self.connect_and_pump().await {
                Ok(Flow::Resumed)   => self.backoff.reset(),
                Ok(Flow::Reconnect) => { /* op 7 */ }
                Ok(Flow::Invalid)   => { self.session = None; /* op 9 */ }
                Err(_)              => self.backoff.sleep().await,
            }
        }
    }

    async fn connect_and_pump(&mut self) -> Result<Flow> {
        let url = self.api.gateway_url().await?;
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await?;

        // op 10 Hello → 决定 Identify 还是 Resume
        let hello: Payload<Hello> = next_json(&mut ws).await?;
        if let Some(s) = &self.session {
            send_op6(&mut ws, &self.api.token(), &s.session_id, s.last_seq).await?;
        } else {
            send_op2(&mut ws, &self.api.token(), INTENTS, self.shard).await?;
        }

        let mut hb = self.spawn_heartbeat(hello.heartbeat_interval);

        while let Some(frame) = ws.next().await {
            let raw: Payload<Box<RawValue>> = serde_json::from_slice(&frame?.into_data())?;

            match raw.op {
                0 => {
                    self.session.as_mut().unwrap().last_seq = raw.s;
                    hb.seq.store(raw.s, Ordering::Relaxed);

                    // ★ 两阶段解析：先拿 t，再按 t 精确反序列化，避免通用 Map
                    if let Some(ev) = Event::parse(raw.t.as_deref(), raw.d)? {
                        // ★ 关键：绝不阻塞 WS。背压时降级，不能让心跳饿死
                        if self.events.try_send(Arc::new(ev)).is_err() {
                            metrics::counter!("gateway.events.dropped").increment(1);
                        }
                    }
                }
                11 => { /* heartbeat ack */ }
                7  => return Ok(Flow::Reconnect),
                9  => return Ok(Flow::Invalid),
                _  => {}
            }
        }
        Ok(Flow::Reconnect)
    }
}
```

**要点**：

- `session_id` / `last_seq` / 心跳定时器必须在**同一执行体**更新；拆成多 task + `Mutex` 会导致 Resume 补发错位（最隐蔽的 bug）。
- 事件转发**必须用 `try_send`**，不能用会阻塞的 `send().await` —— 心跳断开会掉线。

### 5.3 ② SessionShard（按 key 分片，**不是一群一 actor**）

⚠️ **最容易做错的一点**：不要每个群一个 actor。群数量上规模后是几万个 actor，调度开销超过收益。

**正确做法**：固定 N 个 shard actor（N = CPU 核数 × 2），按 `group_openid` / `user_openid` 哈希路由，每个 actor 内部持有 `HashMap<OpenId, SessionState>`。

```rust
// qqbot-core/src/session_actor.rs
pub struct SessionState {
    msg_seq: u32,                          // 被动回复序号，递增
    passive: Option<PassiveWindow>,        // { msg_id, deadline, remaining }
    active: TokenBucket,                   // 主动消息配额
}

pub enum SessionMsg {
    Reply {
        key: OpenId,
        body: Body,
        passive: Option<MsgId>,            // 有 msg_id = 被动回复
        ack: oneshot::Sender<Result<MsgId>>,
    },
    Evict(OpenId),
}

pub struct SessionShard {
    inbox: mpsc::Receiver<SessionMsg>,
    state: HashMap<OpenId, SessionState>,  // ★ 状态局部性
    lru: LruCache<OpenId, ()>,
    http: Arc<ApiClient>,
}

impl SessionShard {
    async fn run(mut self) {
        while let Some(msg) = self.inbox.recv().await {
            match msg {
                SessionMsg::Reply { key, body, passive, ack } => {
                    let st = self.state.entry(key.clone()).or_default();

                    // ★ 被动/主动判定 —— 协议硬约束集中在一处
                    let (msg_id, msg_seq, is_wakeup) = match passive {
                        Some(id) if st.passive.as_ref().is_some_and(|w| w.valid()) => {
                            st.msg_seq += 1;                       // 递增，否则重复发送会失败
                            (Some(id), Some(st.msg_seq), false)
                        }
                        _ => {
                            // 被动窗口过期 → 只能走主动消息
                            if !st.active.try_consume() {
                                let _ = ack.send(Err(Error::QuotaExhausted));
                                continue;
                            }
                            (None, None, false)
                        }
                    };
                    let r = self.http.send(&key, body, msg_id, msg_seq, is_wakeup).await;
                    let _ = ack.send(r);
                }
                SessionMsg::Evict(k) => { self.state.remove(&k); }
            }
        }
    }
}

// 路由表：一致性哈希到固定 N 个 shard
pub struct SessionRegistry {
    shards: Vec<mpsc::Sender<SessionMsg>>,   // N = CPU 核数 × 2
    hasher: RandomState,
}
impl SessionRegistry {
    pub fn route(&self, key: &OpenId) -> &mpsc::Sender<SessionMsg> {
        &self.shards[self.hasher.hash_one(key) as usize % self.shards.len()]
    }
}
```

**一次性解决的问题**：

1. `msg_seq` 递增 —— 零锁，不会重复
2. 被动窗口 —— 单聊 60min/4 次、群聊 5min/5 次，状态在本地
3. 主动配额 —— TokenBucket + 每日账本在本地
4. 同一会话回复顺序天然保证（mailbox FIFO）
5. 无需任何 `Mutex`，**不可能死锁**

> ⚠️ 单聊与群聊的被动窗口规则**不同**，`SessionState` 必须按 target 类型分支。这是高频错误点。

### 5.4 ③ RenderActor（per 浏览器实例）

```rust
// qqbot-render/src/actor.rs
pub struct RenderJob {
    pub html: String,
    pub viewport: (u32, u32),
    pub scale: f32,
    pub ack: oneshot::Sender<Result<Bytes>>,
}

pub struct RenderActor {
    browser: Browser,                        // chromiumoxide
    inbox: mpsc::Receiver<RenderJob>,        // ★ 有界 = 背压
}

impl RenderActor {
    async fn run(mut self) {
        while let Some(job) = self.inbox.recv().await {
            let page = self.browser.new_page(&job.html).await;
            match tokio::time::timeout(Duration::from_millis(3000), render(page, &job)).await {
                Ok(Ok(png)) => { let _ = job.ack.send(Ok(png)); }
                _ => {
                    let _ = job.ack.send(Err(Error::RenderTimeout));
                    self.restart_if_unhealthy().await;   // ★ 监督重启
                }
            }
        }
    }
}
```

**对比旧实现**（cq-bot 的 `PuppeteerUtil`：单 Browser + 渲染 500 次重启）：Actor 版本的优势是**有界 mailbox 自动背压 + 单次超时隔离 + 健康检查重启**，且 `oneshot` 让调用方可以 `select!` 到自己的超时上，直接降级到纯文本。

### 5.5 不该用 Actor 的部分

**插件链与命令路由** —— 用责任链 + 显式路由表，而非过程宏魔法：

```rust
// qqbot-core/src/router.rs
pub struct Router { rules: Vec<Rule> }      // 按 priority 排序

pub struct Rule {
    scope: Scope,                           // Group | C2c | Any
    matcher: Matcher,                       // Exact | Prefix | Regex | Trie
    priority: i32,
    handler: Arc<dyn Handler>,
}

#[async_trait]
pub trait Handler: Send + Sync + 'static {
    async fn call(&self, ctx: &Ctx) -> Handled;
}

#[derive(Clone, Copy, PartialEq)]
pub enum Handled { Consumed, Next }          // 对应 Shiro 的 MESSAGE_BLOCK / MESSAGE_IGNORE
```

注册（编译期，零运行期扫描）：

```rust
// qqbot-plugins/src/lib.rs
pub fn register(r: &mut Router) {
    r.on_group(Trie::new("签到"), SignInPlugin);
    r.on_group(Prefix::new("塔科夫 "), TarkovPlugin);
    r.on_any(Regex::new(r"^1[3-9]\d{9}$")?, PhonePlugin);
    r.on_group(Trie::new("词云"), WordCloudPlugin);   // → 走 RenderActor
    r.on_group(Trie::new("帮助"), HelpPlugin);        // 可自动生成 /help
}
```

**为什么不用过程宏**：

1. Rust 过程宏的编译错误定位差，对 AI 迭代极不友好
2. 路由表是**显式数据**，可打印、可单测、可自动生成帮助文档
3. 编译期 `register()` 已达成 Shiro 注解「无需写 plugin-list」的效果

### 5.6 Actor 框架选型

| 方案 | 结论 |
|---|---|
| **手写**（tokio + mpsc + oneshot + JoinSet） | ✅ **推荐**。约 200 行，零魔法，性能最好，完全可控 |
| `ractor` | 可选。Erlang 风格 supervision tree / `ActorRef`，基于 tokio，不绑定 Web 框架。适合 RenderActor 池与连接管理 |
| `actix` | ❌ **禁用**。actor 框架与 actix-web 强耦合，在 tokio 主导的生态里已边缘化 |

---

## 6. 发送层与协议约束集中点

```rust
pub enum Target {
    C2c    { user_openid: String },     // 被动窗口 60min / 4 次
    Group  { group_openid: String },    // 被动窗口  5min / 5 次
    Channel{ channel_id: String },      // 被动窗口  5min
}

pub struct OutMessage {
    pub msg_type: MsgType,              // 0 文本 / 2 Markdown / 7 富媒体
    pub body: Body,
    pub msg_id: Option<String>,         // 被动回复凭证
    pub msg_seq: Option<u32>,           // ★ 必须递增
    pub is_wakeup: bool,                // 互动召回
}
```

**发送路径**：业务 → `SessionRegistry::route(key)` → `SessionShard`（分配 msg_seq / 校验配额）→ `ApiClient`。

**错误处理原则**：

- 判成败**只依据 `err_code`**，不依据 HTTP 状态码，也不依据 `message` 文本
- 记录 `trace_id` / `X-Tps-trace-ID` 便于向平台求助
- 429 / 5xx 走指数退避；`11281` / `11252` 类系统错误最多重试一次

---

## 7. 渲染子系统

### 7.1 两阶段路线

| 阶段 | 方案 | 单张耗时 | 说明 |
|---|---|---|---|
| **一** | `chromiumoxide` 驱动 Chromium | 50–150ms | **复用现有全部 HTML 模板**，零改造成本 |
| **二** | `resvg` 直出 SVG → PNG | 5–20ms | Top N 高频图（词云/排行榜/签到卡），性能 10×，内存 ~5MB |

> 阶段一可先**跨进程复用现有 Java Puppeteer 服务**（JSON/stdio 接口），先跑通再替换。

### 7.2 渲染服务接口

```rust
pub struct RenderRequest {
    pub template: String,               // 模板名
    pub data: serde_json::Value,        // 模板数据
    pub viewport: Option<(u32, u32)>,
    pub scale: Option<f32>,             // 默认 2.0 出高清图
}

// 三级降级：resvg 快路 → chromium 慢路 → 纯文本
```

### 7.3 必做优化

| 优化 | 说明 |
|---|---|
| **结果缓存** | `hash(模板 + 数据) → PNG`，`moka` 本地 LRU + Redis 二级。排行榜类重复率极高 |
| **file_info 缓存** | 上传结果有 TTL，缓存复用；上传往往比渲染更慢 |
| **字体** | 中文字体**本地加载 + 子集化**（`fontmin`），禁止走网络字体 |
| **输出规格** | 宽度 720–1080，高度 < 3000，PNG/JPEG（webp 支持存疑，稳妥用 PNG） |
| **禁止** | 每个请求启动新浏览器进程 |

---

## 8. 富媒体子系统

**分片上传 4 步**（推荐，无需公网 CDN）：

```
upload_prepare (拿 upload_id + block_size + 预签名 URL)
   ↓ for each chunk
PUT 预签名 URL  →  upload_part_finish
   ↓
POST .../files { upload_id }  →  file_info
```

**关键点**：

| 项 | 说明 |
|---|---|
| 分片大小 | 默认 5MB，由 `upload_config` 下发 |
| 超时 | 上传接口建议 ≥ 5s |
| 秒传 | `md5_10m`（前 10002432 字节 MD5）先查缓存 |
| 场景隔离 | 单聊 / 群聊上传接口不互通，缓存 key 必须带场景 |
| `srv_send_msg=true` | 上传即发送，但**占用主动消息频次**，慎用 |
| 文件类型 | 1 图片（png/jpg/gif/webp/bmp，软限 20MB / 硬限 200MB）、2 视频 mp4、3 语音 silk、4 文件 |

---

## 9. 存储与可观测性

| 用途 | 实现 |
|---|---|
| 会话状态 | 内存（SessionShard，2h 空闲淘汰） |
| 事件去重 | `moka` 本地 LRU（8192 条 / 10min） |
| 配额账本 | 内存 TokenBucket（窗口从**收到消息**起算） |
| 渲染缓存 | `moka` 本地（FNV-1a 内容 hash） |
| file_info | `moka` 本地，key 带场景，TTL 跟随服务端 |
| **消息持久化** | **嵌入式 SQLite（`rusqlite` bundled），无需外部服务** |
| 指标 | `metrics` 门面：事件速率、丢弃数、渲染耗时、上传耗时、存储写入/丢弃/清理 |
| 日志 | `tracing` 结构化 |

### 9.1 消息持久化（`qqbot-store`）

**需求**：群友与私聊的消息全部入库；定时清理 1 年以上的消息。

```text
 GatewayActor ──► Dispatcher ──► MessageStore::record   (try_send，非阻塞)
                                      │
                                      ▼
                              ┌──────────────────┐
                              │ 有界队列 4096    │ 满则丢弃 + 计数
                              └────────┬─────────┘
                                       ▼
                              [写线程] 批量事务 ──► SQLite (WAL)
                                                       ▲
                      词云插件 ──► recent_texts ──► [读线程]（独立连接）
 [清理任务] tokio interval ──► purge_before(now - 365d)
```

**三条设计红线**：

1. **写入绝不阻塞消息热路径**。`record()` 只做一次 `try_send`；SQLite 事务
   全部发生在独立 OS 线程上，按 `flush_batch`(512) 或 `flush_interval`(250ms)
   批量提交。队列满就丢弃并计数——宁可丢日志，不可拖慢回消息。
   压测断言：**20000 次 `record` 必须 < 2 秒完成**。
2. **读写分连接**。WAL 模式下读不阻塞写，因此词云那种「扫上万行」的慢查询
   跑在自己的线程上，不会卡住正在提交的写入。
3. **`msg_id` 即主键**。`INSERT OR IGNORE` 让重复推送天然幂等，
   与内存 dedup 缓存形成双保险。

**Schema**：

```sql
CREATE TABLE messages (
    id          TEXT PRIMARY KEY,        -- msg_id
    scope       TEXT NOT NULL CHECK (scope IN ('group','c2c')),
    target_id   TEXT NOT NULL,           -- group_openid / user_openid
    sender_id   TEXT,
    sender_name TEXT,
    event_name  TEXT NOT NULL,
    content     TEXT NOT NULL,
    created_at  INTEGER NOT NULL         -- Unix 秒
);
CREATE INDEX idx_messages_scope_target_time ON messages(scope, target_id, created_at DESC);
CREATE INDEX idx_messages_created_at ON messages(created_at);
```

**保留期清理**：`tokio::time::interval` 的**第一次 tick 立即触发**，
所以进程启动就会清理一次，不必等第一个间隔。清理后执行
`PRAGMA incremental_vacuum` 归还空间，避免数据库文件只增不减。
删除条件是**严格早于** `now - retention`（边界上的消息保留）。

**降级**：存储打开失败只打 ERROR 日志并继续启动，词云自动退回内存语料——
持久化故障不该让机器人整体不可用。

### 9.2 词云：词频 → 字号 + 透明度

```rust
// 面积 ∝ 词频 ⇒ 线性维度取平方根。
// 真实词频是长尾分布，线性映射会让绝大多数词挤在最小字号上。
let t = (weight / max_weight).sqrt();
let font  = MIN_FONT  + t * (MAX_FONT  - MIN_FONT);   // 20 → 78
let alpha = MIN_ALPHA + t * (MAX_ALPHA - MIN_ALPHA);  // 0.35 → 1.0
```

于是「重要性」有了**大小**与**浓淡**两个正交的视觉通道：
高频词又大又实，低频词又小又淡。

词云的语料来自数据库（`recent_texts`，默认回溯 30 天），因此**重启不丢**。
分词是纯 CPU 活，放在 `spawn_blocking` 里，不占异步运行时。

**命令调用会被剔除**：命令名从路由表推导（`Matcher::Command` 的
`describe()` 就是命令名），否则「词云」自己会变成高频词——自指噪声。

---

## 10. 迁移映射（Shiro / cq-bot → Rust）

| 旧实现 | Rust 方案 |
|---|---|
| `@PrivateMessageHandler` / `@GroupMessageHandler` + `@MessageHandlerFilter` | 显式路由表 `Router` + `trait Handler` |
| 插件顺序执行，`MESSAGE_IGNORE` / `MESSAGE_BLOCK` | `enum Handled { Consumed, Next }` |
| `PluginManager` + `DependencyResolver` | trait + 显式拓扑排序 |
| `EventHandler` / `ActionHandler` 入站出站分离 | **保留** |
| 事件 DTO 层次 | Rust enum + `match` 穷尽检查 |
| `MsgUtils.builder()` | `OutMessage` builder |
| 适配器（GoCQHTTP/NapCat/Lagrange） | **丢弃**（官方 API 只有一种协议） |
| `Bot` / `BotContainer` / `BotFactory` | 单 Bot 身份 + 多 shard 连接 |
| `PuppeteerUtil` | `RenderActor` + `chromiumoxide` |
| `PluginAspect` (AOP) | `tracing::instrument` span |
| `ThreadPoolConfig` / `BotAsyncTask` | tokio + 有界 `Semaphore` |
| `RedisUtil` / `CacheUtil` | `fred` + `moka` |
| MyBatis-Plus / MySQL | `sqlx` |
| `timer/BotTask.java` | `tokio::time::interval` per actor |
| `BiliSubscribeListener` | 独立 subscribe actor，推送走 SessionShard |
| `ImageController` | 直接走 file_info 缓存，无需 HTTP 图床 |
| `lagrange/markdown/Keyboard` | `qqbot-api::markdown` + `keyboard`（官方原生支持） |
| Jackson | `serde` + 两阶段 `RawValue` 解析 |
| Lombok | `derive` 宏 |

---

## 11. 分期落地计划

```
P0  qqbot-api + qqbot-gateway       ← 收消息 / 心跳 / Resume（核心难点）
P1  SessionShard + 发送层 + 富媒体   ← 能回文本与图片
P2  RenderActor (chromiumoxide)     ← 复用现有 HTML 模板
P3  路由表 + 迁移 Top 5 插件
P4  resvg 快路替换 Top N 高频图      ← 性能跃升
P5  Webhook 模式（可选，多实例扩容）
```

---

## 12. 风险与应对

| 风险 | 应对 |
|---|---|
| **Rust 无官方 QQ SDK** | 协议层自研（2000–3000 行）。先把 `qqbot-api` 做扎实，它是唯一必须一次写对的部分 |
| `chromiumoxide` 版本跟进慢 | 阶段一跨进程复用现有 Java Puppeteer 服务，先跑通再替换 |
| 异步 trait 复杂度 | `async-trait` 或 Rust 1.75+ 原生 AFIT；`dyn` 场景仍需 `async-trait` |
| Actor 抽象失控 | 见第 13 节约束 |
| 编译慢 | `cargo check` 作为 AI 验证命令 + `sccache` + workspace 拆 crate |
| 中文渲染 | 字体子集化，禁用网络字体 |

---

## 13. AI 开发约束（建议提取为 `AGENTS.md`）

```markdown
## Actor 边界（不得扩大）
- 只允许 3 类 actor：GatewayActor(per shard)、SessionShard(固定 N 个)、RenderActor(per browser)
- 禁止 Box<dyn Any> 消息，所有消息必须是具体 enum
- 禁止在 actor 内部 await 无超时的外部 IO（必须 timeout）
- 禁止新增全局 Mutex<HashMap<...>>，会话状态一律走 SessionShard

## 渲染
- 禁止每个请求启动新浏览器进程
- 所有渲染必须走 RenderActor，且必须带 3s 超时 + 降级到纯文本
- 禁止网络字体

## 依赖
- tokio / reqwest / serde / tracing 版本锁定，见 Cargo.lock
- 禁止引入 actix-* 系列
- 禁止在 qqbot-api 的 types/event/message/payload 模块引入 IO（HTTP 只能写在 client 子模块）

## 协议
- 判成败只依据 err_code
- 发送必须走 SessionShard 分配 msg_seq
- 所有对外请求必须有超时
```

---

## 附录：参考链接

- QQ 机器人官方文档（API v2）：<https://bot.q.qq.com/wiki/develop/api-v2/>
- 获取访问凭证：<https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/access-token.html>
- API 调用指南：<https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/api-call-guide.html>
- 事件订阅与通知（Intents / OpCode）：<https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/interface-framework/event-emit.html>
- 消息收发概述（频率与时效规则）：<https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/overview.html>
- 富媒体消息概述：<https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/rich-media.html>
- 群聊 @ 消息事件：<https://bot.q.qq.com/wiki/develop/api-v2/autogen/event/group_at_message_create.html>
- 参考项目 Shiro（OneBot 框架，插件责任链设计）：<https://github.com/MisakaTAT/Shiro>
- 参考项目 cq-bot（Shiro 上的业务实现）：<https://github.com/xiaomujin/cq-bot>
