# ADR 0001：插件对外接口重构（A 簇）

- 状态：已接受
- 范围：本 ADR 只覆盖 A 簇（插件往外发的入口）。B 簇（核心运行时）与 C 簇（路由元数据）另开。
- 来源：一次只读架构审查（6 个 Strong 加深机会）。本文件记录**决定**，不记录实现细节。

## 背景

插件往外发东西有三条互相独立的路径，各自都把内部细节漏给了调用方：

1. **回复**没有自己的 module —— 「这次发送属于哪条消息/哪个事件」是唯一的被动凭证，却由 8 个 `Ctx::reply_*` 各自手工补一次；漏掉不会编译失败也不会运行失败，只会静默降级成主动消息、吃掉配额。
2. **卡片**契约把模板几何漏给了 4 个插件 —— 每个调用方都得知道首行 y=156、行距 48、页脚在 `height-24` 才能算对高度；算错不报错，只是页脚压住最后一行。
3. **对外 HTTP** 只有一个 18 行的 Client 构造器 —— UA、状态码、空体、大小上限、41 处中文错误文案全在每个插件里各写一遍，传输层失败零测试覆盖。

## 决定

### 范围与节奏

1. 分三轮：**A** 插件往外发的入口 → **B** 核心运行时（存储写时序、装配、actor 内部接缝）→ **C** 路由元数据。
2. 一条线一轮，一个候选一次提交，每轮结束仓库保持可发布、可推送。
3. 判据：**只做「加深」**（删除测试显示复杂度集中），不做「拆分」（只是把代码搬到别处）。

### A 簇的三个提交

4. **提交 1 —— `http.rs` 深模块**：它持有「对外 HTTP」这件事，接口是 `fetch_text` / `fetch_json` / `fetch_bytes` / `send`（后者给需要原始 `Response` 的两个调用点：delta 读 Set-Cookie、bili 取重定向后的最终 URL）；超时、UA、状态码、空体、大小上限与三种中文错误形状都收在内部。plugins 启用 `thiserror`（workspace 已有），`FetchError` 的 `Display` 就是给用户看的那句话。
   - 例外：`delta` 逐请求覆盖成**完整浏览器 UA**（kkrb 按 UA 拦），共享 client 上的默认 UA 不是给它的。这一条用 `cargo test -p qqbot-plugins --lib -- --ignored live_delta` 实跑验证过。

5. **提交 2 —— `Card` 类型**：落在 `qqbot-render`（几何属于模板）。调用方只给标题 / 行 / 页脚 / 宽度，高度由 `Card` 自己算；「分段行」是行的一个变体，调用方不再需要知道 `section: true` 这个键，也不再需要选 `card.svg` 还是 `sections.svg`。通用 `render_template(name, data)` 留在 `RenderService` 上供 self-test 与未来模板使用，**不进插件面**。
6. **提交 3 —— `Reply` module**：落在 `crates/qqbot-core/src/reply.rs`，`Reply` 持有 `Arc<Services>` + target + 凭证，凭证在**构造它**时确定（`for_message` / `for_event` 两种构造；`for_event(None)` 退化成主动消息）。凭证只在私有 `attach(request, credential)` 一处挂上去，每个语义方法都走它。**删掉 `Ctx` 的 8 个 `reply_*`**，`Ctx` 改持有一个用消息凭证构造的 `pub reply: Reply`；18 个插件机械改成 `ctx.reply.text(...)`。
   `Services` 的字段收成 `pub(crate)`，公开面只剩 `new` / `with_store` / `bot_name` / `set_bot_name`；`send` / `render_card_png` / `render_svg_png` / `upload_png` / `upload_image` 降为 `pub(crate)`，**只给 `Reply` 用**（`render_template_png` 已在提交 2 删除）。
   - 订正：这几个方法本来就不在 `Ctx` 上，而在 `Services` 上；`Ctx` 要删的只有 8 个 `reply_*`。它们也不是「零调用」——`Reply` 的语义方法全靠它们（`upload_image` 原本确实没人调，本次由 `Reply::image_named` 接手）。本次只是把可见性降成 `pub(crate)`，把「只有 `Reply` 能发」交给编译器管。
   `InteractionCtx` 保留（`button_data` / `sender_openid` 是消息上下文没有的），内部持有一个用事件凭证构造的 `Reply`，并删掉它的 `send_markdown` / `send_markdown_with_keyboard` / 私有 `send`。`Reply` **不留** `send(SendRequest)` 逃生口 —— 真需要时再加方法比现在留一个没人用的口子安全。

### 用户可见的变化（已确认）

7. **卡片宽度统一 1000**（原本 980 / 900 / 1000 / 1000）—— 用户明确选择接受的**外观变化**。页脚的规则是：**默认 `qqbot`，只有承载信息时才显式覆盖**。去掉 market 的装饰性页脚（「价格单位：卢布 · 数据来自 tarkov.dev」），保留三处承载信息的：番剧的「共 N 部 / 仅显示前 M 部」（截断披露）、三角洲密码与一图流的「更新于 <日期>」（数据新鲜度）、三角洲集市的「<活动名> · <活动时间>」（价格对应哪个活动）。
   - 这条实施时一度做过头：删页脚连带把 `PasswordEntry.updated`、`MarketItem.activity_name` / `activity_time` 从**解析模型**里删掉了。表现层的决定不该动数据模型 —— 已回退。判据记在这里，免得下次再犯。
8. **HTTP 失败话术统一**：去掉「接口 / 下载 / 页面」这类前缀（调用点自己的前缀已说明上下文），保留各插件的语义前缀（`下载失败：` / `查询失败：` / `日报获取失败：` …）。
9. **`tarkov` 的失败话术不变**：它故意不把上游状态码给用户（见下）。

### 渲染层零调用面

10. 留 `notice.svg`（self-test 覆盖，且是唯一使用 `body_lines` 的样本）；删 `TemplateEngine::render_source` 与 `TemplateEngine::add_template` —— 两者全仓零调用，留成私有只会触发 `dead_code`（零警告是硬要求）。`new()` 内部直接用 `env.add_template`，不受影响。

### 数据库（为 B 簇预先定下）

11. 本次重构**允许重建数据库**。B 簇可以加表、加列、改索引、改已有列的数据语义；`messages` 表的迁移**不再维护**，`upgrade()` 里为它写的旧 `ALTER` 分支可以删；检测到旧版本时**明确报错并提示删库**，不要静默半残。
    - 实施时的订正：`upgrade()` 与它的三个辅助函数（`add_column_if_missing` / `drop_column_if_present` / `has_column`）**整个删掉**了，不只是 `messages` 的那几个分支 —— 版本**不等**（含更新的库）一律报错，加表加列只改 `DDL` 并提 `SCHEMA_VERSION`。版本判定发生在执行 `DDL` **之前**：否则旧库会被顺手建上一堆形状不一致的新表，报错之后更没人敢碰。
12. 重建会丢的东西（已确认可丢）：消息历史（词云语料，跑几天就回来）、`tarkov_*`（三条 `更新*` 命令可重导）、`captions`（临时）。`settings` 里的系统控制者可由 `QQBOT_SYSTEM_CONTROLLERS` 恢复。

## 明确不做（避免后续审查重提）

13. **不拆大文件**：`task.rs` 1801 行、`store/resource.rs` 1811 行、`end_to_end.rs` 2772 行、`qqbot-api` 的 692 个 pub 项 —— 长不等于浅，这些不在目标里。（`ResourceStore` 那个 33 方法跨 7 个领域的接口是「宽」不是「长」，留给 B 簇连同存储时序一起看。）
14. **保留零消费者的键盘 / 按钮链路**：`Keyboard` / `Button` / `InteractionCtx` / `Handler::on_interaction` 目前没有生产消费者，但这是迁移清单里明确有意保留的能力（v2 的 `keyboard` 已实现，可作独立小功能重做）。本次只把凭证收进 `Reply`。
15. **保留 `tarkov` 的固定失败串**：不把上游状态码丢给用户是刻意行为 —— `boss_chance_reports_upstream_failure_gracefully` 断言回复里含「查询失败」且**不含 `422`**。审查期间曾把它误判为「与其余 13 处不一致」，已纠正。
16. **不动网关状态机**（`crates/qqbot-gateway/src/actor.rs`）：492 行 0 单测看着刺眼，但规则已被 `crates/qqbot-gateway/tests/gateway_protocol.rs` 的真实 mock WebSocket 覆盖（含 Resume 的 `session_id` 与 `seq` 断言）；393 行 mock 是「IO 必须真连」的代价，状态也已独占在一个 actor 里。
17. **不建 `GLOSSARY.md`**：领域词汇已经写在 AGENTS.md 与协议文档里，再开一份只会多一个漂移源。
18. **不改协议不变量**（AGENTS.md §5）、**不新增第四类 actor**（AGENTS.md §4）、**不改命令的触发词与别名**（只允许改文案）。

## 后果

- 插件作者要学的东西变少：一个 `ctx.reply`、一个 `Card`、四个 `fetch_*`。
- 每个新接缝至少有一条**进程内**测试（不起 socket、不起 mock 服务器）；传输层失败从零覆盖变成有覆盖。
- 卡片外观会变（宽度统一），这是本轮唯一有意的用户可见回归面。

## 修订：评审修复轮

1. **第 4 条的接口面多了一个 `send_raw`**：`http::send` 对非 2xx 返回 `Err`，
   而 delta 的握手预热**只为了拿 `Set-Cookie`** —— 站点在 403 / 302 上照样下发会话标记，
   状态码闸门会把整段握手掐掉。改造前那里是裸 `.send()`，所以这是本次重构
   **无意引入的退化**（代码注释还写着「与改造前一致」，已经改成描述真实实现）。
   现在拆成 `send_raw`（只映射传输层失败）/ `send`（含状态码闸门），
   并补了一条回环测试固定「403 仍能拿到响应头」。
2. **`fetch_json` 有了生产调用点**：`ba.rs` 原来手写了等价的 `fetch_text` + `from_str`
   （连解析失败映射成的 `FetchError::Parse` 都一样），改用 `fetch_json` 后它不再只有测试在调。
3. **`fetch_bytes` 改成边读边数**：原来先 `res.bytes()` 把整个响应体读进内存再查上限，
   挡不住声明长度不实的上游。现在先看 `Content-Length` 提前退出，再按块累计。
4. **收录失败的错误链不再被吃掉**：`resource.rs` 原来用 `format!("收录失败：{err}")`，
   而 anyhow 的 `Display` 只印最外层，`.context("下载附件失败")` 会把
   「下载内容过大 / 返回 HTTP 403」这些真正的死因盖住。改成 `{err:#}`。
