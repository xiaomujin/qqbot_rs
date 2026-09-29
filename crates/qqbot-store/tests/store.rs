//! 存储层集成测试。
//!
//! 这里验证的是**行为契约**，不是实现细节：
//! 幂等、隔离、保留期边界、以及「写入永不阻塞调用方」。

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

/// 测试用配置：flush 间隔压到 20ms，避免每个断言都要等 250ms。
fn cfg(path: std::path::PathBuf) -> StoreConfig {
    StoreConfig {
        path,
        flush_interval: Duration::from_millis(20),
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

/// 轮询等待写入落盘。写线程是异步批提交，断言前必须等它。
async fn wait_for_rows(store: &MessageStore, want: u64) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let n = store.count().await.unwrap();
        if n >= want || Instant::now() > deadline {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn persists_and_isolates_scopes() {
    let store = MessageStore::open(cfg(temp_db())).unwrap();

    store.record(msg("G1", Scope::Group, "GROUP_A", "群里的消息", 1000));
    store.record(msg("G2", Scope::Group, "GROUP_B", "另一个群", 1000));
    store.record(msg("C1", Scope::C2c, "USER_A", "私聊消息", 1000));

    assert_eq!(wait_for_rows(&store, 3).await, 3);

    let group_a = store.recent_texts(Scope::Group, "GROUP_A", None, 0, 100).await.unwrap();
    assert_eq!(group_a, vec!["群里的消息"]);

    let c2c = store.recent_texts(Scope::C2c, "USER_A", None, 0, 100).await.unwrap();
    assert_eq!(c2c, vec!["私聊消息"]);

    // 群与私聊互不串台
    let cross = store.recent_texts(Scope::C2c, "GROUP_A", None, 0, 100).await.unwrap();
    assert!(cross.is_empty(), "C2C 查询不应命中群消息: {cross:?}");
}

#[tokio::test]
async fn duplicate_message_id_is_stored_once() {
    let store = MessageStore::open(cfg(temp_db())).unwrap();

    for _ in 0..5 {
        store.record(msg("SAME", Scope::Group, "G", "重复推送", 1000));
    }
    assert_eq!(wait_for_rows(&store, 1).await, 1, "消息 id 是主键，重复推送只能入库一次");
}

#[tokio::test]
async fn purge_removes_only_messages_older_than_cutoff() {
    let store = MessageStore::open(cfg(temp_db())).unwrap();
    let now = 1_800_000_000i64;
    let cutoff = now - 365 * DAY_SECS as i64;

    store.record(msg("OLD", Scope::Group, "G", "两年前", cutoff - 1));
    store.record(msg("EDGE", Scope::Group, "G", "恰好一年", cutoff));
    store.record(msg("NEW", Scope::Group, "G", "昨天", now - DAY_SECS as i64));
    assert_eq!(wait_for_rows(&store, 3).await, 3);

    let deleted = store.purge_before(cutoff).await.unwrap();
    assert_eq!(deleted, 1, "只有严格早于 cutoff 的才该被删");

    let texts = store.recent_texts(Scope::Group, "G", None, 0, 100).await.unwrap();
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
    wait_for_rows(&store, 2).await;

    let texts = store
        .recent_texts(Scope::Group, "G", None, now - 30 * DAY_SECS as i64, 100)
        .await
        .unwrap();
    assert_eq!(texts, vec!["最近"], "窗口外的消息不应进入语料");
}

#[tokio::test]
async fn empty_content_is_excluded_from_corpus() {
    let store = MessageStore::open(cfg(temp_db())).unwrap();
    store.record(msg("E", Scope::Group, "G", "", 1000));
    store.record(msg("T", Scope::Group, "G", "有内容", 1001));
    wait_for_rows(&store, 2).await;

    let texts = store.recent_texts(Scope::Group, "G", None, 0, 100).await.unwrap();
    assert_eq!(texts, vec!["有内容"], "空正文不参与词云，但消息本身要入库");
}

#[tokio::test]
async fn long_content_is_truncated_before_storage() {
    let store = MessageStore::open(cfg(temp_db())).unwrap();
    let huge = "超".repeat(qqbot_store::MAX_CONTENT_CHARS + 3000);
    store.record(msg("L", Scope::Group, "G", &huge, 1000));
    wait_for_rows(&store, 1).await;

    let texts = store.recent_texts(Scope::Group, "G", None, 0, 10).await.unwrap();
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
        wait_for_rows(&store, 1).await;
    }
    // store 已 drop → 读写线程退出、连接关闭

    let store = MessageStore::open(cfg(path)).unwrap();
    assert_eq!(wait_for_rows(&store, 1).await, 1, "数据必须持久化到磁盘");
    let texts = store.recent_texts(Scope::Group, "G", None, 0, 10).await.unwrap();
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
    wait_for_rows(&store, 2).await;

    let handle = store.spawn_sweeper();
    // 等第一次 tick 完成
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.count().await.unwrap() > 1 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.abort();

    assert_eq!(store.count().await.unwrap(), 1, "超过保留期的消息应被定时任务删除");
    let texts = store.recent_texts(Scope::Group, "G", None, 0, 10).await.unwrap();
    assert_eq!(texts, vec!["刚刚"]);
}

/// 原始事件 JSON 要能落库并按会话查回来 —— 资源收录靠它找附件。
///
/// 附件不在 `content` 里，而类型又只声明了文档写到的字段，
/// 所以「原文」是唯一能事后还原完整消息的东西。
#[tokio::test]
async fn raw_payload_is_persisted_and_queryable() {
    let store = MessageStore::open(cfg(temp_db())).unwrap();

    let raw = r#"{"content":"","attachments":[{"url":"https://example.invalid/a.jpg"}]}"#;
    let mut with_raw = msg("M_RAW", Scope::Group, "G1", "", now_unix());
    with_raw.raw = Some(raw.to_string());
    store.record(with_raw);

    // 没有原文的消息（调用方没提供）不该被查出来，
    // 否则收录会拿到一条读不出附件的空壳。
    store.record(msg("M_PLAIN", Scope::Group, "G1", "纯文本", now_unix()));

    assert_eq!(wait_for_rows(&store, 2).await, 2);

    let raws = store.recent_raw(Scope::Group, "G1", 0, 10).await.unwrap();
    assert_eq!(raws.len(), 1, "只有带原文的那条应当被返回：{raws:?}");
    assert_eq!(raws[0], raw);

    // 会话与场景都要隔离，否则会捞到别的群的图。
    assert!(store.recent_raw(Scope::Group, "G2", 0, 10).await.unwrap().is_empty());
    assert!(store.recent_raw(Scope::C2c, "G1", 0, 10).await.unwrap().is_empty());
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
    assert_eq!(wait_for_rows(&store, 2).await, 2);

    // `None` = 不限发送者（本群词云）
    let all = store.recent_texts(Scope::Group, "G1", None, 0, 10).await.unwrap();
    assert_eq!(all.len(), 2, "本群词云应当拿到两个人的话：{all:?}");

    // `Some` = 只看这个人（我的词云）
    let mine = store.recent_texts(Scope::Group, "G1", Some("UA"), 0, 10).await.unwrap();
    assert_eq!(mine, vec!["甲说的话"], "我的词云只该有自己说的");

    // 发送者过滤与时间窗要能叠加，不能互相覆盖。
    let future = store
        .recent_texts(Scope::Group, "G1", Some("UA"), i64::MAX, 10)
        .await
        .unwrap();
    assert!(future.is_empty(), "时间窗应当仍然生效");

    // 没说过话的人拿到空，而不是别人的话。
    let stranger = store
        .recent_texts(Scope::Group, "G1", Some("UC"), 0, 10)
        .await
        .unwrap();
    assert!(stranger.is_empty(), "不该把别人的话算成他的");
}
