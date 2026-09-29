//! 建表、PRAGMA 与版本迁移。

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// 当前 schema 版本，记录在 `meta` 表里。
pub const SCHEMA_VERSION: i64 = 1;

const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS messages (
    id          TEXT PRIMARY KEY,
    scope       TEXT NOT NULL CHECK (scope IN ('group','c2c')),
    target_id   TEXT NOT NULL,
    sender_id   TEXT,
    sender_name TEXT,
    event_name  TEXT NOT NULL,
    content     TEXT NOT NULL,
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
            conn.execute(
                "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [SCHEMA_VERSION.to_string()],
            )?;
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
