//! 写线程：批量事务提交。
//!
//! **核心约束：写入绝不能阻塞消息热路径。**
//! 因此 `record` 只做一次 `try_send`，队列满就丢弃并计数，
//! 真正的 SQLite 事务提交全部发生在独立的 OS 线程上。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::model::NewMessage;

/// 消息 id 是主键 → 重复推送天然被忽略，与内存 dedup 形成双保险。
const INSERT_SQL: &str = "INSERT OR IGNORE INTO messages
    (id, scope, target_id, sender_id, sender_name, event_name, content, created_at)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";

/// 写入侧计数器，用于观测丢弃率。
#[derive(Debug, Default)]
pub struct WriteStats {
    pub enqueued: AtomicU64,
    pub written: AtomicU64,
    pub dropped: AtomicU64,
    pub batches: AtomicU64,
}

pub struct WriteHandle {
    tx: SyncSender<NewMessage>,
    pub stats: Arc<WriteStats>,
}

impl WriteHandle {
    pub fn spawn(
        conn: Connection,
        queue: usize,
        flush_interval: Duration,
        flush_batch: usize,
    ) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel(queue.max(1));
        let stats = Arc::new(WriteStats::default());
        let thread_stats = Arc::clone(&stats);
        std::thread::Builder::new()
            .name("qqbot-store-writer".into())
            .spawn(move || writer_loop(conn, rx, flush_interval, flush_batch, thread_stats))
            // 线程创建失败是**环境/资源**问题（EAGAIN、线程数上限），不是代码 bug。
            // 上报给调用方走降级路径，而不是 panic 掉整个机器人。
            .context("启动存储写线程失败")?;
        Ok(Self { tx, stats })
    }

    /// 非阻塞投递。队列满时**丢弃并计数**，绝不阻塞调用方。
    pub fn record(&self, msg: NewMessage) {
        match self.tx.try_send(msg) {
            Ok(()) => {
                self.stats.enqueued.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("qqbot_store_write_dropped_total", "reason" => "queue_full")
                    .increment(1);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("qqbot_store_write_dropped_total", "reason" => "writer_gone")
                    .increment(1);
            }
        }
    }
}

fn writer_loop(
    mut conn: Connection,
    rx: Receiver<NewMessage>,
    flush_interval: Duration,
    flush_batch: usize,
    stats: Arc<WriteStats>,
) {
    let flush_batch = flush_batch.max(1);
    // 0 间隔会让下面的循环变成忙等。
    let flush_interval = flush_interval.max(Duration::from_millis(1));
    let mut buf: Vec<NewMessage> = Vec::with_capacity(flush_batch);

    // ⚠️ 截止时间必须锚定在**本批第一条消息**上，不能每条消息都重新计时。
    // 否则只要消息到达间隔小于 flush_interval，recv_timeout 就永远返回 Ok、
    // 超时分支永不触发，缓冲区会一直攒到 flush_batch 才落盘 ——
    // 崩溃丢失窗口从「一个间隔」被放大到「一整批」（5 msg/s 下约 100 秒）。
    let mut deadline = Instant::now() + flush_interval;

    loop {
        // 到期就提交。放在循环开头统一处理，超时分支因此无需重复逻辑。
        if !buf.is_empty() && Instant::now() >= deadline {
            flush(&mut conn, &mut buf, &stats);
            deadline = Instant::now() + flush_interval;
        }

        let wait = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(msg) => {
                // 新批次从第一条消息开始计时。
                if buf.is_empty() {
                    deadline = Instant::now() + flush_interval;
                }
                buf.push(msg);
                if buf.len() >= flush_batch {
                    flush(&mut conn, &mut buf, &stats);
                    deadline = Instant::now() + flush_interval;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if !buf.is_empty() {
                    flush(&mut conn, &mut buf, &stats);
                }
                tracing::info!("消息存储写线程退出");
                return;
            }
        }
    }
}

fn flush(conn: &mut Connection, buf: &mut Vec<NewMessage>, stats: &WriteStats) {
    let started = Instant::now();
    let rows = buf.len();

    let result = (|| -> rusqlite::Result<()> {
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(INSERT_SQL)?;
            for m in buf.iter() {
                stmt.execute(rusqlite::params![
                    m.id,
                    m.scope.as_str(),
                    m.target_id,
                    m.sender_id,
                    m.sender_name,
                    m.event_name,
                    m.truncated_content().as_ref(),
                    m.created_at,
                ])?;
            }
        }
        tx.commit()
    })();

    match result {
        Ok(()) => {
            stats.written.fetch_add(rows as u64, Ordering::Relaxed);
            stats.batches.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("qqbot_store_written_total").increment(rows as u64);
            metrics::histogram!("qqbot_store_flush_seconds")
                .record(started.elapsed().as_secs_f64());
            tracing::debug!(rows, elapsed_ms = started.elapsed().as_millis() as u64, "消息已入库");
        }
        Err(err) => {
            // 写失败不 panic：丢一批总比整个机器人挂掉好。
            tracing::warn!(rows, error = %err, "消息批量入库失败，本批丢弃");
            metrics::counter!("qqbot_store_write_dropped_total", "reason" => "sql_error")
                .increment(rows as u64);
        }
    }
    buf.clear();
}
