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

### 1. 配置

任选一种（优先级从高到低）：

```bash
# 方式一：配置文件（推荐）—— 模板带注释，config.toml 已加入 .gitignore
cp config.example.toml config.toml
# 然后填 app_id / client_secret

# 方式二：环境变量（容器 / CI 用这个）
export QQBOT_APP_ID=你的AppID
export QQBOT_APP_SECRET=你的AppSecret
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
| `日报` | 当日早报长图（需在配置里填 `daily.token`，否则该指令不注册） |
| `<关键词>` | 发送收录的素材。**本群优先，其次系统资源** |
| `资源列表` | 本群资源 + 系统资源清单（含别名与说明） |
| `收录 <关键词>` | **群管理员**：把本条消息的图片收进本群；或 `收录 <关键词> <路径>` 从服务器本地导入（保真） |
| `别名 <关键词> <新词>` | **群管理员**：给本群资源加触发词 |
| `删除资源 <关键词>` | **群管理员**：删除本群资源（删不到系统资源） |
| `系统收录` / `系统别名` / `系统删除` / `系统列表` | **系统控制者**：管理全局资源 |
| `系统控制者 添加/移除 <openid>` | **系统控制者**：管理控制者名单 |
| `帮助` | 自动生成的指令表 |

---

## 配置

优先级（从高到低）：

1. **环境变量** `QQBOT_*` / `RUST_LOG` —— 部署期覆盖（容器 / CI / systemd）
2. **`config.toml`** —— 本机基线，允许只写一部分
3. 内置默认值

`config.toml` 默认从当前目录向上查找；也可以用 `QQBOT_CONFIG` 指定路径（此时文件必须存在）。
仓库里的 [config.example.toml](config.example.toml) 是带注释的模板：

```bash
cp config.example.toml config.toml
```

**没写进 `config.toml` 的键一律退回默认值**，所以以后新增键不会让旧配置失效；
但键名**拼错会直接报错**，不会被静默忽略。

| 键 | 环境变量 | 默认值 | 说明 |
|---|---|---|---|
| `app_id` | `QQBOT_APP_ID` | 无（必填） | 机器人 AppID |
| `client_secret` | `QQBOT_APP_SECRET` | 无（必填） | 机器人 AppSecret |
| `api_base` | `QQBOT_API_BASE` | 官方地址 | API 基址（调试 / 私有化部署） |
| `gateway_url` | `QQBOT_GATEWAY_URL` | 自动探测 | 网关地址（调试用） |
| `db_path` | `QQBOT_DB_PATH` | `data/qqbot.db` | 消息库路径。**置为空串即关闭持久化**（词云退回内存语料） |
| `retention_days` | `QQBOT_RETENTION_DAYS` | `365` | 消息保留天数（1 ~ 36500），超期由定时任务清理 |
| `wordcloud_window_days` | `QQBOT_WORDCLOUD_WINDOW_DAYS` | `30` | 词云统计窗口 |
| `log_level` | `RUST_LOG` | `info` | 日志级别 |
| `session_shards` | `QQBOT_SESSION_SHARDS` | CPU 核数 × 2 | 会话分片数 |
| `dispatch_concurrency` | `QQBOT_DISPATCH_CONCURRENCY` | `16` | 单条事件处理的最大并发 |
| `render.timeout_secs` | `QQBOT_RENDER_TIMEOUT_SECS` | `5` | 单次渲染超时（秒），超时降级为纯文本 |
| `daily.token` | `QQBOT_DAILY_TOKEN` | 空 | 早报接口令牌。**留空则不注册 `日报` 指令** |
| `daily.api_url` | `QQBOT_DAILY_API_URL` | alapi 早报 | 早报接口地址（任何返回 `data.image` 的接口都能换） |
| `daily.cache_secs` | `QQBOT_DAILY_CACHE_SECS` | `1800` | 早报缓存时长（1 ~ 86400） |
| `resources.basepath` | `QQBOT_RESOURCES_BASEPATH` | `data/resources` | 从消息收录的素材落盘目录 |
| `resources.system_controllers` | `QQBOT_SYSTEM_CONTROLLERS` | 数据库 | 系统控制者 openid（逗号或空格分隔）。**显式设置时覆盖数据库** |

> 「缺失」与「非法」是两回事：**没写** → 用默认值；**写了但解析不了**
> （例如 `QQBOT_RETENTION_DAYS=abc`）→ 启动直接报错，不会静默回退成 365 天。
>
> 启动日志会打一行 `配置已加载 sources=...`，说明这次生效的来源，
> 便于排查「我改的到底是哪个文件」。

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
cargo test --workspace        # 224 项测试（含端到端）
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
└─ config.rs        配置加载（环境变量 > config.toml > 默认值）
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