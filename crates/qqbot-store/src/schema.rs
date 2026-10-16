//! 建表、PRAGMA 与版本迁移。

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// 当前 schema 版本，记录在 `meta` 表里。
///
/// v2 增加了资源映射（`resources` / `resource_keywords`）与系统设置（`settings`）。
/// v3 给 `messages` 增加 `raw` 列，保存**原始事件 JSON**。
/// v4 增加 `bili_subscriptions`（B 站订阅）。
/// v5 增加 `pending_captions`（图语：等下一张图配字）。
/// v6 增加 `ammo`（塔科夫弹药数据，从 tarkov.dev 静态 JSON 导入）。
/// v7 增加 `tarkov_task`（塔科夫任务数据，同一来源）。
/// v8 增加 `tarkov_item`（塔科夫物品与跳蚤价格，同一来源）。
/// v9 给 `tarkov_task` 加 `name_zh`（GraphQL `lang: zh` 的中文名）。
/// v10 重写任务：`tarkov_task` 补图/地图/商人中文名，去掉 `objectives` 计数列，
/// 并拆出 `tarkov_task_objective` / `tarkov_task_reward` / `tarkov_task_prereq`。
/// v11 塔科夫全模块改走**静态语言包**（`json.tarkov.dev/regular/<资源>_zh`）：
/// 新增 `tarkov_task_successor`（由 `taskRequirements` 反转得到），
/// 任务补英文名/地图中文名/导入时间，目标补 `detail_json`，
/// `ammo` 与 `tarkov_item` 补 `name_zh`。
/// v12 去掉 `detail_json` 这条「结构化字段塞进 JSON、渲染时再解析回来」的链路。
///
/// 它的问题不是性能，而是**语义在两端各实现一次**：导入时把字段序列化成 JSON，
/// 渲染时又 `from_str::<Value>(..).ok()` 解析回来，于是「数量该摆在正文还是块引用」
/// 「×1 要不要显示」这类判断散落在渲染函数里，每加一条排版规则就多一个 `if`。
/// v12 把这条链路整条删掉：
///
///   - 目标的**正文与标记在导入时拼好**（`display_text` / `marks`），
///     渲染层拿到什么印什么，不再判断数量与标记该怎么摆；
///   - 任务级的阵营 / 可重复接取落成列，需要钥匙 / 商人要求 / 失败条件落成三张子表。
///     名字类字段一律在导入时用语言包解析好 —— 它们是数据，不是状态快照。
///
/// 这与项目早就定下的做法一致：`name_zh` 也是导入时解析好存下来的，
/// 而不是渲染时再查语言包。
///
/// v12 升级时清空过塔科夫缓存 —— 正文只能由上游数据重算，库里的行救不回来。
/// 它是上游的副本，系统控制者发一次「更新任务」就重建。
///
/// v13 增加 `message_attachments`（附件落成行）。
/// v14 **删掉它**：那张表唯一的读者是「收录」的第 3 级回查（回消息库找最近一条带附件的
/// 消息），而这一级在 v14 被整条移除 —— 收录改为只认「本条消息自带的附件」与
/// 「被引用消息的附件」。一张没有读者的表就是一处没有测试面的状态，而它正是
/// 「按 `(created_at, ordinal)` 取最新」那个排序 bug 的栖身之处。
///
/// 从 v13 起**不再迁移旧库**：版本不等一律报错要求删库重建，理由见 `migrate`。
pub const SCHEMA_VERSION: i64 = 14;

const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS messages (
    id          TEXT PRIMARY KEY,
    scope       TEXT NOT NULL CHECK (scope IN ('group','c2c')),
    target_id   TEXT NOT NULL,
    sender_id   TEXT,
    sender_name TEXT,
    event_name  TEXT NOT NULL,
    content     TEXT NOT NULL,
    -- 原始事件 JSON。类型里只声明了文档写到的字段，**没声明的会在
    -- 反序列化时丢掉** —— 附件、引用、聊天记录恰恰是文档最含糊的部分。
    -- 存原文是为了将来能重新提取，而不是只能靠当时解析出的那几个字段。
    raw         TEXT,
    created_at  INTEGER NOT NULL
);

-- 词云查询走 (scope, target_id, created_at)
CREATE INDEX IF NOT EXISTS idx_messages_scope_target_time
    ON messages(scope, target_id, created_at DESC);

-- 保留期清理走 created_at
CREATE INDEX IF NOT EXISTS idx_messages_created_at
    ON messages(created_at);

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- 资源映射。只存**路径**，素材留在磁盘上：换图直接替换文件即可，数据库不用动。
CREATE TABLE IF NOT EXISTS resources (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    -- group = 某群自建，只在该群可见；system = 全局可见
    scope       TEXT    NOT NULL CHECK (scope IN ('group','system')),
    -- 群资源存 group_openid；系统资源存**空串**。
    -- 不能用 NULL：SQLite 的 UNIQUE 索引把 NULL 视为互不相等，
    -- 那样下面的三列主键就拦不住重复关键词了。详见 resource.rs。
    owner_id    TEXT    NOT NULL,
    name        TEXT    NOT NULL,
    -- 素材文件路径（收录时已解析成绝对路径）
    path        TEXT    NOT NULL,
    -- 发送时给平台判格式用
    file_name   TEXT    NOT NULL,
    -- 官方富媒体 FileType：1 图片 / 2 视频 / 3 语音 / 4 文件
    file_type   INTEGER NOT NULL,
    description TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    UNIQUE (scope, owner_id, name)
);

-- 触发词。主名也在其中，这样「同一作用域内关键词唯一」由主键保证，
-- 而不是靠应用层自觉 —— 两个资源抢一个词必须在 INSERT 时就失败。
CREATE TABLE IF NOT EXISTS resource_keywords (
    keyword     TEXT    NOT NULL,
    scope       TEXT    NOT NULL CHECK (scope IN ('group','system')),
    owner_id    TEXT    NOT NULL,
    resource_id INTEGER NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
    PRIMARY KEY (keyword, scope, owner_id)
);

CREATE INDEX IF NOT EXISTS idx_resource_keywords_resource
    ON resource_keywords(resource_id);

-- 通用系统设置（键值）。
CREATE TABLE IF NOT EXISTS settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- B 站订阅。按**群**订阅 UP 主，与源项目一致。
CREATE TABLE IF NOT EXISTS bili_subscriptions (
    uid        TEXT NOT NULL,
    group_id   TEXT NOT NULL,
    -- UP 主昵称。订阅时查过一次接口，顺手存下来，推送时不必再查。
    name       TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    PRIMARY KEY (uid, group_id)
);

-- 图语：`图语 <文本>` 之后，等同一会话里同一个人发的下一张图。
-- 一次性，用完即删；过期判断在读取时做。
CREATE TABLE IF NOT EXISTS pending_captions (
    target_id  TEXT NOT NULL,
    sender_id  TEXT NOT NULL,
    text       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (target_id, sender_id)
);

-- 塔科夫弹药。字段对齐 cq-bot 的 `bullet` 表（MIT），但只保留
-- 静态 JSON 里真实存在、且玩家真正会看的那几项。
CREATE TABLE IF NOT EXISTS ammo (
    id                  TEXT PRIMARY KEY,
    -- 可读的 slug（`556x45mm-m855`）。静态 JSON 里的 `name` 是**翻译键**
    -- （形如 `<id> Name`），由 `regular/items_zh` 语言包解析成中文。
    normalized_name     TEXT NOT NULL,
    -- 语言包里的中文名。解析不到时为 NULL，检索与显示退回 slug。
    name_zh             TEXT,
    caliber             TEXT NOT NULL DEFAULT '',
    damage              INTEGER NOT NULL DEFAULT 0,
    penetration_power   INTEGER NOT NULL DEFAULT 0,
    armor_damage        INTEGER NOT NULL DEFAULT 0,
    fragmentation_chance REAL NOT NULL DEFAULT 0,
    initial_speed       INTEGER NOT NULL DEFAULT 0,
    projectile_count    INTEGER NOT NULL DEFAULT 1,
    tracer              INTEGER NOT NULL DEFAULT 0,
    base_price          INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_ammo_normalized_name ON ammo(normalized_name);

-- 塔科夫任务。对齐 cq-bot 的 `tkf_task`（MIT），但**不照抄它的两处做法**：
--
--   1. 它的 `id` 是自增整数，而每次更新都是 `remove(null)` 全删重插 ——
--      id 会漂，`pre_task_id` 里存的就是旧 id，跨版本直接失效。
--      这里用 24 位 hex 的**上游 id**，重导多少次都不变。
--   2. 它把奖励与前置任务**渲染成 Markdown 字符串**塞进 `finish_reward` /
--      `pre_task` 两列。查询时零计算，但改了渲染格式要全量重导，
--      而且 `pre_task` 里的「(进行中)/(完成)」是**导出那一刻**的快照，
--      之后永远不会更新。这里拆成下面三张表。
CREATE TABLE IF NOT EXISTS tarkov_task (
    id              TEXT PRIMARY KEY,
    -- 可读 slug（`gunsmith-part-1`）。与弹药同理：`name` 是翻译键。
    normalized_name TEXT NOT NULL,
    -- 中文名，来自 `regular/tasks_zh` 语言包的 `<id> name`。
    -- 实测 515/515 命中，导入时**强制非空**；列仍可空只为兼容旧库。
    name_zh         TEXT,
    -- 英文名（`The Punisher - Part 1`），从 `normalizedName` 反推。
    name_en         TEXT,
    -- 商人的可读 slug（`prapor`），由 traders 表解析而来。
    trader          TEXT NOT NULL DEFAULT '',
    -- 商人的中文名与头像。头像来自静态 JSON 的 `imageLink`，
    -- 是渲染卡片时要用的（cq-bot 也存了）。
    trader_name_zh  TEXT,
    trader_image    TEXT NOT NULL DEFAULT '',
    -- 任务自己的图标。
    task_image      TEXT NOT NULL DEFAULT '',
    -- 任务发生的地图 slug，来自 `map` 字段。
    map             TEXT NOT NULL DEFAULT '',
    -- 地图中文名（`regular/maps_zh` 的 `<mapId> Name`）。
    map_name_zh     TEXT,
    min_level       INTEGER NOT NULL DEFAULT 0,
    is_kappa        INTEGER NOT NULL DEFAULT 0,
    is_lightkeeper  INTEGER NOT NULL DEFAULT 0,
    experience      INTEGER NOT NULL DEFAULT 0,
    wiki_link       TEXT NOT NULL DEFAULT '',
    -- 阵营（USEC / BEAR）。上游的 `Any` 在导入时就被过滤掉，存进来的一定有意义。
    faction         TEXT,
    -- 是否可重复接取。
    restartable     INTEGER NOT NULL DEFAULT 0,
    -- 本次导入的 unix 秒。卡片底部显示「数据更新于」，也用于判断是否需要重导。
    updated_at      INTEGER NOT NULL DEFAULT 0
);

-- 任务目标。对应 cq-bot 的 `tkf_task_target`，三处改进：
--
--   1. 用**稳定的 `task_id`** 而不是自增整数 `parent_id`；
--   2. 显式存 `ordinal` —— 它靠插入顺序，而 `saveBatch` 不保证顺序；
--   3. 把「翻译键」与「解析出的中文」分成两列。静态 JSON 里 `description`
--      就是目标自己的 id（一个翻译键），由 `regular/tasks_zh` 解析。
--      实测 1441/1441 命中；万一新目标还没进语言包，
--      渲染退回按 `objective_type` 写的通用文案，而不是显示一串 hex。
CREATE TABLE IF NOT EXISTS tarkov_task_objective (
    task_id         TEXT NOT NULL,
    ordinal         INTEGER NOT NULL,
    -- 上游的 `type`（`visit` / `shoot` / …）。渲染已经不用它，
    -- 留着是排障：能看出某条目标是「语言包缺了」还是「上游换了类型」。
    objective_type  TEXT NOT NULL DEFAULT '',
    -- 语言包里的翻译键（实测就是目标自己的 id）。覆盖率统计与重新解析要用。
    description_key TEXT NOT NULL DEFAULT '',
    -- **导入时拼好的正文**：「在海关使用 AKS-74U 消灭 Scav ×25」。
    -- 数量并进正文、×1 省掉、语言包缺失时退回按 `objective_type` 写的通用文案 ——
    -- 全都在导入时决定一次。渲染层拿到什么印什么，不再判断数量该怎么摆。
    display_text    TEXT NOT NULL DEFAULT '',
    -- **导入时拼好的附加标记**，`|` 分隔（`可选|战局内|21:00-6:00`），空串表示没有。
    -- 分隔符用 `|` 而不是 ` · `：标记怎么呈现是排版的事 ——
    -- 目标少时走块引用，目标多时整段塞进代码块、标记只能写在行内。
    marks           TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (task_id, ordinal)
);

-- 任务奖励。cq-bot 把它渲染成文本塞进 `finish_reward`，这里拆成行。
CREATE TABLE IF NOT EXISTS tarkov_task_reward (
    task_id     TEXT NOT NULL,
    ordinal     INTEGER NOT NULL,
    -- standing（商人声望）/ item（物品）/ skill（技能）/ unlock（解锁）。
    kind        TEXT NOT NULL,
    -- 商人或物品的上游 id，用来在解析出中文名后回填。
    ref_id      TEXT NOT NULL DEFAULT '',
    name_zh     TEXT,
    -- 声望是小数（0.1），物品是整数，用 REAL 通吃。
    amount      REAL NOT NULL DEFAULT 0,
    -- 结构化补充：工艺解锁的 `{station, level}`、起始奖励标记等。
    extra       TEXT,
    PRIMARY KEY (task_id, ordinal)
);

-- 前置任务。cq-bot 存 `pre_task_id`（`|` 分隔的**旧自增 id**）
-- 与 `pre_task`（带状态的中文文本快照）。这里两者都结构化，
-- 状态可以每次刷新，而不是永远停在导出那一刻。
CREATE TABLE IF NOT EXISTS tarkov_task_prereq (
    task_id     TEXT NOT NULL,
    ordinal     INTEGER NOT NULL,
    prereq_id   TEXT NOT NULL,
    -- complete / active / failed。上游给的是数组，这里拍平成逗号分隔。
    status      TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (task_id, ordinal)
);

-- 后续任务。上游**没有**这个方向的数据，由 `taskRequirements` 反转得到：
-- A 的前置里有 B ⇒ B 的后续里就有 A。
-- 单独建表而不是查询时反转：卡片要显示「后续任务」，而反转得扫全表的前置。
CREATE TABLE IF NOT EXISTS tarkov_task_successor (
    task_id      TEXT NOT NULL,
    ordinal      INTEGER NOT NULL,
    successor_id TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (task_id, ordinal)
);

-- 任务需要钥匙。一行 = 一张地图 + 这张地图上要的钥匙。
-- 钥匙名在导入时就用语言包解析好、`、` 连接 —— 渲染层只管加 `- `。
CREATE TABLE IF NOT EXISTS tarkov_task_key (
    task_id  TEXT NOT NULL,
    ordinal  INTEGER NOT NULL,
    map_name TEXT NOT NULL DEFAULT '',
    keys     TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (task_id, ordinal)
);

-- 商人的接取门槛。一行 = 一句人话（「大老板 忠诚等级 >= 2」），导入时拼好。
CREATE TABLE IF NOT EXISTS tarkov_task_requirement (
    task_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    text    TEXT NOT NULL,
    PRIMARY KEY (task_id, ordinal)
);

-- 失败条件。一行 = 一个会把任务判失败的任务 + 触发它的状态。
CREATE TABLE IF NOT EXISTS tarkov_task_fail (
    task_id   TEXT NOT NULL,
    ordinal   INTEGER NOT NULL,
    task_name TEXT NOT NULL,
    status    TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (task_id, ordinal)
);

CREATE INDEX IF NOT EXISTS idx_tarkov_task_name ON tarkov_task(normalized_name);
CREATE INDEX IF NOT EXISTS idx_tarkov_task_name_zh ON tarkov_task(name_zh);
CREATE INDEX IF NOT EXISTS idx_tarkov_task_prereq_pre ON tarkov_task_prereq(prereq_id);
CREATE INDEX IF NOT EXISTS idx_tarkov_task_successor_succ
    ON tarkov_task_successor(successor_id);

-- 塔科夫物品与跳蚤价格。价格字段可为 NULL —— 实测 5442 件里只有 3525 件有价。
CREATE TABLE IF NOT EXISTS tarkov_item (
    id              TEXT PRIMARY KEY,
    -- 可读 slug（`colt-m4a1-556x45-assault-rifle`）。理由同 ammo / tarkov_task。
    normalized_name TEXT NOT NULL,
    -- 语言包里的中文名（`regular/items_zh` 的 `<id> Name`）。
    name_zh         TEXT,
    base_price      INTEGER NOT NULL DEFAULT 0,
    -- 跳蚤市场价：最低挂牌 / 24 小时均价 / 24 小时最低 / 24 小时最高。
    last_low_price  INTEGER,
    avg24h_price    INTEGER,
    low24h_price    INTEGER,
    high24h_price   INTEGER,
    weight          REAL NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_tarkov_item_name ON tarkov_item(normalized_name);
"#;

/// 打开（必要时创建）数据库并应用 PRAGMA。
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() && !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建数据库目录 {} 失败", parent.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("打开数据库 {} 失败", path.display()))?;
    apply_pragmas(&conn)?;
    Ok(conn)
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    // WAL：读不阻塞写、写不阻塞读 —— 这是「读写分两个线程」得以成立的前提。
    // journal_mode 会返回结果行，必须用 query_row 而不是 pragma_update。
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .context("设置 journal_mode=WAL 失败")?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(mode, "数据库未能启用 WAL，并发读写性能会下降");
    }

    // NORMAL：WAL 下崩溃最多丢最后一个事务，换来数量级的写入吞吐提升。
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", 5_000i64)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // 刻意不设 `auto_vacuum`。实测（sqlite 3.53.1）：在 `journal_mode=WAL` 之后设它会被
    // **静默忽略**（`PRAGMA auto_vacuum` 读回 0），而即便设对了，一次
    // `PRAGMA incremental_vacuum` 也只回收 **1 页** —— 空 1977 页要调 1977 次。
    // 更要紧的是真实库里 `freelist_count` 恒为 0：保留期删掉的页立刻被新消息复用，
    // 文件在「一个保留期的高水位」处就稳定了，本来就没有空闲页可回收。
    // 缩容手段是手动 `VACUUM`（或按 ADR 0001 第 11 条删库重建），不是这个 pragma。
    Ok(())
}

/// 表存不存在。用于在**建表之前**判断这是不是一个全新的库。
fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let found: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get(0),
    )?;
    Ok(found > 0)
}

/// 建表并写入 schema 版本。三条规则：
///
/// 1. **新库**（连 `meta` 表都没有）→ 建表 + 写版本 + 播种默认资源，三件事一个事务；
/// 2. 版本**恰好相等** → 直接用，什么都不做；
/// 3. 版本**不等**（旧库或更新的库）→ 返回明确错误，**不建表、不迁移**；
/// 4. `meta` 表在、版本行不在 → 也返回明确错误（见下），**不建表**。
///
/// 第 3 条是 ADR 0001 第 11 条的授权结果：本次重构允许重建数据库，
/// 所以加表加列只需要改 `DDL` 并提 `SCHEMA_VERSION`，不必再维护跨版本的
/// `ALTER` / `DROP` 分支。宁可让用户删掉 `data/qqbot.db` 重启，
/// 也不要留下「升了一半」的库 —— 那种半残状态既没有测试能覆盖，
/// 报出来的错也指不到真正的原因。
///
/// ⚠️ 版本判定必须发生在执行 `DDL` **之前**：否则旧库会被顺手建上新表，
/// 之后即使报错，库里也已经多出一堆形状不一致的表。
pub fn migrate(conn: &Connection) -> Result<()> {
    // ⚠️ 不能用 `.ok()` 把三种语义压成同一个 None：
    //   (a) 行不存在     → 正常首次初始化，走新库分支
    //   (b) 查询失败     → 真实错误，必须上报
    //   (c) 值不是整数   → 数据损坏，必须上报
    // (b)/(c) 若混进 None 分支，会撞上 meta.key 主键约束，报出一条毫无指向性的
    // "UNIQUE constraint failed"，进而让 main 静默把整个持久化降级掉。
    //
    // ⚠️ 「`meta` 表在、版本行不在」**不是**新库。它只可能来自手改数据库，或
    // 「建表成功、写版本行之前进程退出」。当成新库的后果很隐蔽：`CREATE TABLE
    // IF NOT EXISTS` 修不了已存在表的形状，版本却被写成当前值 —— 库看起来一切正常，
    // 直到运行期报 `no such column`，而写线程对 SQL 失败只 warn 后丢批
    // （消息一条不落库，日志里只有一行 warn）。所以这里明确报错。
    let current: Option<i64> = if !table_exists(conn, "meta")? {
        None
    } else {
        match conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get::<_, String>(0),
        ) {
            Ok(v) => Some(
                v.parse()
                    .with_context(|| format!("meta.schema_version 不是整数: {v:?}"))?,
            ),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                anyhow::bail!(
                    "数据库里有 meta 表却没有 schema_version 行，无法判断版本。\
                     删掉 data/qqbot.db 后重启即可重建"
                );
            }
            Err(err) => return Err(err).context("读取 schema_version 失败"),
        }
    };

    match current {
        Some(v) if v == SCHEMA_VERSION => return Ok(()),
        Some(v) => {
            anyhow::bail!(
                "数据库 schema 版本 {v} 与本程序要求的 {SCHEMA_VERSION} 不一致。\
                 本次重构（ADR 0001 第 11 条）不再迁移旧库：删掉 data/qqbot.db 后重启即可重建"
            );
        }
        None => {}
    }

    // 建表、写版本、播种必须在**同一个事务**里。否则「建了表却没写版本行」这个半残
    // 状态就能由崩溃产生 —— 而它正是上面那条报错要挡住的东西。
    // 用 `unchecked_transaction`：`migrate` 只拿得到 `&Connection`。
    let tx = conn.unchecked_transaction().context("开启建库事务失败")?;
    tx.execute_batch(DDL).context("建表失败")?;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [SCHEMA_VERSION.to_string()],
    )?;
    // 只在**首次建库**时播种。之后控制者清空列表不会让默认值复活。
    crate::resource::seed_defaults(&tx)?;
    tx.commit().context("提交建库事务失败")?;
    tracing::info!(version = SCHEMA_VERSION, "数据库 schema 已初始化");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 版本不符（旧库或更新的库）必须**原地报错**，且不得把新表建进旧库。
    ///
    /// 「不建表」是这个新规则的一半：如果先执行 `DDL` 再判版本，
    /// 旧库会被顺手建上一堆形状不一致的新表 —— 报错之后它更没人敢碰了。
    #[test]
    fn mismatched_version_errors_without_creating_tables() {
        for v in [SCHEMA_VERSION - 1, SCHEMA_VERSION + 1] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta(key, value) VALUES('schema_version', '{v}');"
            ))
            .unwrap();

            let err = format!("{:#}", migrate(&conn).unwrap_err());
            assert!(err.contains("删掉 data/qqbot.db"), "版本 {v} 应提示删库重建: {err}");
            assert!(!table_exists(&conn, "messages").unwrap(), "版本 {v} 时不该建表");
        }
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let v: String = conn
            .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION.to_string());
        // 表必须建出来 —— 重复调用不会把它建第二遍（IF NOT EXISTS）。
        assert!(table_exists(&conn, "messages").unwrap());
    }

    /// 回归：「`meta` 表在、版本行不在」**不是**新库。
    ///
    /// 当成新库的后果很隐蔽：`CREATE TABLE IF NOT EXISTS` 修不了已存在表的形状，
    /// 版本却被写成当前值 —— 库看起来一切正常，直到运行期报 `no such column`。
    #[test]
    fn meta_without_version_row_is_an_error_not_a_fresh_db() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();

        let err = format!("{:#}", migrate(&conn).unwrap_err());
        assert!(err.contains("删掉 data/qqbot.db"), "应提示删库重建，实际: {err}");
        // 这段文案由多行字面量拼成：丢掉行尾的续行反斜杠会把缩进原样印进报错，
        // 症状是运维看到句子中间二十来个空格。测试钉住「渲染出来的文案是干净的」。
        assert!(!err.contains("  "), "报错文案里不该有连续空格（续行反斜杠丢了？）: {err}");
        assert!(!table_exists(&conn, "messages").unwrap(), "不该把表建进这个库");
    }

    /// 回归：损坏的 schema_version 必须报出**真实原因**。
    ///
    /// 修复前 `.ok()` 会把「查询失败」和「值不是整数」一起吞成 None，
    /// 于是走进裸 INSERT 撞上 meta.key 主键约束，最终只报一句
    /// "UNIQUE constraint failed" —— 而 main 会因此静默把整个持久化降级掉。
    #[test]
    fn corrupt_schema_version_reports_a_real_error() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "UPDATE meta SET value = '不是数字' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();

        let err = format!("{:#}", migrate(&conn).unwrap_err());
        assert!(err.contains("不是整数"), "应指出真实原因，实际: {err}");
        assert!(!err.contains("UNIQUE"), "不应退化成主键冲突: {err}");
    }

    #[test]
    fn schema_check_constraint_rejects_unknown_scope() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let r = conn.execute(
            "INSERT INTO messages(id, scope, target_id, event_name, content, created_at)
             VALUES('M1','channel','X','E','c',0)",
            [],
        );
        assert!(r.is_err(), "scope 只允许 group/c2c");
    }
}
