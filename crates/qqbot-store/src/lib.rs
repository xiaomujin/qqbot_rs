//! 消息持久化与保留期清理。
//!
//! # 在系统中的位置
//!
//! ```text
//!   GatewayActor ──► Dispatcher ──► MessageStore::record  (try_send，非阻塞)
//!                        │                    │
//!                        │                    ▼
//!                        │            ┌──────────────────┐
//!                        │            │ 有界队列 4096    │ 满则丢弃并计数
//!                        │            └────────┬─────────┘
//!                        │                     ▼
//!                        │            [写线程] 批量事务 ──► SQLite (WAL)
//!                        │                                    ▲
//!                        └──► 词云插件 ──► recent_texts ──► [读线程]（独立连接）
//!
//!   [清理任务] tokio interval ──► purge_before(now - 365d)
//! ```
//!
//! 三条设计红线：
//!
//! 1. **写入不阻塞热路径**：`record` 只做一次 `try_send`，SQLite 事务在独立 OS 线程上批量提交。
//! 2. **读写分连接**：WAL 下读不阻塞写，慢查询（词云扫上万行）不会卡住写入。
//! 3. **消息 id 即主键**：重复推送天然幂等，不依赖内存 dedup 缓存。

mod model;
mod reader;
mod schema;
mod writer;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

pub use model::{fmt_unix, now_unix, NewMessage, Scope, MAX_CONTENT_CHARS};
pub use reader::{MAX_TEXT_CHARS, MAX_TEXT_ROWS};

use reader::ReadHandle;
use writer::WriteHandle;

/// 一天的秒数。
pub const DAY_SECS: u64 = 24 * 3600;

#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub path: PathBuf,
    /// 保留期：早于 `now - retention` 的消息会被定时清理。
    pub retention: Duration,
    /// 写入队列容量。满了就丢弃并计数 —— 绝不阻塞消息热路径。
    pub write_queue: usize,
    /// 批量提交间隔：低峰期消息不会在内存里久留。
    pub flush_interval: Duration,
    /// 攒够这么多行立刻提交，不必等间隔。
    pub flush_batch: usize,
    /// 清理任务的执行间隔。
    pub sweep_interval: Duration,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("data/qqbot.db"),
            retention: Duration::from_secs(365 * DAY_SECS),
            write_queue: 4096,
            flush_interval: Duration::from_millis(250),
            flush_batch: 512,
            sweep_interval: Duration::from_secs(6 * 3600),
        }
    }
}

impl StoreConfig {
    pub fn retention_days(&self) -> u64 {
        self.retention.as_secs() / DAY_SECS
    }
}

#[derive(Debug, Clone)]
pub struct StoreStats {
    pub rows: u64,
    pub enqueued: u64,
    pub written: u64,
    pub dropped: u64,
    pub batches: u64,
    pub retention_days: u64,
}

pub struct MessageStore {
    writer: WriteHandle,
    reader: ReadHandle,
    cfg: StoreConfig,
}

impl MessageStore {
    /// 打开数据库、建表，并启动读写线程。
    pub fn open(cfg: StoreConfig) -> Result<Arc<Self>> {
        let write_conn = schema::open(&cfg.path)?;
        schema::migrate(&write_conn)?;
        // 读线程用**独立连接**：WAL 下读不阻塞写。
        let read_conn = schema::open(&cfg.path)?;

        let writer = WriteHandle::spawn(
            write_conn,
            cfg.write_queue,
            cfg.flush_interval,
            cfg.flush_batch,
        )?;
        let reader = ReadHandle::spawn(read_conn)?;

        tracing::info!(
            path = %cfg.path.display(),
            retention_days = cfg.retention_days(),
            write_queue = cfg.write_queue,
            "消息存储已打开"
        );
        Ok(Arc::new(Self { writer, reader, cfg }))
    }

    /// 异步入口：把建库 / PRAGMA / 建线程这些**同步阻塞**工作挪到阻塞线程池。
    ///
    /// `open` 内部有 `create_dir_all`、`Connection::open`，以及
    /// `PRAGMA journal_mode = WAL`（涉及文件创建与 fsync）——都是同步阻塞 IO，
    /// 不该直接跑在 runtime 的 worker 线程上。
    pub async fn open_async(cfg: StoreConfig) -> Result<Arc<Self>> {
        tokio::task::spawn_blocking(move || Self::open(cfg))
            .await
            .context("存储初始化任务 panic")?
    }

    /// 投递一条消息入库。**非阻塞**：队列满时丢弃并计数。
    pub fn record(&self, msg: NewMessage) {
        self.writer.record(msg);
    }

    /// 某会话在时间窗口内的正文（按时间倒序）。
    pub async fn recent_texts(
        &self,
        scope: Scope,
        target_id: &str,
        since: i64,
        limit: usize,
    ) -> Result<Vec<String>> {
        self.reader
            .recent_texts(scope.as_str(), target_id, since, limit)
            .await
    }

    /// 删除早于 `cutoff`（Unix 秒）的消息，返回删除行数。
    pub async fn purge_before(&self, cutoff: i64) -> Result<usize> {
        self.reader.purge_before(cutoff).await
    }

    /// 当前总行数。
    pub async fn count(&self) -> Result<u64> {
        self.reader.count().await
    }

    pub fn retention(&self) -> Duration {
        self.cfg.retention
    }

    pub fn config(&self) -> &StoreConfig {
        &self.cfg
    }

    pub async fn stats(&self) -> Result<StoreStats> {
        let s = &self.writer.stats;
        Ok(StoreStats {
            rows: self.count().await?,
            enqueued: s.enqueued.load(Ordering::Relaxed),
            written: s.written.load(Ordering::Relaxed),
            dropped: s.dropped.load(Ordering::Relaxed),
            batches: s.batches.load(Ordering::Relaxed),
            retention_days: self.cfg.retention_days(),
        })
    }

    /// 启动保留期清理定时任务。
    ///
    /// `tokio::time::interval` 的第一次 tick 立即触发，因此进程启动时就会清理一次，
    /// 不必等到第一个间隔过去。
    pub fn spawn_sweeper(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let store = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(store.cfg.sweep_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                // 收窄转换 + 饱和减法：裸 `now - x` 在 release 下会静默回绕成
                // 一个巨大正数，让下面的 DELETE 清空全表。
                let retention_secs =
                    i64::try_from(store.cfg.retention.as_secs()).unwrap_or(i64::MAX);
                let cutoff = now_unix().saturating_sub(retention_secs);
                match store.purge_before(cutoff).await {
                    Ok(0) => tracing::debug!(cutoff, "保留期清理：无需删除"),
                    Ok(deleted) => {
                        metrics::counter!("qqbot_store_purged_total").increment(deleted as u64);
                        tracing::info!(
                            deleted,
                            cutoff,
                            cutoff_at = %fmt_unix(cutoff),
                            "保留期清理完成"
                        );
                    }
                    Err(err) => tracing::warn!(error = %err, "保留期清理失败"),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_retention_is_one_year() {
        let cfg = StoreConfig::default();
        assert_eq!(cfg.retention_days(), 365);
    }

    #[test]
    fn retention_days_truncates_partial_days() {
        let cfg = StoreConfig {
            retention: Duration::from_secs(365 * DAY_SECS + 3600),
            ..StoreConfig::default()
        };
        assert_eq!(cfg.retention_days(), 365);
    }
}
