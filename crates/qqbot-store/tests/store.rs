//! 存储层集成测试。
//!
//! 这里验证的是**行为契约**，不是实现细节：
//! 幂等、隔离、保留期边界、屏障语义、以及「写入永不阻塞调用方」。
//!
//! 需要看到写入的断言一律先 `store.flush()`，**不 sleep 猜写线程什么时候提交** ——
//! 猜短了会假失败，猜长了白等。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use qqbot_store::{now_unix, MessageStore, NewMessage, Scope, StoreConfig, DAY_SECS};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_db() -> std::path::PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "qqbot-store-test-{}-{}-{n}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("qqbot.db")
}

/// 测试用配置：保留默认的 `flush_interval`（250ms）。
///
/// 以前为了「让断言快一点」把它压到 20ms，那是**猜时间**：现在所有断言前
/// 都先 `flush()`，间隔多大都不影响结果。真正依赖间隔的用例
/// （`steady_writes_...` / `record_never_blocks_...`）各自显式覆盖它，
/// 并在那里写清理由。
fn cfg(path: std::path::PathBuf) -> StoreConfig {
    StoreConfig {
        path,
        sweep_interval: Duration::from_secs(3600),
        ..StoreConfig::default()
    }
}

fn msg(id: &str, scope: Scope, target: &str, content: &str, ts: i64) -> NewMessage {
    NewMessage {
        id: id.into(),
        scope,
        target_id: target.into(),
        sender_id: Some("U1".into()),
        sender_name: Some("小明".into()),
        event_name: match scope {
            Scope::Group => "GROUP_MESSAGE_CREATE".into(),
            Scope::C2c => "C2C_MESSAGE_CREATE".into(),
        },
        content: content.into(),
        raw: None,
        created_at: ts,
    }
}

#[tokio::test]
async fn persists_and_isolates_scopes() {
    let store = MessageStore::open(cfg(temp_db())).unwrap();

    store.record(msg("G1", Scope::Group, "GROUP_A", "群里的消息", 1000));
    store.record(msg("G2", Scope::Group, "GROUP_B", "另一个群", 1000));
    store.record(msg("C1", Scope::C2c, "USER_A", "私聊消息", 1000));
    store.flush().await.unwrap();

    assert_eq!(store.count().await.unwrap(), 3);

    let group_a = store.recent_texts(Scope::Group, "GROUP_A", None, 0).await.unwrap();
    assert_eq!(group_a, vec!["群里的消息"]);

    let c2c = store.recent_texts(Scope::C2c, "USER_A", None, 0).await.unwrap();
    assert_eq!(c2c, vec!["私聊消息"]);

    // 群与私聊互不串台
    let cross = store.recent_texts(Scope::C2c, "GROUP_A", None, 0).await.unwrap();
    assert!(cross.is_empty(), "C2C 查询不应命中群消息: {cross:?}");
}

/// flush 屏障：`record` 之后立刻 `flush()`，**不 sleep** 就能查到。
///
/// 这是本模块最重要的新契约 —— 在此之前，测试只能轮询或 sleep
/// 猜写线程什么时候提交。
#[tokio::test]
async fn flush_barrier_commits_prior_writes() {
    // 故意用默认的 250ms 间隔：屏障必须能在间隔到期**之前**把行交出来，
    // 否则它只是个换皮的 sleep。
    let store = MessageStore::open(cfg(temp_db())).unwrap();

    store.record(msg("F1", Scope::Group, "G", "屏障之后立刻可见", 1000));
    store.flush().await.unwrap();

    assert_eq!(store.count().await.unwrap(), 1, "flush 返回时写入必须已提交");
    let texts = store.recent_texts(Scope::Group, "G", None, 0).await.unwrap();
    assert_eq!(texts, vec!["屏障之后立刻可见"]);

    // 屏障是幂等的，空队列上再调一次也不该出错。
    store.flush().await.unwrap();
    assert_eq!(store.count().await.unwrap(), 1);
}

#[tokio::test]
async fn duplicate_message_id_is_stored_once() {
    let store = MessageStore::open(cfg(temp_db())).unwrap();

    for _ in 0..5 {
        store.record(msg("SAME", Scope::Group, "G", "重复推送", 1000));
    }
    store.flush().await.unwrap();
    assert_eq!(store.count().await.unwrap(), 1, "消息 id 是主键，重复推送只能入库一次");
}

#[tokio::test]
async fn purge_removes_only_messages_older_than_cutoff() {
    let store = MessageStore::open(cfg(temp_db())).unwrap();
    let now = 1_800_000_000i64;
    let cutoff = now - 365 * DAY_SECS as i64;

    store.record(msg("OLD", Scope::Group, "G", "两年前", cutoff - 1));
    store.record(msg("EDGE", Scope::Group, "G", "恰好一年", cutoff));
    store.record(msg("NEW", Scope::Group, "G", "昨天", now - DAY_SECS as i64));
    store.flush().await.unwrap();

    let deleted = store.purge_before(cutoff).await.unwrap();
    assert_eq!(deleted, 1, "只有严格早于 cutoff 的才该被删");

    let texts = store.recent_texts(Scope::Group, "G", None, 0).await.unwrap();
    assert!(texts.contains(&"恰好一年".to_string()), "边界上的消息必须保留: {texts:?}");
    assert!(texts.contains(&"昨天".to_string()));
    assert!(!texts.contains(&"两年前".to_string()));
}

#[tokio::test]
async fn recent_texts_respects_time_window() {
    let store = MessageStore::open(cfg(temp_db())).unwrap();
    let now = 1_800_000_000i64;

    store.record(msg("A", Scope::Group, "G", "很旧", now - 100 * DAY_SECS as i64));
    store.record(msg("B", Scope::Group, "G", "最近", now - DAY_SECS as i64));
    store.flush().await.unwrap();

    let texts = store
        .recent_texts(Scope::Group, "G", None, now - 30 * DAY_SECS as i64)
        .await
        .unwrap();
    assert_eq!(texts, vec!["最近"], "窗口外的消息不应进入语料");
}

#[tokio::test]
async fn empty_content_is_excluded_from_corpus() {
    let store = MessageStore::open(cfg(temp_db())).unwrap();
    store.record(msg("E", Scope::Group, "G", "", 1000));
    store.record(msg("T", Scope::Group, "G", "有内容", 1001));
    store.flush().await.unwrap();

    let texts = store.recent_texts(Scope::Group, "G", None, 0).await.unwrap();
    assert_eq!(texts, vec!["有内容"], "空正文不参与词云，但消息本身要入库");
}

#[tokio::test]
async fn long_content_is_truncated_before_storage() {
    let store = MessageStore::open(cfg(temp_db())).unwrap();
    let huge = "超".repeat(qqbot_store::MAX_CONTENT_CHARS + 3000);
    store.record(msg("L", Scope::Group, "G", &huge, 1000));
    store.flush().await.unwrap();

    let texts = store.recent_texts(Scope::Group, "G", None, 0).await.unwrap();
    assert_eq!(texts.len(), 1);
    assert_eq!(
        texts[0].chars().count(),
        qqbot_store::MAX_CONTENT_CHARS + 1,
        "超长正文必须被截断，否则单行体积无界"
    );
}

/// 这是本模块最重要的一条契约：**队列满时丢弃，绝不阻塞调用方**。
#[tokio::test]
async fn record_never_blocks_when_queue_is_full() {
    let store = MessageStore::open(StoreConfig {
        write_queue: 2,
        flush_batch: 100_000,
        // 故意把间隔拉长，让写线程来不及消费
        flush_interval: Duration::from_secs(30),
        ..cfg(temp_db())
    })
    .unwrap();

    let started = Instant::now();
    for i in 0..20_000 {
        store.record(msg(&format!("M{i}"), Scope::Group, "G", "压测", 1000));
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "20000 次 record 耗时 {elapsed:?}，说明写入路径被阻塞了"
    );

    let stats = store.stats().await.unwrap();
    assert!(stats.dropped > 0, "队列必然溢出，应记录丢弃数: {stats:?}");
    assert_eq!(
        stats.enqueued + stats.dropped,
        20_000,
        "每条消息要么入队要么被丢弃，不能凭空消失"
    );
}

/// 回归：flush 截止时间必须锚定在**本批第一条消息**上，不能每条消息都重新计时。
///
/// 修复前每条消息都会重置整个 `flush_interval`，于是只要写入间隔小于
/// `flush_interval`，超时分支永不触发，数据会一直攒到 `flush_batch` 才落盘
/// （崩溃丢失窗口从「一个间隔」放大到「一整批」）。
///
/// 这里用「批次永远填不满 + 持续写入」把缺陷逼出来，并且断言在**写入仍在进行时**
/// 求值 —— 停止写入后，即便是旧实现也会在最后一次超时后刷盘，那样就测不出来了。
/// 所以这条**不能**用 `flush()` 对齐，必须真的等时间过去。
#[tokio::test]
async fn steady_writes_are_flushed_by_time_not_by_batch() {
    let path = temp_db();
    let store = MessageStore::open(StoreConfig {
        path,
        flush_interval: Duration::from_millis(80),
        flush_batch: 100_000, // 批次永远填不满，只能靠时间触发
        sweep_interval: Duration::from_secs(3600),
        ..StoreConfig::default()
    })
    .unwrap();

    // 每 20ms 写一条（间隔 < flush_interval），持续约 400ms。
    for i in 0..20 {
        store.record(msg(&format!("S{i}"), Scope::Group, "G", "稳态写入", 1000));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 此刻已过去约 5 个 flush_interval；修复前这里恒为 0。
    let n = store.count().await.unwrap();
    assert!(n > 0, "持续写入下必须按时间刷盘，实际 {n} 行（旧实现会一直攒到批次满）");
}

#[tokio::test]
async fn data_survives_reopen() {
    let path = temp_db();
    {
        let store = MessageStore::open(cfg(path.clone())).unwrap();
        store.record(msg("P1", Scope::Group, "G", "重启前", 1000));
        store.flush().await.unwrap();
    }
    // store 已 drop → 读写线程退出、连接关闭

    let store = MessageStore::open(cfg(path)).unwrap();
    assert_eq!(store.count().await.unwrap(), 1, "数据必须持久化到磁盘");
    let texts = store.recent_texts(Scope::Group, "G", None, 0).await.unwrap();
    assert_eq!(texts, vec!["重启前"]);
}

#[tokio::test]
async fn sweeper_task_runs_and_purges_expired_rows() {
    let store = MessageStore::open(StoreConfig {
        retention: Duration::from_secs(30 * DAY_SECS),
        // interval 首次 tick 立即触发，所以启动即清理
        sweep_interval: Duration::from_secs(3600),
        ..cfg(temp_db())
    })
    .unwrap();

    let now = qqbot_store::now_unix();
    store.record(msg("ANCIENT", Scope::Group, "G", "很久以前", now - 400 * DAY_SECS as i64));
    store.record(msg("FRESH", Scope::Group, "G", "刚刚", now));
    store.flush().await.unwrap();

    let handle = store.spawn_sweeper();
    // 这里等的是**清理任务被调度**，不是写线程提交 —— 只能轮询。
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.count().await.unwrap() > 1 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.abort();

    assert_eq!(store.count().await.unwrap(), 1, "超过保留期的消息应被定时任务删除");
    let texts = store.recent_texts(Scope::Group, "G", None, 0).await.unwrap();
    assert_eq!(texts, vec!["刚刚"]);
}

/// 原始事件 JSON 仍然入库（将来要重新提取未声明的字段只能靠它）。
#[tokio::test]
async fn raw_event_json_is_still_stored() {
    let path = temp_db();
    let store = MessageStore::open(cfg(path.clone())).unwrap();

    let mut m = msg("M_RAW", Scope::Group, "G1", "带原文", 1000);
    m.raw = Some(r#"{"id":"M_RAW"}"#.to_string());
    store.record(m);
    store.flush().await.unwrap();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let raw: Option<String> = conn
        .query_row("SELECT raw FROM messages WHERE id = 'M_RAW'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(raw.as_deref(), Some(r#"{"id":"M_RAW"}"#));
}

/// 回归：写线程丢批时 `flush()` 必须返回 `Err`，不能回一句 `Ok(())`。
///
/// 修复前它无条件回成功，于是「本批最多 512 条已丢弃」只留下一条 warn 日志，
/// 而屏障的调用方以为「此前入队的都已提交」。
#[tokio::test]
async fn flush_reports_a_failed_batch_instead_of_claiming_success() {
    let path = temp_db();
    // 故意把间隔拉长：让写线程**只**在屏障到来时提交，否则它可能抢先把这批刷掉，
    // 屏障看到的缓冲区就是空的（那时无错可报，测试会偶发失败）。
    let store = MessageStore::open(StoreConfig {
        flush_interval: Duration::from_secs(30),
        ..cfg(path.clone())
    })
    .unwrap();

    // 让写入必然失败：把 messages 换成形状不兼容的同名表。
    // 保留一张表（而不是 DROP 掉）是为了让 `stats()` 里的 count 仍然能跑。
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("DROP TABLE messages; CREATE TABLE messages (wrong TEXT);").unwrap();
    }

    store.record(msg("M1", Scope::Group, "G", "写不进去", 1000));
    let err = store.flush().await.expect_err("写失败必须报错");
    assert!(format!("{err:#}").contains("存储写入失败"), "{err:#}");

    let stats = store.stats().await.unwrap();
    assert_eq!(stats.failed_rows, 1, "失败的行数要单独计数: {stats:?}");
    assert_eq!(stats.written, 0, "{stats:?}");
    assert_eq!(stats.dropped, 0, "SQL 失败不是「未入队」: {stats:?}");
}

/// 保留期清理的边界：严格早于 cutoff 的删掉，正好等于 cutoff 的留着。
#[tokio::test]
async fn purge_keeps_the_boundary_row() {
    let path = temp_db();
    let store = MessageStore::open(cfg(path.clone())).unwrap();
    let now = 1_800_000_000i64;
    let cutoff = now - 365 * DAY_SECS as i64;

    store.record(msg("A_OLD", Scope::Group, "G", "过期", cutoff - 1));
    store.record(msg("A_EDGE", Scope::Group, "G", "边界", cutoff));
    store.flush().await.unwrap();

    assert_eq!(store.purge_before(cutoff).await.unwrap(), 1, "只有严格早于 cutoff 的消息被删");

    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut stmt = conn.prepare("SELECT id FROM messages ORDER BY id").unwrap();
    let left: Vec<String> = stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(left, vec!["A_EDGE"], "边界上的那条要留着");
}

/// 「我的词云」靠发送者过滤：同一会话里只取某个人说的话。
#[tokio::test]
async fn recent_texts_can_filter_by_sender() {
    let store = MessageStore::open(cfg(temp_db())).unwrap();

    let mut from_a = msg("M_A", Scope::Group, "G1", "甲说的话", now_unix());
    from_a.sender_id = Some("UA".into());
    let mut from_b = msg("M_B", Scope::Group, "G1", "乙说的话", now_unix());
    from_b.sender_id = Some("UB".into());
    store.record(from_a);
    store.record(from_b);
    store.flush().await.unwrap();

    // `None` = 不限发送者（本群词云）
    let all = store.recent_texts(Scope::Group, "G1", None, 0).await.unwrap();
    assert_eq!(all.len(), 2, "本群词云应当拿到两个人的话：{all:?}");

    // `Some` = 只看这个人（我的词云）
    let mine = store.recent_texts(Scope::Group, "G1", Some("UA"), 0).await.unwrap();
    assert_eq!(mine, vec!["甲说的话"], "我的词云只该有自己说的");

    // 发送者过滤与时间窗要能叠加，不能互相覆盖。
    let future = store
        .recent_texts(Scope::Group, "G1", Some("UA"), i64::MAX)
        .await
        .unwrap();
    assert!(future.is_empty(), "时间窗应当仍然生效");

    // 没说过话的人拿到空，而不是别人的话。
    let stranger = store
        .recent_texts(Scope::Group, "G1", Some("UC"), 0)
        .await
        .unwrap();
    assert!(stranger.is_empty(), "不该把别人的话算成他的");
}
