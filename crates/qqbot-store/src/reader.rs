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
///
/// 上限是 store 自己的事：调用方不需要知道这个数字，知道了也只会照抄回来。
const MAX_TEXT_ROWS: usize = 20_000;
/// 单次查询累计正文字符上限，避免超大群把内存吃满。
const MAX_TEXT_CHARS: usize = 8 * 1024 * 1024;

pub enum ReadReq {
    RecentTexts {
        scope: String,
        target_id: String,
        /// `None` 表示不限发送者（本群词云）；`Some` 只看这个人（我的词云）。
        sender_id: Option<String>,
        since: i64,
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
        sender_id: Option<&str>,
        since: i64,
    ) -> Result<Vec<String>> {
        let (reply, rx) = oneshot::channel();
        self.dispatch(ReadReq::RecentTexts {
            scope: scope.to_string(),
            target_id: target_id.to_string(),
            sender_id: sender_id.map(str::to_string),
            since,
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
            ReadReq::RecentTexts { scope, target_id, sender_id, since, reply } => {
                let _ = reply.send(recent_texts(
                    &conn,
                    &scope,
                    &target_id,
                    sender_id.as_deref(),
                    since,
                ));
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
/// 词云只需要「最近的一段语料」，因此同时受行数与总字符数两道限制 ——
/// 两道限制都由本函数自己施加，调用方不必（也不该）传上限进来。
fn recent_texts(
    conn: &Connection,
    scope: &str,
    target_id: &str,
    sender_id: Option<&str>,
    since: i64,
) -> Result<Vec<String>> {
    let limit = MAX_TEXT_ROWS as i64;
    // `?4 IS NULL OR sender_id = ?4`：同一个参数既表达「不限发送者」（本群词云）
    // 又表达「只看某人」（我的词云），不必维护两条几乎相同的 SQL。
    let mut stmt = conn.prepare_cached(
        "SELECT content FROM messages
          WHERE scope = ?1 AND target_id = ?2 AND created_at >= ?3 AND content <> ''
            AND (?4 IS NULL OR sender_id = ?4)
          ORDER BY created_at DESC
          LIMIT ?5",
    )?;

    let rows = stmt.query_map(
        rusqlite::params![scope, target_id, since, sender_id, limit],
        |r| r.get::<_, String>(0),
    )?;

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
    // 这里**不做** `incremental_vacuum`：删掉的页会被后续消息复用，真实库里
    // `freelist_count` 恒为 0，本来就没有空闲页可回收；`auto_vacuum` 也已在
    // `apply_pragmas` 里刻意不设（理由见那里）。文件在「一个保留期的高水位」处
    // 稳定下来，要缩容得手动 `VACUUM`。
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
