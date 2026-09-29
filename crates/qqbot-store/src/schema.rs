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
pub const SCHEMA_VERSION: i64 = 7;

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
    -- 可读的 slug（`556x45mm-m855`）。静态 JSON 里的 `name` 是**翻译键**，
    -- 只有 GraphQL 的 `lang: zh` 能解析它，所以这里用 normalizedName 做检索与显示。
    normalized_name     TEXT NOT NULL,
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

-- 塔科夫任务。字段对齐 cq-bot 的 `tkf_task`（MIT），但只保留静态 JSON
-- 里真实有值的项 —— 它没有 `finish_reward` / `pre_task` 这些。
CREATE TABLE IF NOT EXISTS tarkov_task (
    id              TEXT PRIMARY KEY,
    -- 可读 slug（`gunsmith-part-1`）。与弹药同理：`name` 是翻译键。
    normalized_name TEXT NOT NULL,
    -- 商人的可读 slug（`prapor`），由 traders 表解析而来。
    trader          TEXT NOT NULL DEFAULT '',
    min_level       INTEGER NOT NULL DEFAULT 0,
    is_kappa        INTEGER NOT NULL DEFAULT 0,
    is_lightkeeper  INTEGER NOT NULL DEFAULT 0,
    experience      INTEGER NOT NULL DEFAULT 0,
    -- 目标条数。目标的**文字**也是翻译键，所以只存个数。
    objectives      INTEGER NOT NULL DEFAULT 0,
    wiki_link       TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_tarkov_task_name ON tarkov_task(normalized_name);
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
    // 删除大量历史消息后可按需回收空间。必须在建表前设置，否则要 VACUUM 才生效。
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    Ok(())
}

/// 逐版本升级**已存在的表**。
///
/// `DDL` 里的 `CREATE TABLE IF NOT EXISTS` 只能建新表；
/// 给已存在的表**加列**必须显式 ALTER。这一步最容易漏 ——
/// 新库一切正常，旧库直到读那一列时才报 `no such column: raw`。
fn upgrade(conn: &Connection, from: i64) -> Result<()> {
    if from < 3 {
        add_column_if_missing(conn, "messages", "raw", "TEXT")?;
    }
    Ok(())
}

fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    // 先查一遍再 ALTER：`ALTER TABLE ADD COLUMN` 没有 IF NOT EXISTS，
    // 重复执行会直接报错。
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(std::result::Result::ok)
        .any(|name| name == column);
    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))
            .with_context(|| format!("给 {table} 增加列 {column} 失败"))?;
        tracing::info!(table, column, "已增加列");
    }
    Ok(())
}

/// 建表并写入 schema 版本。
pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(DDL).context("建表失败")?;

    // ⚠️ 不能用 `.ok()` 把三种语义压成同一个 None：
    //   (a) 行不存在     → 正常首次初始化，走 None 分支
    //   (b) 查询失败     → 真实错误，必须上报
    //   (c) 值不是整数   → 数据损坏，必须上报
    // (b)/(c) 若混进 None 分支，会撞上 meta.key 主键约束，报出一条毫无指向性的
    // "UNIQUE constraint failed"，进而让 main 静默把整个持久化降级掉。
    let current: Option<i64> = match conn.query_row(
        "SELECT value FROM meta WHERE key = 'schema_version'",
        [],
        |r| r.get::<_, String>(0),
    ) {
        Ok(v) => Some(
            v.parse()
                .with_context(|| format!("meta.schema_version 不是整数: {v:?}"))?,
        ),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(err) => return Err(err).context("读取 schema_version 失败"),
    };

    match current {
        Some(v) if v == SCHEMA_VERSION => {}
        Some(v) if v < SCHEMA_VERSION => {
            // 目前只有 v1，后续版本在这里按序升级。
            tracing::info!(from = v, to = SCHEMA_VERSION, "数据库 schema 已升级");
            upgrade(conn, v)?;
            conn.execute(
                "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [SCHEMA_VERSION.to_string()],
            )?;
            // 只在**首次到达本版本**时播种。之后控制者清空列表不会让默认值复活。
            crate::resource::seed_defaults(conn)?;
        }
        Some(v) => {
            anyhow::bail!(
                "数据库 schema 版本 {v} 高于本程序支持的 {SCHEMA_VERSION}，请升级程序或更换数据库文件"
            );
        }
        None => {
            // 与上面 v < SCHEMA_VERSION 的分支保持一致，做到真正幂等。
            conn.execute(
                "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [SCHEMA_VERSION.to_string()],
            )?;
            tracing::info!(version = SCHEMA_VERSION, "数据库 schema 已初始化");
            crate::resource::seed_defaults(conn)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL").unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let v: String = conn
            .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn rejects_future_schema_version() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
            [(SCHEMA_VERSION + 1).to_string()],
        )
        .unwrap();
        let err = migrate(&conn).unwrap_err().to_string();
        assert!(err.contains("高于本程序支持"), "应拒绝未来版本: {err}");
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
