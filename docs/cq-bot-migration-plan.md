# cq-bot → qqbot 迁移优先级

> 对 [xiaomujin/cq-bot](https://github.com/xiaomujin/cq-bot)（Java 25 + Spring Boot + Shiro，OneBot v11 反向 WS）
> 的业务功能盘点，以及迁移到本项目（QQ 官方 API v2，Rust）的优先级排序。
> 盘点基于 `master` 分支逐文件阅读，141 个 Java 文件、24 条触发正则、4 张 SQLite 表。

---

## 0. 一句话结论

cq-bot 有 **52 个业务功能**（完整清单见 [cq-bot-feature-catalog.md](cq-bot-feature-catalog.md)）。

> **更新（已开启群聊全量模式）**：原本最大的障碍是「官方群聊只推送 @机器人的消息」，
> 而全量模式解除了它——裸关键词触发可以**原样保留**。本项目代码无需改动，
> `event/mod.rs:133` 已解析 `GROUP_MESSAGE_CREATE`，`dispatch.rs:81` 已同等处理。
>
> 同时，每条群消息都带 `msg_id`，命令响应可以走**被动回复**而不消耗主动配额。

剩下的真正障碍只有一个：**渲染方式**。源项目靠 Chromium 截图（约 12 处），
而本项目是纯 Rust resvg —— 这部分必须重写模板，是**主要工作量**所在。

---

## 1. 源项目结构

| 维度 | 情况 |
|---|---|
| 语言 / 框架 | Java 25 + Spring Boot + [Shiro](https://github.com/MisakaTAT/Shiro) 3.x |
| 协议 | OneBot v11 反向 WebSocket，端口 8081，路径 `/ws/cq/` |
| 规模 | 141 个 `.java`，242 个版本库条目 |
| 插件模型 | **两套并存**：Shiro 注解链（`@AnyMessageHandler` + `@MessageHandlerFilter`）与自研反射链（`@BotHandler` + `@BotMsgHandler`） |
| 命令定义 | `enums/Regex.java` 24 条正则 + `constant/CmdConst.java` 24 个前缀常量 |
| 数据 | SQLite `bot.db`（MyBatis-Plus）4 张表：`bullet`、`tkf_task`、`tkf_task_target`、`word_cloud` |
| 配置 | `configs/<类名>.json` 反射加载，**无文件监听、无热重载** |
| 渲染 | Chromium（jvppeteer，headless，每 500 次重启）+ 纯 Java Kumo 词云 |
| 缓存 | 进程内 ExpiringMap（`CacheUtil`），重启即失 |
| Redis | **实际未使用** —— `RedisUtil.java` 整个文件被注释，是死代码 |

---

## 2. 六个硬约束（决定优先级的真正依据）

| # | cq-bot 的做法 | 官方 API v2 | 影响 |
|---|---|---|---|
| 1 | `@AnyMessageHandler` 全量监听，**裸文本正则即可触发** | ✅ **已解除**：群聊全量模式已开启，收到 `GROUP_MESSAGE_CREATE` | 触发契约**可以原样保留**，无需给命令加 @ 前缀 |
| 2 | `setGroupReaction("424")` 给消息加「处理中」表情（6 个插件在用） | **无对应接口** | 只能靠先回一条「处理中」文本，或直接去掉 |
| 3 | `OneBotMedia.file(本地路径)` / `base64://` 直发图 | 必须先上传富媒体拿 `file_info`（单聊/群聊接口隔离） | 本项目 `qqbot-media` 已解决 |
| 4 | Puppeteer 截 HTML 页出图（**约 12 处**） | 无浏览器 | 必须改写成 resvg 模板，这是最大的**工作量**来源 |
| 5 | 无限制主动推送（13 分钟轮询全群广播、B 站订阅） | 群聊主动消息 **60 qpm / 单群 20 qpm / 每群每天 1000 条**；命令响应改走**被动回复**（依附 `msg_id`，不占配额） | 只有**无消息可依附的定时推送**才需要主动消息，日常够用 |
| 6 | 硬编码凭据：SauceNao key、alapi token、`cf_clearance`、`PHPSESSID`、B 站 `buvid3` cookie、BA token | — | 一律外置到 `config.toml`，**绝不能入库** |

反过来，有两条原本以为会挡路、其实不挡：

- **引用回复**：`message_reference` 已在 [crates/qqbot-api/src/message.rs](crates/qqbot-api/src/message.rs) 实现（`:229`）。
- **Markdown + 按钮**：`msg_type=2` 与 `keyboard` 均已实现（`:218`），cq-bot 里被注释掉的 `md2` 反而能在 v2 复活。

而且 cq-bot **从未使用**撤回、禁言、群成员列表、合并转发、戳一戳 ——
v2 这些接口本项目都有了，没有历史包袱要背。

---

## 3. 业务功能全清单

### 3.1 通用

| 功能 | 触发 | 做什么 | 目标侧现状 |
|---|---|---|---|
| 帮助 | `帮助` | 硬编码 20 条列表（`HelpPlugin`） | ✅ 已有，且是**自动生成**的，比源实现好 |
| 骰子 | `.r` / `。r` | 解析 `[min,max]` 取随机数 | ✅ 已有 `骰子` / `roll` / `r`，语法待对齐 |
| 词云 | `(我的\|本群)(今日\|本周\|本月\|本年)词云` | 8 种组合；数据源 `word_cloud` 表 | ⚠️ **只有单一 `词云` 命令**，8 个变体与「我的」过滤缺失 |

### 3.2 塔科夫（体量最大的一块，14 个命令）

| 功能 | 触发 | 数据源 | 输出 |
|---|---|---|---|
| 服务器状态 | `服务器` / `tkf服务器` | `status.escapefromtarkov.com` | 文本 |
| 跳蚤市场 | `跳蚤 <名>` | `tarkov-market.com/api/be/items`（需 `cf_clearance`） | 图文 |
| 跳蚤（@ 版） | `@bot 跳蚤 <名\|24位id>` | `api.tarkov.dev/graphql` | Markdown → 截图 |
| 查任务 | `查任务 <名>` | 表 `tkf_task` + `tkf_task_target` | 截图 |
| 更新任务库 | `更新任务`（仅管理员） | GraphQL 全量重建两表 | 文本 |
| 查子弹 | `查子弹 <名>` | 表 `bullet`（2629 行） | 截图（画布高随命中数 550→2100） |
| BOSS 刷新率 | `boss刷` / `boss概` | `api.tarkov.dev/graphql` | 纯文本 |
| 塔科夫时间 | `塔科夫时间` | **纯本地计算**（莫斯科时间 × 7） | 纯文本 |
| 地图 / 任务流程图 / 任务物品图 / 信誉栏位图 / boss丢包时间 / 3x4道具 / 耳机强度 | 对应前缀 | wiki 抓取 + 本地图 | 图片 |
| 转世人生 | `转世人生` / `活一回` / `转世` / `转生` | 本地模板页 | 截图 |

### 3.3 B 站

| 功能 | 触发 | 数据源 | 输出 |
|---|---|---|---|
| 视频卡片解析 | 消息含 `bilibili.com/`、`b23.tv/` 或小程序卡片 | `api.bilibili.com/x/web-interface/view` | 封面图 + 文本 |
| 动态 / 专栏 | 同上，URL 为 `t.bilibili.com` / `opus` / `read` | Puppeteer 截页面 | 图片 |
| 哔哩动态 | `哔哩动态 <uid>` | 抓 `x/polymer/web-dynamic/v1/feed/space` 取最新一条 → 截图 | 图片 + UP 信息 |
| 订阅 / 退订 | `哔哩订阅 <uid>` / `哔哩退订 <uid>` | 存 `BiliCfg` JSON（uid → 群列表） | 主动推送到群 |

### 3.4 游戏战绩与资讯

| 功能 | 触发 | 数据源 |
|---|---|---|
| 三角洲行动 | `三角洲\|df\|sjz` + `集市\|脑机\|密码\|一图流` | `kkrb.net`（需 `PHPSESSID` 会话） |
| 永劫无间 | `永劫\|yj\|劫` + `战绩 <名>` | `record.uu.163.com`（硬编码 cookie） |
| 彩虹六号 | `r6战绩` | `r6.tracker.network`（Puppeteer 渲染，反爬） |
| 300 英雄 | `300战绩` | `300report.jumpw.com` + Puppeteer |
| 蔚蓝档案 | `总力战` / `ba日历` / `ba <名>` | `api.arona.icu` + `arona.diyigemt.com` |
| 番剧日历 | `今日番剧` / `每日番剧` / `最新番剧` | Puppeteer 截 `agedm.io` |
| 日报 | `日报` | `v2.alapi.cn/api/zaobao`（token 硬编码） |
| 摸鱼日历 | `日历` | `api.52vmy.cn/api/wl/moyu`（源码里用 **curl 子进程**下载） |

### 3.5 工具类

| 功能 | 触发 | 做什么 |
|---|---|---|
| AI 问答 | **@机器人 即触发**（`isAtMe`），无命令前缀，**仅群聊**（走 `sendGroupMsg`） | Spring AI 流式；会话按 **groupId** 记忆（整群共用一条历史，**不按用户**），窗口 16 条 / 60 分钟过期；支持图片理解（仅取首张，无图则回溯被引用消息取图）；4 个 function tool（`web_search` / `current_time` / `help` / `tarkov_flea_market`）；按 `\n\n` 分段发送，首条带引用 |
| 搜图 / 识图 | `搜图` / `识图` 进模式（100 秒），之后任何图消息 | SauceNao + ascii2d 兜底 |
| 图语 | `图语 <文本>` | 给下一张图配字回图 |
| 涩图 | `涩图` / `高清涩图` | `api.lolicon.app/setu/v2`（帮助里已注释掉） |
| 车牌 | `车牌 <关键词>` | `0magnet.com` 搜索 |
| 爬 / 来份腿 / 买家秀 / 舔狗日记 | `爬 @某人` 等 | `ovooa.caonm.net` 系列接口 |
| 语音 | `.说 <文本>` | VITS TTS 接口 → 语音消息 |
| 自我更新 | `自我更新`（仅管理员） | 执行 `sh /mnt/qqbot/wsserver/release.sh` |
| 群消息记录 | 无命令，全量落库 | 表 `word_cloud`（约 182 万行）；源实现另调 `get_group_info` 换群名 |

---

## 4. 迁移优先级

排序依据：**用户价值 × 目标侧可行性 ÷ 成本**，其中「目标侧可行性」由第 2 节的六个约束决定，
「成本」主要由**要不要新写 resvg 模板**和**数据源能不能稳定拿到**决定。

### P0 —— 零外部依赖，直接复用现有基础设施

| 序 | 功能 | 为什么排最前 | 成本 |
|---|---|---|---|
| 1 | **词云补齐 8 个变体** | 数据（`messages` 表）、分词（jieba）、渲染（resvg 词云）**全部已就绪**，只差时间窗参数与「我的」的 `sender_id` 过滤。cq-bot 侧这块本来就不用 Chromium | 半天 |
| 2 | **塔科夫时间** | 纯本地计算，零依赖零风险，是最便宜的一条命令 | 1 小时 |
| 3 | **骰子语法对齐** | 已有实现，补 `.r` / `。r` 别名与参数记法即可 | 1 小时 |
| 4 | 帮助表自动收录 | 随插件注册自动完成，无需单独工作 | — |

### P1 —— 纯文本或单次 HTTP，无渲染

| 序 | 功能 | 为什么 | 成本 |
|---|---|---|---|
| 5 | **AI 问答** | **语义零损失**：cq-bot 用 `isAtMe` 判定，官方 API 的 `GROUP_AT_MESSAGE_CREATE` 天然就是「@ 才推送」——这是全项目唯一一个触发契约完全对齐的功能。两个要处理的点：群聊无流式（官方仅单聊支持），降级为按 `\n\n` 分段发送；且**群聊被动窗口只有 5 次**，长回答必须限制分段数，否则第 6 段会失败 | 3–5 天 |
| 6 | **塔科夫服务器状态** | 单次 HTTP + 文本，20 分钟缓存 | 半天 |
| 7 | **BOSS 刷新率** | 单次 GraphQL + 文本 | 半天 |
| 8 | **B 站视频卡片解析** | 单次 HTTP 拿稿件信息 + 上传封面图；`qqbot-media` 已解决发图 | 1–2 天 |

### P2 —— 需要新写 resvg 模板或并入新表

| 序 | 功能 | 为什么 | 成本 |
|---|---|---|---|
| 9 | **塔科夫跳蚤 + 查任务 + 查子弹** | 价值最集中的一块。三张表并入现有 SQLite（`bullet` 2629 行、`tkf_task` 356 行、`tkf_task_target` 8528 行），配三个 resvg 模板替掉三处 Puppeteer 截图 | 1–2 周 |
| 10 | **日报 / 摸鱼日历** | **比源实现更简单**：源项目截自己的模板页，而这里第三方接口直接给图片，下载 → 上传 → 发送即可，**不需要模板** | 1 天 |
| 11 | **B 站动态卡片** | 从「Puppeteer 截页面」改成「抓 API + resvg 卡片」，是渲染改写的标准练习 | 2–3 天 |

### P3 —— 第三方接口稳定性与合规不可控，按兴趣

| 序 | 功能 | 备注 |
|---|---|---|
| 12 | 蔚蓝档案 `ba <名>` | arona 接口直接返回图片，成本低，但接口是个人维护 |
| 13 | 番剧日历 | `agedm.io` 抓取 + 出图 |
| 14 | 三角洲行动 | 需要维持 `kkrb.net` 会话与版本号，脆弱 |
| 15 | 永劫无间战绩 | 硬编码 cookie，随时失效 |
| 16 | 搜图 / 识图 + 图语 | 需要图片消息输入（v2 支持 `attachments`），但 SauceNao key 与 ascii2d 抓取都是外部不确定项 |

### P4 —— 需要重新设计，或建议不迁移

| 功能 | 判断 |
|---|---|
| **B 站订阅主动推送** | 技术上可行（每群每天 1000 条主动消息），但需要订阅表 + 定时器 + 配额器 + 去重 + 内容审核。**价值高、成本也高，应单独立项**，不要塞进插件批次里 |
| 涩图 / 买家秀 / 来份腿 / 舔狗日记 / 车牌 | **建议不迁移**。合规风险 + 第三方接口不稳定，且与本项目的定位不符 |
| 彩虹六号 / 300 英雄战绩 | 源实现靠 Chromium 绕过反爬，Rust 侧没有对应手段；除非能找到官方 API，否则放弃 |
| `.说` TTS 语音 | 官方 API 语音要求 silk 格式，Rust 侧没有 silk 编码器。`FileType::Voice` 虽已支持，但编码是硬缺口 |
| 自我更新 | 强部署耦合（写死 `/mnt/qqbot/wsserver/release.sh`），**直接砍掉** |
| 群消息记录 `LogPlugin` | 已由 `qqbot-store` 的 `messages` 表覆盖。群名解析不成问题：v2 的 `group_info` 在 [client.rs](crates/qqbot-api/src/client.rs) 已实现，对应源实现的 `get_group_info` |
| `md2`（markdown + 按钮演示） | 源里是注释状态；v2 的 `keyboard` 已实现，可作为独立小功能重做，不属于迁移 |

---

## 5. 建议的第一批

如果要一次提交做完，我建议这个组合（约 1 周，全部落在 P0 + P1）：

1. **词云 8 变体** —— 唯一一个「数据、分词、渲染全就绪」的功能，性价比最高
2. **塔科夫时间** + **BOSS 刷新率** + **服务器状态** —— 三条纯文本命令，把塔科夫域的入口先立起来
3. **骰子语法对齐**

第二批再上 **AI 问答**（价值最高但需要接 LLM 与工具调用）和 **塔科夫三张表 + 三个模板**（工作量最大但价值最集中）。

## 6. 迁移时必须一并处理的五件事

1. **触发契约可以保留，但要防误触发**：全量模式下机器人会看到**所有**群消息，而 cq-bot 用的是 `startsWith` 宽前缀（`帮助`、`地图`、`日历` 这类词在日常聊天里很容易被碰到）。建议改用整串匹配或加前缀符号，否则会频繁插话。
2. **凭据外置**：`config.toml` 新增键（SauceNao key、alapi token、LLM key 等），同步更新 `config.example.toml`，**绝不让真实值进版本库**。
3. **配额器**：任何主动推送（订阅、定时）都必须过 `SessionShard` 的配额校验，否则会撞 1000 条/群/天 的上限。
4. **表情回应替代**：6 个插件用 `setGroupReaction("424")` 表示「处理中」。v2 没有这个接口，统一改为先回一条短文本（如「回想中，请耐心等待～」——源项目里这句话本来就有，只是被注释了）。
5. **权限模型要重新决定**：cq-bot 唯一的权限概念是一份**硬编码的 adminList**（`AdminCfg`，两个 QQ 号写死在源码里），而 `涩图` 与 `哔哩订阅` 实际上都是管理员专属——普通成员发 `涩图` 只会收到「暂未开放」。照搬这套会把这些功能对普通成员锁死。v2 侧可以用现成的群成员角色（`MessageEvent::is_admin` / `sender_role`）重做，**在动手前先想清楚哪些命令对谁开放**。

---

## 附：与源项目的对应关系速查

| 源项目位置 | 目标项目位置 |
|---|---|
| `plugin/*.java`、`annoPlugin/*.java` | `crates/qqbot-plugins/src/<name>.rs` + `lib.rs::register` |
| `enums/Regex.java` | `Matcher::Regex`（`crates/qqbot-core/src/plugin.rs:84`） |
| `core/cfg/ConfigManager.java` | `src/config.rs` |
| `utils/CacheUtil.java` | `moka`（已在 `[workspace.dependencies]`） |
| `utils/PuppeteerUtil.java` | `crates/qqbot-render/src/template.rs`（resvg） |
| `utils/BotUtil.getLocalMedia` | `crates/qqbot-media/src/uploader.rs` |
| `entity/WordCloud.java` + `word_cloud` 表 | `crates/qqbot-store` 的 `messages` 表 |
| `timer/BotTask.java` | **尚无对应** —— 需要新增定时任务基础设施 |
| `constant/Constant.java` 的 URL 表 | 应改为配置项，不要照抄成常量 |

> 注意：`timer/BotTask.java` 的三个 `@Scheduled` 与订阅推送，是本项目**目前完全没有**的能力。
> 如果要做 P4 的 B 站订阅，必须先补一个带配额控制的调度器。
