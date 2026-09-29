//! 读线程：独占一个**独立连接**。
//!
//! WAL 模式下读不阻塞写，所以把查询放在自己的线程上，
//! 慢查询（例如词云要扫上万行）不会卡住正在提交的写入。

use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::sync::oneshot;

/// 单次查询最多取多少条正文。
pub const MAX_TEXT_ROWS: usize = 20_000;
/// 单次查询累计正文字符上限，避免超大群把内存吃满。
pub const MAX_TEXT_CHARS: usize = 8 * 1024 * 1024;

pub enum ReadReq {
    RecentTexts {
        scope: String,
        target_id: String,
        since: i64,
        limit: usize,
        reply: oneshot::Sender<Result<Vec<String>>>,
    },
    PurgeBefore {
        cutoff: i64,
        reply: oneshot::Sender<Result<usize>>,
    },
    Count {
        reply: oneshot::Sender<Result<u64>>,
    },
}

pub struct ReadHandle {
    tx: Sender<ReadReq>,
}

impl ReadHandle {
    pub fn spawn(conn: Connection) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("qqbot-store-reader".into())
            .spawn(move || reader_loop(conn, rx))
            // 同 writer：环境失败不该 panic 掉整个进程。
            .context("启动存储读线程失败")?;
        Ok(Self { tx })
    }

    fn dispatch(&self, req: ReadReq) -> Result<()> {
        self.tx.send(req).map_err(|_| anyhow::anyhow!("存储读线程已退出"))
    }

    pub async fn recent_texts(
        &self,
        scope: &str,
        target_id: &str,
        since: i64,
        limit: usize,
    ) -> Result<Vec<String>> {
        let (reply, rx) = oneshot::channel();
        self.dispatch(ReadReq::RecentTexts {
            scope: scope.to_string(),
            target_id: target_id.to_string(),
            since,
            limit,
            reply,
        })?;
        rx.await.context("存储读线程未回复")?
    }

    pub async fn purge_before(&self, cutoff: i64) -> Result<usize> {
        let (reply, rx) = oneshot::channel();
        self.dispatch(ReadReq::PurgeBefore { cutoff, reply })?;
        rx.await.context("存储读线程未回复")?
    }

    pub async fn count(&self) -> Result<u64> {
        let (reply, rx) = oneshot::channel();
        self.dispatch(ReadReq::Count { reply })?;
        rx.await.context("存储读线程未回复")?
    }
}

fn reader_loop(conn: Connection, rx: Receiver<ReadReq>) {
    while let Ok(req) = rx.recv() {
        match req {
            ReadReq::RecentTexts { scope, target_id, since, limit, reply } => {
                let _ = reply.send(recent_texts(&conn, &scope, &target_id, since, limit));
            }
            ReadReq::PurgeBefore { cutoff, reply } => {
                let _ = reply.send(purge_before(&conn, cutoff));
            }
            ReadReq::Count { reply } => {
                let _ = reply.send(count(&conn));
            }
        }
    }
    tracing::info!("消息存储读线程退出");
}

/// 取某会话在时间窗口内的正文，按时间倒序。
///
/// 词云只需要「最近的一段语料」，因此同时受行数与总字符数两道限制。
fn recent_texts(
    conn: &Connection,
    scope: &str,
    target_id: &str,
    since: i64,
    limit: usize,
) -> Result<Vec<String>> {
    let limit = limit.min(MAX_TEXT_ROWS) as i64;
    let mut stmt = conn.prepare_cached(
        "SELECT content FROM messages
          WHERE scope = ?1 AND target_id = ?2 AND created_at >= ?3 AND content <> ''
          ORDER BY created_at DESC
          LIMIT ?4",
    )?;

    let rows = stmt.query_map(rusqlite::params![scope, target_id, since, limit], |r| {
        r.get::<_, String>(0)
    })?;

    let mut out = Vec::new();
    let mut chars = 0usize;
    for row in rows {
        let text = row?;
        chars += text.chars().count();
        out.push(text);
        if chars >= MAX_TEXT_CHARS {
            tracing::debug!(rows = out.len(), chars, "词云语料已达字符上限，提前截断");
            break;
        }
    }
    Ok(out)
}

fn purge_before(conn: &Connection, cutoff: i64) -> Result<usize> {
    let started = Instant::now();
    let deleted = conn
        .execute("DELETE FROM messages WHERE created_at < ?1", [cutoff])
        .context("删除过期消息失败")?;

    if deleted > 0 {
        // 删除只把页标记为空闲，这里按需归还给文件系统，
        // 避免长期运行后数据库文件只增不减。
        if let Err(err) = conn.execute_batch("PRAGMA incremental_vacuum;") {
            tracing::debug!(error = %err, "incremental_vacuum 失败（不影响清理结果）");
        }
    }
    tracing::debug!(
        deleted,
        cutoff,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "保留期清理"
    );
    Ok(deleted)
}

fn count(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
    Ok(n.max(0) as u64)
}
