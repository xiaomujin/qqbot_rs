//! 运维小工具：查看存储统计，或塞一条「N 天前」的消息以验证保留期清理。
//!
//! 用法：
//! ```text
//! cargo run -p qqbot-store --example seed -- <db>              # 只打印统计
//! cargo run -p qqbot-store --example seed -- <db> <天数> [内容]  # 插入一条旧消息
//! ```
//!
//! 例：塞一条 400 天前的消息，然后重启机器人，观察它是否被自动清理。

use std::time::Duration;

use qqbot_store::{now_unix, MessageStore, NewMessage, Scope, StoreConfig, DAY_SECS};

// store crate 只启用了 rt（不启用 rt-multi-thread），因此显式指定单线程运行时。
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let db = args.get(1).cloned().unwrap_or_else(|| "data/qqbot.db".into());
    let days: Option<i64> = args.get(2).and_then(|s| s.parse().ok());
    let content = args
        .get(3)
        .cloned()
        .unwrap_or_else(|| format!("{} 天前的测试消息", days.unwrap_or(0)));

    let store = MessageStore::open(StoreConfig {
        path: db.clone().into(),
        ..StoreConfig::default()
    })?;

    let before = store.count().await?;

    // 不带天数 → 只报告统计，不写入
    let Some(days) = days else {
        let stats = store.stats().await?;
        println!(
            "{db}: 共 {} 行；已入队 {} / 已写入 {} / 丢弃 {} / 批次数 {}；保留期 {} 天",
            stats.rows, stats.enqueued, stats.written, stats.dropped, stats.batches,
            stats.retention_days
        );
        return Ok(());
    };
    store.record(NewMessage {
        id: format!("SEED-{}", now_unix()),
        scope: Scope::Group,
        target_id: "SEED".into(),
        sender_id: None,
        sender_name: Some("seed".into()),
        event_name: "SEED".into(),
        raw: None,
        content,
        created_at: now_unix() - days * DAY_SECS as i64,
    });
    tokio::time::sleep(Duration::from_millis(600)).await;

    println!(
        "已向 {db} 插入 1 条 {days} 天前的消息；行数 {before} → {}",
        store.count().await?
    );
    Ok(())
}
