//! 写线程：批量事务提交。
//!
//! **核心约束：写入绝不能阻塞消息热路径。**
//! 因此 `record` 只做一次 `try_send`，队列满就丢弃并计数，
//! 真正的 SQLite 事务提交全部发生在独立的 OS 线程上。
//!
//! 需要「确定的时刻」时用 `flush()`：它是走**同一条队列**的屏障，
//! 返回即表示此前入队的记录都已提交。热路径不用它。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::sync::oneshot;

use crate::model::NewMessage;

/// 消息 id 是主键 → 重复推送天然被忽略，与内存 dedup 形成双保险。
const INSERT_SQL: &str = "INSERT OR IGNORE INTO messages
    (id, scope, target_id, sender_id, sender_name, event_name, content, raw, created_at)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)";

/// 附件行。主键 (msg_id, ordinal) → 同一条消息重复推送（重推是常态）天然幂等。
const INSERT_ATTACHMENT_SQL: &str = "INSERT OR IGNORE INTO message_attachments
    (msg_id, ordinal, scope, target_id, created_at, quoted, url, content_type, filename, size)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)";

/// flush 屏障的总超时。
///
/// 写线程正常情况下毫秒级就能确认；超过它说明写线程卡死了（例如 SQLite 被
/// 别的连接长时间锁住），此时**报错**比让调用方无限挂住好。
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// 写线程的输入。
///
/// 屏障必须和记录走**同一条**通道：分成两条通道的话，
/// 「先 record 再 flush」的两次投递可能被写线程反序看到，
/// 而 flush 承诺的是「**此前**已入队的写入都已提交」。
enum WriteMsg {
    Record(NewMessage),
    /// 收到它的写线程先把已攒的记录提交，再通过 oneshot 应答。
    Flush(oneshot::Sender<()>),
}

/// 写入侧计数器，用于观测丢弃率。
#[derive(Debug, Default)]
pub struct WriteStats {
    pub enqueued: AtomicU64,
    pub written: AtomicU64,
    pub dropped: AtomicU64,
    pub batches: AtomicU64,
}

pub struct WriteHandle {
    tx: SyncSender<WriteMsg>,
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
        match self.tx.try_send(WriteMsg::Record(msg)) {
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

    /// flush 屏障：返回表示「**此前**已入队的写入都已提交」，之后的写入不保证。
    ///
    /// 存在的理由是测试与运维工具都需要一个确定的时刻 —— 在此之前，
    /// 「写入什么时候落盘」只能靠 sleep 猜 `flush_interval`（猜短了假失败、
    /// 猜长了白等）。消息热路径（`record`）不用它，仍然是纯 `try_send`。
    ///
    /// 这里刻意不阻塞：本函数是 `async`，可能跑在 runtime 的 worker 线程上，
    /// 而 `SyncSender::send` 是同步阻塞调用 —— 写线程正忙时它会把 worker
    /// 整个卡住。所以队列满时用 `try_send` + `yield_now()` 重试：
    /// 只要写线程还在消费，队列迟早会腾出位置。
    pub async fn flush(&self) -> Result<()> {
        let (ack, rx) = oneshot::channel();
        let mut pending = WriteMsg::Flush(ack);
        let deadline = Instant::now() + FLUSH_TIMEOUT;

        loop {
            match self.tx.try_send(pending) {
                Ok(()) => break,
                Err(TrySendError::Full(msg)) => {
                    pending = msg;
                    if Instant::now() >= deadline {
                        anyhow::bail!("存储写队列持续满载，flush 未能在 {FLUSH_TIMEOUT:?} 内入队");
                    }
                    // 让出当前任务：写线程是独立 OS 线程，这里空转没有意义。
                    tokio::task::yield_now().await;
                }
                Err(TrySendError::Disconnected(_)) => {
                    anyhow::bail!("存储写线程已退出，无法 flush");
                }
            }
        }

        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, rx).await {
            Ok(Ok(())) => Ok(()),
            // oneshot 发送端被丢弃：写线程在提交之前退出了。
            Ok(Err(_)) => anyhow::bail!("存储写线程在确认 flush 之前退出"),
            Err(_) => anyhow::bail!("等待存储写线程 flush 超时（{FLUSH_TIMEOUT:?}）"),
        }
    }
}

fn writer_loop(
    mut conn: Connection,
    rx: Receiver<WriteMsg>,
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
            Ok(WriteMsg::Record(msg)) => {
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
            Ok(WriteMsg::Flush(ack)) => {
                // 屏障：**先提交再应答**。反过来的话，调用方在收到 ack 之后
                // 立刻查库会查不到自己刚写的东西 —— 屏障就没意义了。
                // 通道是 FIFO，所以此刻缓冲区里已经包含所有先于它入队的记录。
                if !buf.is_empty() {
                    flush(&mut conn, &mut buf, &stats);
                    deadline = Instant::now() + flush_interval;
                }
                // 调用方可能已经超时放弃（接收端被 drop）—— 不是错误。
                let _ = ack.send(());
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
            let mut att_stmt = tx.prepare_cached(INSERT_ATTACHMENT_SQL)?;
            for m in buf.iter() {
                stmt.execute(rusqlite::params![
                    m.id,
                    m.scope.as_str(),
                    m.target_id,
                    m.sender_id,
                    m.sender_name,
                    m.event_name,
                    m.truncated_content().as_ref(),
                    m.raw,
                    m.created_at,
                ])?;
                // 附件与消息在**同一个事务**里。分成两个事务的话，
                // 中间那一刻库里的消息是没有附件的，而「最近一张图」
                // 恰恰是按附件行找的 —— 回查会漏掉刚发的那张。
                for (ordinal, att) in m.attachments.iter().enumerate() {
                    att_stmt.execute(rusqlite::params![
                        m.id,
                        ordinal as i64,
                        m.scope.as_str(),
                        m.target_id,
                        m.created_at,
                        att.quoted,
                        att.url,
                        att.content_type,
                        att.filename,
                        // size 是 u64 而 SQLite 只有 i64：超范围就存 NULL，
                        // 而不是回绕成一个负数让上限检查失效。
                        att.size.and_then(|s| i64::try_from(s).ok()),
                    ])?;
                }
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
