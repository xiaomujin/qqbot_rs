# 迁移清单

> 源项目 [xiaomujin/cq-bot](https://github.com/xiaomujin/cq-bot) → 本项目（QQ 官方 API v2，Rust）。
> 52 项功能中**保留 36 项**。本文件是进度的唯一真相，做完一项勾一项。
>
> 完整功能盘点见 [cq-bot-feature-catalog.md](cq-bot-feature-catalog.md)，
> 优先级论证见 [cq-bot-migration-plan.md](cq-bot-migration-plan.md)。

**进度：23 / 36 完成**

> 勾选框计数说明：阶段 1 的「B9–B15 静态图」一条含 **7 项**功能，
> 所以勾选框总数（4 + 26）会小于功能总数（36）。

---

## 0. 已确认的决策（无人值守开发的依据）

| # | 问题 | 决定 |
|---|---|---|
| 1 | B9–B15 素材来源 | **文件稍后提供，先做其他项** —— 这几项先不勾，等文件到位 |
| 2 | B4/B6 数据来源 | **用 `tarkov.dev` GraphQL 重新抓**，字段按 cq-bot 表结构映射（需 schema v4） |
| 3 | 第三方凭据 | **复用 cq-bot 里硬编码的那些**（都是公开 key，随时可能失效） |
| 4 | 遇阻塞 | **跳过并在本文件标注原因**，继续下一项，不中断整批 |
| 5 | 验收标准 | **单测 + mock 端到端**；真实群验证等人工回来补 |
| 6 | 提交节奏 | **每完成一项就单独提交并推送** |
| 7 | `.r` 骰子 | **两套并存**：`.r 100` / `.r 5 10` 走数字范围，`骰子 3d6` 走 NdM |
| 8 | 时区 | **塔科夫用 `Europe/Moscow`，其余用 `Asia/Shanghai`** |
| 9 | 模板风格 | **参照现有 `card.svg` / `notice.svg`，自行设计**，不满意再迭代 |

---

## 1. 范围

### 1.1 保留（36 项）

| 分组 | 数量 | 编号 |
|---|---|---|
| A 通用 / 系统 | 5 | A1 A2 A3 A4 A6 |
| B 塔科夫 | 15 | B1 – B15 |
| C B 站 | 5 | C1 C2 C4 C5 C6 |
| D 游戏战绩 | 5 | D1 – D5 |
| E 二次元 / 资讯 | 4 | E3 E4 E5 E6 |
| F 工具 | 2 | F2 F3 |

### 1.2 不做（16 项）

| 编号 | 功能 | 理由 |
|---|---|---|
| A5 | 自我更新 | 强部署耦合（写死服务器脚本路径），直接砍掉 |
| B16 | 转世人生 | 与 B9–B15 同源，图拿不到就不做 |
| C3 | 哔哩动态 | 与 C1/C2 重叠，且需要按 uid 抓动态（非官方接口） |
| D6 | 彩虹六号战绩 | 源实现靠 Chromium 绕反爬，Rust 侧无对应手段 |
| D7 | 300 英雄战绩 | 同上，且接口不稳定 |
| E1 | 总力战 | 第三方接口（arona）不稳定 |
| E2 | BA 日历 | 同上 |
| E7 | 涩图 | 合规风险，源项目无 R18 开关 |
| F1 | AI 问答 | 需要接 LLM 网关与工具调用，单独评估后再定 |
| F4 | 车牌 | 合规风险 |
| F5 | 爬 | 第三方娱乐接口，稳定性差 |
| F6 | 来份腿 | 同上 |
| F7 | 买家秀 | 同上 |
| F8 | 舔狗日记 | 同上 |
| F9 | 语音（.说） | 官方语音要求 **silk** 编码，Rust 侧无编码器 |
| F10 | md2 演示 | 纯演示性质，v2 的 keyboard 已实现，不需要迁移 |

---

## 2. 已完成的基础设施

这些不是功能，但后面的功能都依赖它们：

- ✅ **全量模式 @ 前缀修复** —— `MessageEvent::trimmed()` 剥离开头提及。
      修复前裸关键词能触发、@机器人 毫无反应（详见协议文档 §6.2.1）
- ✅ **插件共享 HTTP 客户端** —— `qqbot-plugins/src/http.rs`，统一 20 秒超时
- ✅ **`PluginsConfig` 结构** —— 加插件不再改 `register` 签名
- ✅ **原始事件 JSON 落库** —— `messages.raw`（schema v3），
      类型声明不了的字段只能靠原文保留
- ✅ **引用消息解析** —— 从 `msg_elements` 的 `message_type = 103` 取被引用附件
- ✅ **资源管理** —— 关键词 → 素材，群/系统两级作用域，
      **B9–B15 的静态图直接用它收录，不需要写代码**
- [x] **D1 / D2 / D3 三角洲集市 / 脑机 / 密码** —— 新插件 `delta.rs`
      ｜ ✅ **已实跑验证**（`live_delta`）
      ｜ 🔑 **关键是握手顺序**：必须依次 首页 → `?viewpage=view/overview` → `getMenu`，
      `getOVData` 才返回数据。少了 `getMenu` 会稳定拿到 `code=-101 系统繁忙`
      （实测 6 次全失败；补上之后 2 次全成功）—— 不是限流，是会话状态
      ｜ ⚠️ **实跑抓到 mock 抓不到的 bug**：`currectPrice` 是**浮点**（真实数据里出现过
      `51937.6`），按 `i64` 解析会整个失败。mock 用整数时这个分支从没被走到
- ✅ **测试基线** —— 408 项，clippy 零警告

---

## 3. 任务清单

成本口径：🟢 直接 ｜ 🔵 接口 ｜ 🟡 需写模板 ｜ 🟠 外部依赖 ｜ 🔴 阻塞

### 已完成（23 / 36）

- [x] **A1 帮助** —— `帮助` ｜ 已有，且是**自动生成**的（读路由表），比源项目的硬编码列表好
- [x] **A4 群消息记录** —— 无命令，`messages` 表全量落库（v3 起连原始 JSON 一起存）
- [x] **A6 消息日志** —— INFO 日志含附件地址与元素个数；
      `RUST_LOG=info,qqbot_api::event=debug` 可看完整原始载荷
- [x] **E5 日报** —— `日报` ｜ alapi 早报接口 → 下载图片 → 富媒体转发
- [x] **A3 词云 8 变体** —— `(我的\|本群)(今日\|本周\|本月\|本年)词云` ｜
      新增 `timewin` 模块做固定偏移日历数学（不引 `chrono`），
      `recent_texts` 增加发送者过滤以支持「我的」
- [x] **B8 塔科夫时间** —— `塔科夫时间` ｜ 纯本地计算（现实 × 7 后取莫斯科时刻），零依赖
- [x] **B7 BOSS 刷新率** —— `boss刷` / `boss概` ｜ **静态 JSON** → 纯文本，缓存 10 分钟
      ｜ 数据源从 GraphQL 改成 `json.tarkov.dev/regular/maps`（同一份数据，
      但 GraphQL 后端挂着时它照样能用）。BOSS 在 `mob` 字段且是**可读 slug**
      （`bossReshala`），不像 B4/B6 那样只有翻译键
      ｜ ✅ **已实跑验证**：`cargo test -p qqbot-plugins --lib -- --ignored live_maps` 通过 ——
      真实接口解析出十几张地图、其中若干带 BOSS。这一条补的正是 mock 证明不了的部分
- [x] **E3 BA 图片** —— `ba <名>` ｜ arona 接口 + CDN ｜ 接口已实测可用（`code=200` 命中 /
      `code=101` 模糊搜索）。**官方 API 不能直发远程 URL，必须先下载再上传**；
      模糊搜索一次返回 8 条候选，而群聊被动窗口只有 5 次，所以命中时最多发 3 张
- [x] **A2 骰子别名** —— `.r 100` / `.r 5 10`（`.。r` 都认）｜ **两套语法并存**：
      NdM 能表达「掷 N 次 M 面」，区间记法能表达 `[5,10]`，两者不等价，
      硬映射会丢掉 `[5,10]` 这种写法
- [x] **B1 服务器状态** —— `服务器` / `服务器状态`（前缀 `塔科夫`/`tkf` 可选，共 6 种）
      ｜ 两个接口均**实测可用**；缓存 20 分钟
- [x] **C1 B 站视频卡片** —— 消息含 `bilibili.com/video/` 或 `b23.tv/` 时自动展开
      ｜ 顺带给发送层加了 `media_with_text`：官方 `msg_type=7` **允许带 `content`**，
      图和文字能一条发完，不必拆两条白占被动回复配额
      ｜ 未做「小程序卡片」形态（需要解析 QQ 的 json 卡片，源项目靠 OneBot 的 array 消息）
- [x] **C4 / C5 哔哩订阅与退订** —— `哔哩订阅 <UID>` / `哔哩退订 <UID>`，**群管理员**
      ｜ 新表 `bili_subscriptions`（schema v4），按群订阅；UID 用 `web-interface/card` 校验
      ｜ 与源项目的一处差异：**退订也要管理员**。源项目只给订阅加了检查，
      那意味着任何人都能悄悄拆掉群里其他人配好的订阅
      ｜ 订阅列表已在库里，等 C6 的调度器接上即可推送
- [x] **E4 番剧日历** —— `今日番剧` / `每日番剧` / `最新番剧`
      ｜ 抓 `agedm.io/update`，正则提取标题与集数，**复用 `card.svg`** 渲染
      ｜ 顺带修掉一个渲染层的老 bug：SVG 模板原本**不转义**，数据里出现一个 `&`
      （如番剧名 `柔光魔女 & 公司`）整张卡片就渲染不出来，而错误只说
      `malformed entity reference`。旧注释写的是「不转义，否则 `&` 会破坏 XML」，那是反的
- [x] **F3 图语** —— `图语 <文字>` ｜ 新表 `pending_captions`（schema v5），一次性、5 分钟有效
      ｜ 源项目用 OneBot 的图片 `summary` 字段，**官方 v2 没有这个字段**，
      改用 `media_with_text`（图与文字同条消息），是能力上最接近的替代
- [x] **B6 查子弹** —— `查子弹 <片段>` + `更新子弹`（管理员）｜ 新表 `ammo`（schema v6）
      ｜ **数据源改成了静态 JSON**（`json.tarkov.dev/regular/items`）而不是 GraphQL ——
      两者是同一份数据，但 GraphQL 后端挂着时静态 JSON 照样能用。
      只认 `propertiesType == ItemPropertiesAmmo`：`types` 含 `ammo` 的**还包括手雷**
      ｜ ⚠️ **名字是英文 slug 而不是中文**：静态 JSON 的 `name` 是翻译键，
      只有 GraphQL 的 `lang: zh` 会解析它。用 `normalizedName`（`556x45mm-m855`）
      做检索与显示，`5.45 bp` / `m855` 都能命中。**拿到语言包后只需改一处名称来源**
- [x] **B4 / B5 查任务与更新任务库** —— `查任务 <片段>` + `更新任务`（管理员）
      ｜ 新表 `tarkov_task`（schema v7），同一套「静态 JSON + 本地表」管线
      ｜ 同时拉 `/regular/traders` 把商人的**裸 id** 换成可读 slug（`prapor`）——
      否则界面上只会出现一串十六进制。解析不到商人时留空而不是让整个导入失败
      ｜ ⚠️ 任务目标的**文字也是翻译键**，所以只存条数、不存文字

### 阶段 1 · 零成本批次 🟢

- [ ] **E6 摸鱼日历** —— `日历` ｜ 接口直接返图，下载 → 上传 → 发送，不需要模板
      ｜ ⛔ **上游全废（2026-09-29 实测）**，且都不是暂时故障：
      `api.52vmy.cn/api/wl/moyu` → 522（源站超时）；
      `api.vvhan.com/api/moyu` → 连接失败；
      `api.j4u.ink/.../moyu.json` → `{"code":403,"message":"接口更新"}`，
      同站日历图片路径返回公益 404 页。三个源都出自 cq-bot，无一可用。
      **等有可用源再接**，届时只需照 `daily.rs` 写一个下载转发的插件
- [ ] **B9–B15 静态图（这一条含 7 项）** —— `地图` / `任务流程图` / `任务物品图` /
      `信誉栏位图` / `boss丢包时间` / `3x4道具` / `耳机强度`
      ｜ **零代码**：准备好图片后用 `系统收录` 收进去即可

### 阶段 2 · 接口类 🔵


### 阶段 3 · 需要 resvg 模板 🟡

源项目这几项靠 Chromium 截图，本项目必须改写成模板 —— 这是**主要工作量**所在。

- [x] **B2 / B3 跳蚤市场** —— 合并成一条命令 `跳蚤 <片段|24位id>`，新表 `tarkov_item`（schema v8）
      ｜ **为什么合并**：源项目里 B2 走 `tarkov-market.com`（模糊搜索）、
      B3 走 `api.tarkov.dev` GraphQL（按 id 精确查），但两者都只是「查跳蚤价格」。
      而**两个上游都死了**（前者 403 Cloudflare + 加密载荷，后者 422）。
      静态 JSON 里 5442 件物品有 3525 件带价，一张表就能同时服务两种用法：
      参数是 24 位十六进制就出详情，否则当关键词搜。
      ｜ 缺价的物品存 **NULL 而不是 0** —— 0 会被用户读成「不值钱」
      ｜ ✅ **已实跑验证**：`cargo test -p qqbot-plugins --lib -- --ignored live_items`
      通过（5000+ 物品、3000+ 带价）
      ｜ 原上游细节存档：`tarkov-market.com` 带 Referer 返回 403 Cloudflare 挑战页；
      `api.tarkov-market.app` 返回 200 但载荷是加密串（`{"result":"ok","items":"JTVC..."}`）
- [ ] **C2 B 站动态 / 专栏** —— 消息含 `t.bilibili.com` / `opus` / `read`
      ｜ ⛔ **需要真实登录会话（2026-09-29 实测）**：
      `x/polymer/web-dynamic/v1/feed/space` 对**四个不同的 UID** 全部返回
      `code=0` 但 `items=[]`；补 `offset` / `timezone_offset` / `platform` / `features`
      参数无效；旧接口 `dynamic_svr/space_history` 已 404。
      cq-bot 硬编码的 buvid3 只够过风控（否则 412），不够取数据 ——
      取动态需要 `SESSDATA` 这类登录态。**除非提供 Cookie，否则不做**

### 阶段 4 · 外部依赖 🟠

接口稳定性与合规都不受控，按兴趣推进。

- [ ] **D1–D4 三角洲（集市 / 脑机 / 密码 / 一图流）** —— `三角洲\|df\|sjz` + 各子命令
      ｜ ⛔ **上游现在要求登录（2026-09-29 实测）**：
      `kkrb.net` 首页与 `getMenu` 都正常（`code=1`、`isLoggedIn=false`），
      但 `getOVData` 一律返回 `{"code":-101,"msg":"系统繁忙，请稍后再试"}`，
      补 `built_ver` / `globalData=true` 都无效。cq-bot 的代码早于这次改动
- [ ] **D5 永劫无间战绩** —— `永劫\|yj\|劫` + `战绩 <名>` ｜ `record.uu.163.com`
      ｜ ⛔ **接口已变（2026-09-29 实测）**：带 cq-bot 硬编码的 session 请求返回
      `404 {"code":1012,"msg":"not found"}`。而且那个 session 是**别人的账号**，
      即便路径对了也不该用 —— **需要你自己提供 Cookie**
- [ ] **F2 搜图 / 识图** —— `搜图` / `识图` 进模式 ｜ SauceNao + ascii2d
      ｜ ⛔ **两个上游都连不上（2026-09-29 实测）**：`saucenao.com` 与 `ascii2d.net`
      在本机都是 `fetch failed`（网络层失败，不是 HTTP 错误）。
      SauceNao 的 key 是 cq-bot 硬编码的公开 key，本来就随时可能失效

### 阶段 5 · 需要新建基础设施 🔴

- [ ] **C6 B 站订阅推送** —— 定时轮询 → 有新动态则推送到订阅群
      ｜ ⛔ **同 C2**：订阅表（C4/C5）已经建好，但取新动态依赖那个需要登录态的接口
      ｜ **本项目目前没有调度器**，需要先补：定时任务 + 订阅表 + 配额控制
      ｜ 群聊主动消息配额是 1000 条/群/天，日常够用，但必须有配额器

---

## 3.5 受阻项：解除条件与操作指引

下面这些**不是代码问题**，卡在外部条件上。每一行都写了「你能做什么」。

### B9–B15 塔科夫静态图（7 项）—— 等图片文件

**零代码。** 收录 → 关键词触发的整条链路已经实现并有 4 条端到端测试覆盖
（`resource_keyword_sends_the_file_passively` / `group_resource_is_invisible_to_other_groups` /
`system_resource_is_visible_everywhere` / `non_resource_keyword_falls_through`），
图片到位就能用。

文件名与关键词**照抄 cq-bot 的 `TarKovMapPlugin`**（`keywordToImageMap` + 各分支），
所以直接拿它原来的图就行。共 **19 个文件**。

#### 12 张地图

| 关键词 | cq-bot 文件名 |
|---|---|
| `储备站地图` | `tarkov_map/Reserve.jpg` |
| `灯塔地图` | `tarkov_map/Lighthouse.jpg` |
| `工厂地图` | `tarkov_map/Factory.jpg` |
| `海岸线地图` | `tarkov_map/Shoreline.jpg` |
| `海关地图` | `tarkov_map/Customs.jpg` |
| `街区地图` | `tarkov_map/StreetsOfTarKov.jpg` |
| `立交桥地图` | `tarkov_map/Interchange.jpg` |
| `森林地图` | `tarkov_map/Woods.jpg` |
| `实验室地图` | `tarkov_map/TheLab.jpg` |
| `疗养院地图` | `tarkov_map/ShorelineHose.jpg` |
| `中心区地图` | `tarkov_map/Center.jpg` |
| `迷宫地图` | `tarkov_map/Maze.jpg` |

#### 7 张静态图

| 关键词（= 整条消息） | cq-bot 文件名 |
|---|---|
| `任务流程图` | `tarkov_map/TaskProcess.jpg` |
| `任务物品图` | `tarkov_map/TaskItem.png` |
| `信誉栏位图` | `tarkov_map/reputation.png` |
| `boss刷新率` | `tarkov_map/bossRefreshRate.png` |
| `boss丢包时间` | `tarkov_map/bossLossWrap.png` |
| `3x4道具` | `tarkov_map/3x4.png` |
| `耳机强度` | `tarkov_map/headset.png` |

#### 收录命令

```
系统收录 海关地图 /opt/bot_img/tarkov_map/Customs.jpg
系统收录 任务流程图 /opt/bot_img/tarkov_map/TaskProcess.jpg
```

#### ⚠️ 与 cq-bot 的一处命令差异，必须知道

cq-bot 是 `地图 海关`，我们这里是 `海关地图`。原因是**关键词是单 token**：
`系统收录` 按空格切分参数，`地图 海关` 存不进去；
而资源触发的匹配是**整条消息精确相等**，不是子串包含。

cq-bot 那边是对整条消息做 `contains("地图")` + `contains("海关")`，
也就是**子串**匹配。照搬会带来误触发（「今天海关真难打」也会发图），
所以这里保留了精确匹配，改用 `海关地图` 这种关键词。

另外 7 张静态图**不需要 `地图` 前缀**，关键词就是命令本身，与 cq-bot 完全一致。


### B3 —— 卡在**本地化名称**，不是数据本身

`api.tarkov.dev` 的 GraphQL 后端自 2026-09-29 起对所有查询返回 422
（连 `{ __typename }` 都失败，而主站返回 200）。

**但数据并没有丢** —— 静态 JSON 一直在线，且 GraphQL 挂掉时它照样能用：

| 路径 | 大小 | 内容 |
|---|---|---|
| `https://json.tarkov.dev/regular/items` | 17 MB | 5442 件物品（含弹药、价格） |
| `https://json.tarkov.dev/regular/tasks` | 2.1 MB | 任务 |
| `https://json.tarkov.dev/regular/maps` | 8.6 MB | 17 张地图（含 BOSS 刷新率） |
| `https://json.tarkov.dev/regular/traders` | 50 KB | 商人 |

**真正缺的是本地化名称。** 静态 JSON 里的 `name` 是**翻译键**而不是文本，
形如 `"5448be9a4bdc2dfd2f8b456a Name"`；`?lang=zh` / `?locale=zh` 都无效。
只有 GraphQL 的 `lang: zh` 会把它解析成中文，而那个后端正是挂掉的那个。
站点的 i18next 语言包也没挂在可猜到的路径上（`/locales/**` 一律回落到 SPA 页面）。

**所以：数据有、名字没有。** 解除方式二选一 ——

1. 等 `api.tarkov.dev` 的 GraphQL 恢复（B4/B5/B6/B7 已改用静态 JSON 做完，
   恢复后能拿回**中文名**，届时只需改名称来源那一处）；
2. **导出 cq-bot 的 `bot.db`**，里面有现成的 `bullet`（2629 行）与
   `tkf_task` / `tkf_task_target`，**且是中文名**。
   这条路比等上游更可控，而且能一次解锁 B4 + B6。

如果接受英文/原始名，也可以用静态 JSON 直接做 —— 但翻译键形态的名字对用户没有意义，
所以没有按这条路实现。

### C2 / C6 —— 等 B 站登录 Cookie

`x/polymer/web-dynamic/v1/feed/space`（cq-bot 用的就是它，配硬编码 buvid3）
现在直接返回 **HTTP 412**：

```
错误号: 412  由于触发哔哩哔哩安全风控策略，该次访问请求被拒绝。
```

**对照实验说明问题在接口而不在网络**：同一时刻、同一台机器、同一个 UA，
`x/web-interface/view`（视频详情）返回 **200 正常数据**。
所以是**动态接口的风控更严**，不是网络不通。

（早期探测时它返回的是 `code=0` 但 `items=[]` —— 空数组而非报错，
比 412 更难判断；现在升级成明确的风控拒绝了。）

试过的替代路径，全部不通：
- 补 `b_nut` cookie：无变化
- 换四个不同的活跃 UID：无变化
- RSSHub 三个实例（`rsshub.app` / `rssforever` / `injahow`）：
  第一个连不上，另两个 **503**

**需要你提供带 `SESSDATA` 的 Cookie**（放进 `config.toml`，不要提交）。
只有登录会话才能过这层风控。

### D5 永劫无间 —— 等你的 Cookie

cq-bot 里那个 session 是**别人账号**的，且接口路径已变（返回 404）。
需要你自己登录后提供 Cookie。

### D1–D4 三角洲 —— 上游现在要求登录

`kkrb.net` 的 `getOVData` 一律返回 `-101 系统繁忙`，`isLoggedIn` 为 false。
需要该站的账号，或等它放开。

### F2 搜图 / E6 摸鱼日历 —— 上游已加反爬或已死

这两项**不建议再接**：
F2 的两个上游都连不上、E6 的三个源全部失效。
除非找到稳定且允许抓取的替代源，否则保持不做。

---

## 4. 每项完成的验收口径

按 AGENTS.md §9，一项功能算完成要同时满足：

1. `cargo clippy --workspace --all-targets` **退出码 0**，零警告；
2. `cargo test --workspace` 全绿、**退出码 0**；
3. 纯逻辑有单测（解析、优先级、权限这类）；
4. 涉及发送链路的，在 `tests/end_to_end.rs` 里用 mock 服务器加一条端到端断言；
5. 相关文档同步（README 指令表 / 协议文档 / 本文件勾选）。

---

## 5. 迁移时必须一并处理的约束

| 约束 | 说明 |
|---|---|
| **触发契约** | 全量模式下机器人能看到**所有**群消息。宽前缀（如 `地图`、`日历`）在日常聊天里很容易被碰到，**必须整串匹配** |
| **凭据外置** | 上游 token / key 一律进 `config.toml`，**绝不入库**（仓库是公开的） |
| **配额器** | 任何主动推送都要过 `SessionShard` 的配额校验；命令响应走被动回复，不占配额 |
| **表情回应替代** | 源项目用 `setGroupReaction("424")` 表示「处理中」，v2 没有这个接口，统一改成先回一条短文本 |
| **权限模型** | 群资源归本群管理员（`member_role`），系统资源归系统控制者，两个维度互不派生 |
| **发图必须上传** | v2 不能发远程 URL 或本地路径，一律走富媒体上传拿 `file_info` |

---

## 6. 已知的平台能力缺口

| 源项目用了 | v2 有没有 | 替代方案 |
|---|---|---|
| 表情回应 `424` | ❌ | 先回一条「处理中」文本 |
| 无 @ 全量监听 | ✅ 已开启 | 触发契约可以原样保留 |
| 合并转发 | ❌ | 图文或 markdown |
| 戳一戳 | ❌ | 不做 |
| 语音消息（silk） | ⚠️ 接口支持，但 Rust 侧无 silk 编码器 | F9 已列入不做 |
| Chromium 截图 | ❌ | 全部改 resvg 模板 |

反过来，v2 **有而源项目没用过**的能力：撤回自己的消息、群成员列表、禁言、
黑名单、入群审批、markdown、keyboard 按钮、引用回复。
