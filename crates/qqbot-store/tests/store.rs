//! 存储层集成测试。
//!
//! 这里验证的是**行为契约**，不是实现细节：
//! 幂等、隔离、保留期边界、附件落行、以及「写入永不阻塞调用方」。
//!
//! 需要看到写入的断言一律先 `store.flush()`，**不 sleep 猜写线程什么时候提交** ——
//! 猜短了会假失败，猜长了白等。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use qqbot_store::{
    now_unix, MessageStore, NewAttachment, NewMessage, Scope, StoreConfig, DAY_SECS,
};

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
        attachments: Vec::new(),
        created_at: ts,
    }
}

/// 给消息挂上附件。数组下标就是入库后的 `ordinal`。
fn with_attachments(mut m: NewMessage, urls: &[&str]) -> NewMessage {
    m.attachments = urls
        .iter()
        .map(|url| NewAttachment {
            quoted: false,
            url: (*url).to_string(),
            content_type: Some("image/png".to_string()),
            filename: None,
            size: Some(16),
        })
        .collect();
    m
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

/// 附件落成行：`ordinal` 按 Vec 下标，同一条消息重推不产生重复行。
///
/// 直接读表而不是走公开 API：公开面只暴露「最新一条附件」，
/// 看不到 `ordinal`，也看不出有没有重复行 —— 而这两点正是本测试要守的东西。
/// `messages.raw` 也顺便在这里确认仍然在写（它还有别的用途）。
#[tokio::test]
async fn attachments_keep_their_order_and_reinsert_is_idempotent() {
    let path = temp_db();
    let store = MessageStore::open(cfg(path.clone())).unwrap();

    let mut m = with_attachments(
        msg("M_ATT", Scope::Group, "G1", "两张图", 1000),
        &["https://example.invalid/1.png", "https://example.invalid/2.png"],
    );
    m.raw = Some(r#"{"id":"M_ATT"}"#.to_string());
    store.record(m.clone());
    // QQ 会重推同一条消息 —— 附件行也必须幂等。
    store.record(m);
    store.flush().await.unwrap();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut stmt = conn
        .prepare("SELECT ordinal, url FROM message_attachments WHERE msg_id = 'M_ATT' ORDER BY ordinal")
        .unwrap();
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        rows,
        vec![
            (0, "https://example.invalid/1.png".to_string()),
            (1, "https://example.invalid/2.png".to_string()),
        ],
        "ordinal 必须按 Vec 下标写，且重推不产生重复行"
    );

    let raw: Option<String> = conn
        .query_row("SELECT raw FROM messages WHERE id = 'M_ATT'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(raw.as_deref(), Some(r#"{"id":"M_ATT"}"#), "原始事件 JSON 仍然要入库");

    // 同一条消息内，`ordinal` 大的更「新」。
    let latest = store.latest_attachment(Scope::Group, "G1", 0).await.unwrap().unwrap();
    assert_eq!(latest.url, "https://example.invalid/2.png");
}

/// `latest_attachment` 取的是**最新**那条、不跨会话、且遵守时间窗。
#[tokio::test]
async fn latest_attachment_is_newest_and_scoped() {
    let store = MessageStore::open(cfg(temp_db())).unwrap();
    let now = 1_800_000_000i64;

    store.record(with_attachments(
        msg("A_OLD", Scope::Group, "G1", "", now - 1000),
        &["https://example.invalid/old.png"],
    ));
    store.record(with_attachments(
        msg("A_NEW", Scope::Group, "G1", "", now),
        &["https://example.invalid/new.png"],
    ));
    store.record(with_attachments(
        msg("A_OTHER", Scope::Group, "G2", "", now),
        &["https://example.invalid/other.png"],
    ));
    store.record(with_attachments(
        msg("A_C2C", Scope::C2c, "G1", "", now),
        &["https://example.invalid/c2c.png"],
    ));
    // 没有附件的消息不该影响结果。
    store.record(msg("A_PLAIN", Scope::Group, "G1", "纯文本", now + 1));
    store.flush().await.unwrap();

    let latest = store.latest_attachment(Scope::Group, "G1", 0).await.unwrap().expect("应当有附件");
    assert_eq!(latest.url, "https://example.invalid/new.png", "取最新的那条");
    assert_eq!(latest.content_type.as_deref(), Some("image/png"));
    assert_eq!(latest.size, Some(16));
    assert!(!latest.quoted);
    assert_eq!(latest.created_at, now);

    // 会话与场景都要隔离，否则会捞到别的群的图。
    assert_eq!(
        store.latest_attachment(Scope::Group, "G2", 0).await.unwrap().unwrap().url,
        "https://example.invalid/other.png"
    );
    assert_eq!(
        store.latest_attachment(Scope::C2c, "G1", 0).await.unwrap().unwrap().url,
        "https://example.invalid/c2c.png"
    );

    // 时间窗：把窗口外的旧图挡在外面。
    assert_eq!(
        store.latest_attachment(Scope::Group, "G1", now).await.unwrap().unwrap().url,
        "https://example.invalid/new.png"
    );
    assert!(
        store.latest_attachment(Scope::Group, "G1", now + 1).await.unwrap().is_none(),
        "窗口内没有附件时应当是 None，而不是退回更早那条"
    );
}

/// 保留期清理必须连**附件行**一起删。
///
/// 附件表是自己的 `created_at`，按同一个 cutoff 直接删即可。漏掉这一步的话，
/// 消息删了、附件行却永远留着 —— 保留期只对一半数据生效，库只增不减。
#[tokio::test]
async fn purge_removes_attachment_rows_too() {
    let path = temp_db();
    let store = MessageStore::open(cfg(path.clone())).unwrap();
    let now = 1_800_000_000i64;
    let cutoff = now - 365 * DAY_SECS as i64;

    store.record(with_attachments(
        msg("A_OLD", Scope::Group, "G", "", cutoff - 1),
        &["https://example.invalid/old.png"],
    ));
    store.record(with_attachments(
        msg("A_EDGE", Scope::Group, "G", "", cutoff),
        &["https://example.invalid/edge.png"],
    ));
    store.flush().await.unwrap();

    assert_eq!(store.purge_before(cutoff).await.unwrap(), 1, "只有严格早于 cutoff 的消息被删");

    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut stmt = conn.prepare("SELECT msg_id FROM message_attachments ORDER BY msg_id").unwrap();
    let left: Vec<String> = stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(left, vec!["A_EDGE"], "边界上的附件要留着，过期的那条要跟着消息一起删");
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
