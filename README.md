# qqbot

基于 **QQ 开放平台官方 API v2** 的群聊 / 单聊机器人，Rust 实现。

- **收发消息**：群聊（@机器人 / 全量）、单聊；文本 / Markdown / 富媒体
- **图片渲染**：纯 Rust `resvg` 光栅化，**无 Chromium、无 Node**，单二进制部署
- **协议正确性**：被动回复窗口、`msg_seq` 递增、主动消息配额全部按官方文档实现
- **可扩展**：显式路由表 + 责任链插件模型

> 完整设计说明见 [ARCHITECTURE.md](ARCHITECTURE.md)；
> 协议要点速查见 [docs/qq-bot-api-v2-protocol.md](docs/qq-bot-api-v2-protocol.md)。

---

## 快速开始

### 1. 配置凭据

任选一种（优先级从高到低）：

```bash
# 方式一：环境变量
export QQBOT_APP_ID=你的AppID
export QQBOT_APP_SECRET=你的AppSecret

# 方式二：项目根目录的 bot.txt（已加入 .gitignore）
# AppID：102072130
# AppSecret：xxxxxxxx
```

### 可选配置

| 环境变量 | 默认值 | 说明 |
|---|---|---|
| `QQBOT_DB_PATH` | `data/qqbot.db` | 消息库路径。**显式置空可关闭持久化**（词云退回内存语料） |
| `QQBOT_RETENTION_DAYS` | `365` | 消息保留天数，超期由定时任务清理 |
| `QQBOT_WORDCLOUD_WINDOW_DAYS` | `30` | 词云统计窗口 |
| `QQBOT_API_BASE` | 官方地址 | 覆盖 API 基址（调试用） |
| `QQBOT_GATEWAY_URL` | 自动探测 | 覆盖网关地址（调试用） |

### 2. 运行

```bash
cargo run                  # 连接网关并开始服务
cargo run -- check         # 只校验凭据与网关信息，不建立长连接
cargo run -- self-test     # 离线渲染自检（无需网络与凭据）
```

日志级别用 `RUST_LOG` 控制，例如 `RUST_LOG=debug`。

### 3. 群里 @ 机器人

| 指令 | 说明 |
|---|---|
| `ping` | 存活探测 |
| `骰子` / `骰子 3d6` / `roll 2d20` | 掷骰（Markdown 渲染） |
| `词云` | 用消息库里最近的群聊消息渲染词云图片（字号与透明度按词频映射） |
| `帮助` | 自动生成的指令表 |

---

## 目录结构

```
crates/
├─ qqbot-api/       协议类型 + HTTP 客户端 + access_token 管理（唯一含 IO 的协议层）
├─ qqbot-gateway/   WebSocket 网关：GatewayActor（Identify / 心跳 / Resume / 重连 / 分片）
├─ qqbot-core/      会话 actor、发送层、路由表、插件 trait、事件分发
├─ qqbot-media/     富媒体分片上传、md5_10m 秒传、file_info 缓存
├─ qqbot-render/    SVG → PNG 渲染服务（resvg）与词云布局
├─ qqbot-store/     消息持久化（嵌入式 SQLite）：写线程批量提交 + 读线程 + 保留期清理
└─ qqbot-plugins/   业务插件（帮助 / 骰子 / 词云）
src/
├─ main.rs          组合根：装配服务、连接网关、事件循环
└─ config.rs        配置加载（环境变量 > bot.txt）
```

---

## 架构要点

### Actor 边界：只有三处

| Actor | 粒度 | 数量 | 解决什么 |
|---|---|---|---|
| `GatewayActor` | 每分片一个 | = 分片数 | session_id / seq / 心跳 / Resume 状态机 |
| `SessionShard` | **按 key 分片** | CPU 核数 × 2 | `msg_seq` 递增、被动窗口、主动配额、顺序保证 |
| `RenderService` | 有界队列 + 并发上限 | 1 | CPU 密集渲染隔离、背压、超时降级 |

其余全部是普通 `async fn` + 责任链。判断准则：

> 有**独占的、跨消息保持的、必须串行访问**的状态 → Actor；
> 无状态的纯计算或 IO → `async fn`。

`SessionShard` 是最关键的一环：官方协议的三条硬约束都是「按键串行的状态机」，
放进固定数量的 shard actor 后**零锁、不可能死锁、天然有序**。

### 事件流

```
网关 WS ──try_send(不阻塞)──▶ 事件通道 ──▶ Dispatcher
                                            ├─ 幂等去重（msg_id）
                                            ├─ 登记被动回复窗口
                                            └─ 责任链 → 插件 → Ctx::reply_*
                                                              │
                                              SessionShard ───┘ 分配 msg_seq / 校验配额
                                                              │
                                                          OpenAPI 发送
```

### 渲染链路

```
模板(minijinja) 或 算法生成 SVG
        │
        ▼
   usvg 解析 ──▶ resvg 光栅化 ──▶ tiny-skia ──▶ PNG
        │
   spawn_blocking（不阻塞异步执行器）
        │
   结果缓存（模板+数据 hash）
```

实测（release，单张，含系统字体）：见 `cargo run -- self-test`。

---

## 协议实现要点（踩过的坑）

| 坑 | 说明 |
|---|---|
| **Identify 的 token 必须带前缀** | 格式为 `"QQBot {AccessToken}"`。漏掉前缀服务端会立刻回 op 9 InvalidSession，表现为「连上了但一直收不到 READY」 |
| **失败时 HTTP 仍可能是 200** | 必须依据响应体的 `err_code` 判定成败，不能只看状态码 |
| **被动回复窗口不同** | 单聊 60 分钟 / 4 次；群聊 5 分钟 / 5 次。窗口从**收到消息**开始计时，不是从回复开始 |
| **`msg_seq` 必须递增** | 相同 `msg_id + msg_seq` 重复发送会被拒绝 |
| **事件会重推** | 相同 `msg_id` 可能推送多次，必须幂等去重 |
| **单聊/群聊上传接口隔离** | `file_info` 不能跨场景使用，缓存 key 必须带 scene |
| **`file_info` 有 TTL** | 缓存时取服务端 ttl 与本地上限的较小值，宁短勿长 |
| **预上传要三个校验值** | `md5` + `sha1` + `md5_10m`（前 10002432 字节的 MD5） |
| **预签名 URL 不能带 Authorization** | 对象存储直接校验签名 |

---

## 编写插件

实现 `qqbot_core::Handler`，然后注册到路由表：

```rust
use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler, Matcher};

pub struct SignInPlugin;

#[async_trait]
impl Handler for SignInPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        // ctx.arg(0) / ctx.content() / ctx.sender_name() / ctx.is_admin()
        ctx.reply_text("签到成功 +1 天").await.ok();
        Handled::Consumed   // 终止责任链；返回 Next 则继续交给下一个插件
    }

    fn name(&self) -> &str { "签到" }
}

// 注册（放在 qqbot_plugins::register 里）
router.on_group(Matcher::Command("签到".into()), SignInPlugin);
```

回复图片只需三步（渲染 → 上传 → 发送由 `Ctx` 一次完成）：

```rust
ctx.reply_template("card.svg", serde_json::json!({
    "title": "签到成功",
    "rows": [{"label": "小明", "value": "+1 天"}],
    "width": 720, "height": 240
})).await?;
```

或直接给 SVG：

```rust
let svg = qqbot_render::build_wordcloud_svg(&words, 900, 640, "群聊词云");
ctx.reply_svg(svg).await?;
```

**为什么用显式路由表而不是过程宏注解**：路由表是数据，可打印、可单测、
可自动生成 `帮助`（见 `HelpPlugin`）；而 Rust 过程宏的编译错误定位差，会显著拖慢迭代。

---

## 测试

```bash
cargo test --workspace        # 209 项测试（含端到端）
cargo test --test end_to_end  # 只跑收发链路端到端
cargo test -p qqbot-gateway --test gateway_protocol  # 只跑网关协议（Identify/Resume/分片/op9）
cargo run -- self-test        # 渲染链路离线自检（无需网络与凭据）
cargo run -- check            # 凭据与网关连通性
cargo test -p qqbot-store     # 只跑存储：幂等 / 隔离 / 保留期边界 / 不阻塞
```

### 运维小工具

```bash
# 查看消息库统计（行数、入队/写入/丢弃数、保留期）
cargo run -p qqbot-store --example seed -- data/qqbot.db

# 塞一条 400 天前的消息，然后重启机器人，验证保留期清理是否生效
cargo run -p qqbot-store --example seed -- data/qqbot.db 400 "旧消息"
```

### 端到端测试（`tests/end_to_end.rs`）

不依赖真实凭据、不触碰线上：测试内起一个 **本地 mock HTTP 服务器**，
真实跑 `ApiClient` + `SessionRegistry` + `Dispatcher` + 插件 + `RenderService`，
只把网络出口指向 127.0.0.1，然后断言 mock **实际收到的 HTTP 请求**。

覆盖：鉴权头格式、`msg_id`/`msg_seq` 语义、事件去重幂等、群聊/单聊端点选择、
**「收到词云 → 渲染 PNG → 分片上传 → 合并 → 以 msg_type=7 发送图片」全链路**、
秒传缓存命中、未知事件无副作用。

### 网关协议测试（`crates/qqbot-gateway/tests/gateway_protocol.rs`）

用本地 mock WebSocket 网关驱动真实的 `GatewayActor`，覆盖线上无法构造的分支：

- Identify 的 token 必须带 `QQBot ` 前缀、`shard` 与 `intents` 正确、心跳回传最新 `seq`
- **Resume**：强制断线后必须发 op 6 携带 `session_id`+`seq`，且不重复 Identify
- **分片**：`shards=2` 建立两条连接，分别携带 `[0,2]` / `[1,2]`
- **op 9 InvalidSession**：清空 session 重新 Identify，不出现 Resume

> Resume 写错不会报错，只会在断线期间静默丢事件——这是最容易漏测的路径。

其余单元测试覆盖：协议解析（含未知事件容错）、被动窗口规则（单聊 60min/4 次、群聊 5min/5 次）、
配额账本、插件 panic 隔离、分片规划、渲染与词云几何布局。

---

## 部署

```bash
cargo build --release
./target/release/qqbot
```

单一静态二进制，无运行时依赖。运行期不需要 Redis / 数据库
（会话状态、去重、缓存在进程内）；需要横向扩容时再引入外部存储。

需要多分片时，`/gateway/bot` 会返回建议分片数，`spawn_gateway` 会自动为每个分片
建立独立连接与独立 actor。
