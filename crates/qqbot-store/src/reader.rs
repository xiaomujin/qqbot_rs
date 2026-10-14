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

/// 一条落库的附件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAttachment {
    pub url: String,
    pub content_type: Option<String>,
    pub filename: Option<String>,
    pub size: Option<u64>,
    /// `true` = 来自被引用的消息（`msg_elements` 里 `message_type = 103`）。
    pub quoted: bool,
    pub created_at: i64,
}

pub enum ReadReq {
    RecentTexts {
        scope: String,
        target_id: String,
        /// `None` 表示不限发送者（本群词云）；`Some` 只看这个人（我的词云）。
        sender_id: Option<String>,
        since: i64,
        reply: oneshot::Sender<Result<Vec<String>>>,
    },
    /// 该会话在时间窗口内**最新**的一个附件。
    ///
    /// 取代了原先「取回最近 50 条原始 JSON、在插件里解析」的做法：
    /// 附件已经落成行，取最新一条是一条走索引的 SQL，而不是 50 次反序列化。
    LatestAttachment {
        scope: String,
        target_id: String,
        since: i64,
        reply: oneshot::Sender<Result<Option<StoredAttachment>>>,
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

    /// 该会话在时间窗口内**最新**的一个附件，没有则 `Ok(None)`。
    pub async fn latest_attachment(
        &self,
        scope: &str,
        target_id: &str,
        since: i64,
    ) -> Result<Option<StoredAttachment>> {
        let (reply, rx) = oneshot::channel();
        self.dispatch(ReadReq::LatestAttachment {
            scope: scope.to_string(),
            target_id: target_id.to_string(),
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
            ReadReq::LatestAttachment { scope, target_id, since, reply } => {
                let _ = reply.send(latest_attachment(&conn, &scope, &target_id, since));
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

/// 取某会话在时间窗口内**最新**的一个附件。
///
/// 排序键是 `(created_at DESC, ordinal DESC)`：同一毫秒/同一秒内，
/// 后一个附件更「新」。`since` 把窗口外的旧图挡掉 —— 否则「先发图再发命令」
/// 会捞到十天前那张图。
fn latest_attachment(
    conn: &Connection,
    scope: &str,
    target_id: &str,
    since: i64,
) -> Result<Option<StoredAttachment>> {
    let mut stmt = conn.prepare_cached(
        "SELECT url, content_type, filename, size, quoted, created_at FROM message_attachments
          WHERE scope = ?1 AND target_id = ?2 AND created_at >= ?3
          ORDER BY created_at DESC, ordinal DESC
          LIMIT 1",
    )?;
    let mut rows = stmt.query(rusqlite::params![scope, target_id, since])?;
    let Some(row) = rows.next()? else { return Ok(None) };
    Ok(Some(StoredAttachment {
        url: row.get(0)?,
        content_type: row.get(1)?,
        filename: row.get(2)?,
        // 存进去时就是 i64；负数只可能来自手改数据库，当成「未知大小」。
        size: row.get::<_, Option<i64>>(3)?.and_then(|v| u64::try_from(v).ok()),
        quoted: row.get(4)?,
        created_at: row.get(5)?,
    }))
}

fn purge_before(conn: &Connection, cutoff: i64) -> Result<usize> {
    let started = Instant::now();
    let deleted = conn
        .execute("DELETE FROM messages WHERE created_at < ?1", [cutoff])
        .context("删除过期消息失败")?;
    // 附件行一起删。它自带 created_at / scope / target_id，按同一个 cutoff 直接删
    // 即可，不需要 JOIN。漏掉这一步的话消息删了、附件行却永远留着 ——
    // 保留期就只对一半数据生效，库只增不减。
    conn.execute("DELETE FROM message_attachments WHERE created_at < ?1", [cutoff])
        .context("删除过期附件失败")?;

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
