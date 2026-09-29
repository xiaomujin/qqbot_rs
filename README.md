# qqbot

基于 **QQ 开放平台官方 API v2** 的群聊 / 单聊机器人，Rust 实现。

- **收发消息**：群聊（@机器人 / 全量）、单聊；文本 / Markdown / 富媒体
- **图片渲染**：纯 Rust `resvg` 光栅化，**无 Chromium、无 Node**，单二进制部署
- **协议正确性**：被动回复窗口、`msg_seq` 递增、主动消息配额全部按官方文档实现
- **可扩展**：显式路由表 + 责任链插件模型，加一条指令只需实现一个 trait
- **零外部依赖**：会话状态、去重、缓存在进程内，不需要 Redis / 数据库服务

> 给 AI 编码代理的项目约束见 [AGENTS.md](AGENTS.md)；
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
# AppID：你的AppID
# AppSecret：你的AppSecret
```

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

## 配置项

| 环境变量 | 默认值 | 说明 |
|---|---|---|
| `QQBOT_APP_ID` / `QQBOT_APP_SECRET` | 无 | 凭据。未设置时回退读 `bot.txt` |
| `QQBOT_DB_PATH` | `data/qqbot.db` | 消息库路径。**显式置空可关闭持久化**（词云退回内存语料） |
| `QQBOT_RETENTION_DAYS` | `365` | 消息保留天数，超期由定时任务清理 |
| `QQBOT_WORDCLOUD_WINDOW_DAYS` | `30` | 词云统计窗口 |
| `QQBOT_API_BASE` | 官方地址 | 覆盖 API 基址（调试用） |
| `QQBOT_GATEWAY_URL` | 自动探测 | 覆盖网关地址（调试用） |
| `RUST_LOG` | `info` | 日志级别 |

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

## 测试与自检

```bash
cargo test --workspace        # 209 项测试（含端到端）
cargo test --test end_to_end  # 只跑收发链路端到端
cargo test -p qqbot-gateway --test gateway_protocol  # 只跑网关协议（Identify/Resume/分片/op9）
cargo test -p qqbot-store     # 只跑存储：幂等 / 隔离 / 保留期边界 / 不阻塞
cargo run -- self-test        # 渲染链路离线自检（无需网络与凭据）
cargo run -- check            # 凭据与网关连通性
```

### 端到端测试（`tests/end_to_end.rs`）

不依赖真实凭据、不触碰线上：测试内起一个**本地 mock HTTP 服务器**，
真实跑 `ApiClient` + `SessionRegistry` + `Dispatcher` + 插件 + `RenderService`，
只把网络出口指向 127.0.0.1，然后断言 mock **实际收到的 HTTP 请求**。

覆盖：鉴权头格式、`msg_id`/`msg_seq` 语义、事件去重幂等、群聊/单聊端点选择、
「收到词云 → 渲染 PNG → 分片上传 → 合并 → 以 `msg_type=7` 发送图片」全链路、
秒传缓存命中、未知事件无副作用。

### 运维小工具

```bash
# 查看消息库统计（行数、入队/写入/丢弃数、保留期）
cargo run -p qqbot-store --example seed -- data/qqbot.db

# 塞一条 400 天前的消息，然后重启机器人，验证保留期清理是否生效
cargo run -p qqbot-store --example seed -- data/qqbot.db 400 "旧消息"
```

---

## 部署

需要 **Rust 1.88+**（edition 2024 要 1.85，rusqlite 0.40.2 要 1.88，取较高者）。
SQLite 走 `rusqlite` 的 `bundled` 特性，源码随依赖一起编译，**无需预装 SQLite**。

```bash
cargo build --release
./target/release/qqbot
```

单一静态二进制，无运行时依赖。运行期不需要 Redis / 数据库
（会话状态、去重、缓存在进程内）；需要横向扩容时再引入外部存储。

需要多分片时，`/gateway/bot` 会返回建议分片数，`spawn_gateway` 会自动为每个分片
建立独立连接与独立 actor。

---

## 目录结构

```
crates/
├─ qqbot-api/       协议类型 + HTTP 客户端 + access_token 管理（唯一含 IO 的协议层）
├─ qqbot-gateway/   WebSocket 网关：Identify / 心跳 / Resume / 重连 / 分片
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

## 设计要点

只有三处 actor，其余全部是普通 `async fn` + 责任链：

| Actor | 粒度 | 解决什么 |
|---|---|---|
| `GatewayActor` | 每分片一个 | `session_id` / `seq` / 心跳 / Resume 状态机 |
| `SessionShard` | 按 key 分片（CPU 核数 × 2） | `msg_seq` 递增、被动窗口、主动配额、顺序保证 |
| `RenderService` | 有界队列 + 并发上限 | CPU 密集渲染隔离、背压、超时降级 |

`SessionShard` 是最关键的一环：官方协议的三条硬约束都是「按键串行的状态机」，
放进固定数量的 shard actor 后**零锁、不可能死锁、天然有序**。

踩过的协议坑与接口清单见 [docs/qq-bot-api-v2-protocol.md](docs/qq-bot-api-v2-protocol.md)；
动代码之前请先读 [AGENTS.md](AGENTS.md)。
