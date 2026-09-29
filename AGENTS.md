# AGENTS.md

> 给 AI 编码代理（以及人类贡献者）的项目约束。
> 与 [README.md](README.md) 冲突时，**以本文件为准**；
> 协议要点速查见 [docs/qq-bot-api-v2-protocol.md](docs/qq-bot-api-v2-protocol.md)。

本文件只写「不看就会犯错」的东西。能靠读代码或 cargo 得到的信息一律不重复。

---

## 0. 项目速览

Rust workspace 实现的 QQ 官方 API v2 机器人（群聊 / 单聊，不含频道）。

| 项 | 值 |
|---|---|
| edition / resolver | `2024` / `3` |
| MSRV | **1.88**（edition 2024 要 1.85，rusqlite 0.40.2 要 1.88，取高者） |
| 结构 | 7 个 crate（`crates/*`）+ 根 bin（`src/`） |
| lint 策略 | `unsafe_code = "deny"`、clippy `correctness = "deny"`，**零警告** |
| 测试基线 | 354 项，全绿 |
| 运行时依赖 | 无。单静态二进制，不需要 Redis / 外部数据库 / Node / Chromium |

---

## 1. 命令

| 目的 | 命令 |
|---|---|
| 编译检查（迭代首选） | `cargo check --workspace` |
| lint | `cargo clippy --workspace --all-targets` |
| 测试 | `cargo test --workspace` |
| release 构建 | `cargo build --release --workspace` |
| 渲染自检（离线，无需网络与凭据） | `cargo run -- self-test` |
| 凭据与网关连通性 | `cargo run -- check` |
| 启动机器人 | `cargo run` |
| 消息库统计 | `cargo run -p qqbot-store --example seed -- data/qqbot.db` |

日志级别用 `RUST_LOG`（如 `RUST_LOG=debug`）。

### 与机器人同时构建

Windows 会锁住**正在运行的那个** exe。所以**开发期一律从副本启动**，
让 `target/debug/qqbot.exe` 保持自由 —— 边跑边编，不用停机器人：

```powershell
# 启动 / 换二进制
cargo build
Copy-Item -Force target\debug\qqbot.exe target\devrun\qqbot.exe
Remove-Item target\live.log -ErrorAction SilentlyContinue
$env:RUST_LOG='info'
& .\target\devrun\qqbot.exe run 2>&1 | Tee-Object -FilePath target\live.log
```

`target\devrun\` 落在 `/target` 忽略范围内，不会入库。

只有**改了代码**时才会撞锁（cargo 需要重新链接）。`cargo test` 与 `cargo clippy`
**永远不受影响** —— 它们只写 `target/debug/deps/`，不重写这个 exe。
万一直接跑了 `target/debug/qqbot.exe`，症状是 `failed to remove file ... 拒绝访问 (os error 5)`；
**不要**为了构建去杀进程或改锁，停掉机器人再编即可。

### debug 构建的代价（实测）

渲染慢 **25~31 倍**：卡片 1184ms / 词云 2727ms（release 分别是 47ms / 88ms）。
仍在 `render.timeout_secs` 默认的 5s 之内，所以**不会**降级成纯文本，但余量只有 1.8 倍 ——
调试期间别同时堆多个词云请求。

好处是 debug 打开整数溢出检查：`retention_days * DAY_SECS` 这类乘法一旦回绕会
**直接 panic**，而不是在 release 下静默算出错误结果。

---

## 2. 硬性禁令

1. **绝不运行 `cargo fmt` / `cargo fmt --all`。**
   本项目**不使用 rustfmt**：风格与默认配置不同（单行结构体字面量、存在超过 100 列的行）。
   `cargo fmt --check` 会标记几乎每个文件；跑一次 `--all` 会产生上千行无意义 diff，
   淹没真正的改动。要调整格式，**手工**改，只动你正在改的那几行。

2. **绝不给 `qqbot` 传 `main.rs` 未列出的子命令。**
   只接受 `run` / `check` / `self-test` / `help`。
   其它任何值（例如 `stats`）只会打印一行 `未知模式`，然后**回退到 `run()`** ——
   于是一个意料之外的第二个实例连上网关。两个实例同时在线会导致**同一群消息被回复两次**。

3. **绝不同时运行两个实例。** 重启前先确认旧进程已退出。
   排查时用 `Get-Process qqbot`（注意：进程多于一个时它返回**数组**，不要直接做算术）。

4. **绝不把凭据 / 运行时数据 / 真实样例入库。**
   `config.toml`（含 AppSecret）、`.env`、`/data`、`/docs/samples/` 都在 `.gitignore` 里，
   而**本仓库是公开的**。`docs/samples/` 含真实 AppID、真实群 openid、真实群聊渲染出的词云截图，
   只留本地。提交前扫一眼 `git status`。

5. **不引入 `unsafe`**（`unsafe_code = "deny"`）。vendored 依赖里的不算。
   clippy 的 `correctness` 是 error 级别，不要用 `#[allow]` 压过去。

6. **不新增全局 `Mutex<HashMap<..>>` 之类的共享会话状态。** 会话状态一律走 `SessionShard`。

7. **渲染不引入外部进程 / 网络字体。** 纯 Rust `resvg` 光栅化是刻意的硬约束
   （无 Chromium、无 Node、单二进制）；别为了一个效果把它换掉。

8. **不引入 `actix-*` 系列。**

9. **不要用 `Select-String` / `grep` 过滤构建输出后只看匹配行。**
   这会把编译错误一起过滤掉，制造「测试全绿」的假象。判断成败**只看退出码**
   （PowerShell 里是 `$LASTEXITCODE`），再看完整输出。

---

## 3. 排障：陈旧构建产物

**症状**：源码里明明有这个函数，`cargo check` 却报
`no associated function or constant named ... found for struct ...`，
而 `git diff` 是空的。

**原因**：cargo fingerprint 过期 —— 它认为 crate 是最新的，但 `.rmeta` 早于那次改动。

**处理**：清掉**那一个** crate，然后重新检查：

```powershell
cargo clean -p qqbot-store   # 只清出问题的那一个
cargo check -p qqbot
```

注意 `cargo check` 的输出行本身也是线索：如果只打印了 `Checking qqbot` 而没有
`Checking qqbot-store`，就说明后者被判为「无需重编」，问题多半出在指纹而不是代码。

---

## 4. 架构边界（actor 不得扩大）

**只允许三类 actor**，判定准则：

> 有**独占的、跨消息保持的、必须串行访问**的状态 → Actor；
> 无状态的纯计算或 IO → 普通 `async fn`。

| Actor | 粒度 | 数量 | 独占状态 |
|---|---|---|---|
| `GatewayActor` | 每分片一个 | = 分片数 | `session_id` / `seq` / 心跳 / Resume 状态机 |
| `SessionShard` | **按 key 分片** | CPU 核数 × 2 | `msg_seq` 递增、被动窗口、主动配额、顺序 |
| `RenderService` | 有界队列 + 并发上限 | 1 | CPU 密集渲染的隔离、背压、超时降级 |

其余（发送层、路由表、插件、分发）全部是普通 `async fn` + 责任链。

约束：

- 禁止 `Box<dyn Any>` 消息；actor 的消息必须是**具体 enum**。
- 禁止在 actor 内部 `await` **无超时**的外部 IO。
- `SessionShard` 是「按 key 分片」，**不是一群一 actor** —— 群数量会无界增长。
- 扩大 actor 边界（新增第四类、或把无状态逻辑塞进 actor）属于架构变更：
  必须更新本节上面的边界表，并在提交说明里论证为什么现有三类不够用。

事件流（改分发逻辑前先对照）：

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

`main.rs` 的事件循环里，`dispatcher.handle()` **必须派生（spawn）出去**，不能在
`select!` 分支里直接 `await` —— 否则并发信号量只能拿到 1 个许可，一条慢命令
（如词云）会阻塞所有消息处理。这个注释在代码里，改之前先读。

---

## 5. 协议不变量（改发送链路前必读）

| 不变量 | 说明 |
|---|---|
| **只依据 `err_code` 判成败** | 失败时 HTTP 仍可能是 **200**，响应体里带 `err_code`。绝不能只看状态码 |
| **`msg_seq` 必须由 `SessionShard` 分配** | 同一 `msg_id + msg_seq` 重复发送会被拒；不能在各处自行拼发送请求 |
| **Identify / Resume 的 `token` 必须带 `QQBot ` 前缀** | 漏掉前缀服务端立刻回 op 9 InvalidSession，表现为「连上了但收不到 READY」 |
| **被动窗口不同** | 单聊 60 分钟 / 4 次；群聊 5 分钟 / 5 次。**从收到消息开始计时**，不是从回复开始 |
| **事件会重推** | 相同 `msg_id` 可能推送多次，必须幂等去重 |
| **`file_info` 不能跨场景** | 单聊 / 群聊上传接口隔离，缓存 key 必须带 scene |
| **预签名 URL 不带 `Authorization`** | 对象存储直接校验签名 |
| **所有对外请求必须有超时** | 无超时的 `await` 是缺陷，不是风格问题 |

---

## 6. 代码规范

- **注释与文档用中文**，与现有代码保持一致。公共项写 `///`。
  踩过的坑要在代码里留下「为什么」，不要只写「做了什么」。
- **edition 2024**：let-chains 可用（`if let Some(x) = a && cond`，Rust 1.88+ 稳定）。
  这类写法会让 clippy 的 `collapsible_if` 更敏感 —— 按提示合并，**不要**用 `#[allow]`。
- **依赖只在根 `Cargo.toml` 的 `[workspace.dependencies]` 声明一次**，
  成员一律 `{ workspace = true }`，需要额外 feature 时在成员里追加（feature 是叠加的）。
  这样升版本只有一处要改，也不会出现版本漂移。
- **每个成员带 `[lints] workspace = true`**，不要写局部 lint 配置。
- **错误处理**：库用 `thiserror` 定义类型化错误，组合根（`main.rs` / `config.rs`）用 `anyhow`。
- **`qqbot-api` 的 `types` / `event` / `message` / `payload` 模块禁止出现 IO**；
  HTTP 只能写在 `client.rs`。协议类型要能离线单测。
- **最小化 diff**：不做与任务无关的重命名、重排、格式化。
- **改完 Markdown 必须回读校验**：Markdown 没有编译器，占位符残留、代码块围栏错位、
  路径里的反斜杠丢失，都不会被 clippy / 测试 / 构建发现。用脚本生成 Markdown 时，
  内容里的反引号与反斜杠**一律走占位符替换**（先写 `@@` 再整体换掉），不要直接写进
  模板字面量 —— 单级替换会把 ``` 变成 `@，而反斜杠会被静默吃掉。
- Rust 代码评审可加载 `rust-skills` 规则集（265 条 / 26 类）；
  已知误报：`#[async_trait]`（`Arc<dyn Handler>` 必需）、测试里的 `unwrap()`。

---

## 7. 目录导航：改哪里

| 要做的事 | 位置 |
|---|---|
| 新增 / 修改 HTTP 端点 | `crates/qqbot-api/src/client.rs` + 对应的 `bot.rs` / `group.rs` / `message.rs` |
| 新增协议类型 | 同上的类型模块，并**从 `lib.rs` 显式 re-export** |
| 网关状态机（Identify / 心跳 / Resume / 分片 / 重连） | `crates/qqbot-gateway/src/actor.rs` |
| 发送链路、`msg_seq`、被动窗口、配额 | `crates/qqbot-core/src/session.rs` |
| 事件分发、去重、并发 | `crates/qqbot-core/src/dispatch.rs` |
| 路由表与插件 trait | `crates/qqbot-core/src/plugin.rs` |
| 业务插件 | `crates/qqbot-plugins/src/{help,dice,wordcloud,daily,resource,tarkov,ba,bili,bangumi,timewin}.rs`，注册在 `lib.rs::register` |
| 资源管理（关键词→素材） | `crates/qqbot-plugins/src/resource.rs` + `crates/qqbot-store/src/resource.rs` |
| 数据库表结构 / schema 版本 | `crates/qqbot-store/src/schema.rs`（加表要提 `SCHEMA_VERSION`；**加列必须显式 ALTER**，`CREATE TABLE IF NOT EXISTS` 对已存在的表无效） |
| 插件对外 HTTP | `crates/qqbot-plugins/src/http.rs`（统一 20s 超时，**不要在插件里自建 `reqwest::Client`**） |
| 插件配置 | 各插件自带 `XxxConfig`（如 `DailyConfig`），由 `PluginsConfig` 汇总传入 `register` |
| 富媒体分片上传 / 秒传缓存 | `crates/qqbot-media/src/uploader.rs` |
| 渲染服务、模板、词云布局 | `crates/qqbot-render/src/`。**SVG 模板必须转义**（`&` 不转义会让 XML 解析失败，整张卡片渲染不出来） |
| 存储 schema / 读写线程 / 保留期 | `crates/qqbot-store/src/` |
| 配置项与环境变量 | `src/config.rs`（新增键要同步 `config.example.toml`） |
| 装配与事件主循环 | `src/main.rs` |

配置优先级：**环境变量 > `config.toml` > 内置默认值**。
`config.toml` 已 gitignore，模板是 `config.example.toml`；`QQBOT_CONFIG` 可指定路径。
配置解析是**纯函数**（`Config::resolve(env, file)`，见 `src/config.rs`）：
edition 2024 起 `env::set_var` 是 unsafe，而本 workspace 禁 unsafe，
所以**必须注入环境变量才能测试** —— 不要在 `resolve` 里直接读 `std::env`。

---

## 8. 文档同步义务

代码改了，对应文档必须一起改 —— 否则宁可不动文档，也别留下过期的说法：

| 改了什么 | 同步哪里 |
|---|---|
| 协议行为 / 端点 / 频控 | `docs/qq-bot-api-v2-protocol.md`（端点表在 §11，约束要点在 §12） |
| 测试数量、命令、部署要求 | `README.md` |
| actor 边界、硬性禁令、命令清单 | 本文件自身 |
| 新增接口 | 协议文档 §11 表格 + `client.rs` + 类型模块 + `lib.rs` re-export |

---

## 9. 完成前的自检清单

声称「做完了」之前，逐条确认：

- [ ] `cargo clippy --workspace --all-targets` **退出码 0**，零警告
- [ ] `cargo test --workspace` 全绿，**退出码 0**（不是「过滤后的输出里没有 failed」）
- [ ] 涉及运行时行为的改动，在**真实环境**验证过（机器人实例 + 群消息），而不只是单测
- [ ] 新增协议类型有对应的 serde / 构造器单测
- [ ] 相关文档已同步（见第 8 节）
- [ ] `git status` 干净，且**没有**凭据、`data/`、`docs/samples/`
- [ ] **没有**运行过 `cargo fmt`
- [ ] 没有留下第二个机器人实例在跑
