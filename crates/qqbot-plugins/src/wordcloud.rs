use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use jieba_rs::Jieba;
use qqbot_core::{Ctx, Handled, Handler};
use qqbot_render::{build_wordcloud_svg, WordItem};
use qqbot_store::{now_unix, MessageStore, Scope, MAX_TEXT_ROWS};

/// 每个会话保留的最近消息条数。
const HISTORY_LIMIT: usize = 500;
/// 进入词云的最少不同词数。
const MIN_DISTINCT: usize = 3;

/// 最多同时跟踪多少个会话，防止 `HashMap` 只增不减。
const MAX_SESSIONS: usize = 2048;

/// 语料仓库。会话数按 FIFO 上限淘汰，单会话消息数按 `limit` 滚动覆盖。
struct History {
    sessions: HashMap<String, VecDeque<String>>,
    /// 会话的插入顺序，用于 FIFO 淘汰。
    order: VecDeque<String>,
    max_sessions: usize,
}

impl History {
    fn new(max_sessions: usize) -> Self {
        Self { sessions: HashMap::new(), order: VecDeque::new(), max_sessions }
    }

    fn record(&mut self, key: &str, text: &str, limit: usize) {
        if !self.sessions.contains_key(key) {
            while self.sessions.len() >= self.max_sessions {
                match self.order.pop_front() {
                    Some(oldest) => {
                        self.sessions.remove(&oldest);
                    }
                    None => break,
                }
            }
            self.order.push_back(key.to_string());
        }

        let queue = self.sessions.entry(key.to_string()).or_default();
        if queue.len() >= limit {
            queue.pop_front();
        }
        queue.push_back(text.to_string());
    }

    fn texts(&self, key: &str) -> Vec<String> {
        self.sessions.get(key).map(|q| q.iter().cloned().collect()).unwrap_or_default()
    }

    fn len_of(&self, key: &str) -> usize {
        self.sessions.get(key).map(VecDeque::len).unwrap_or(0)
    }
}

/// 词云插件。
///
/// 一个 Handler 承担两个职责：
///
/// - 命令「词云」→ 渲染并发送图片
/// - 其余消息 → 静默累积语料，返回 [Handled::Next] 不打断责任链
#[derive(Clone)]
pub struct WordCloudPlugin {
    /// 长期语料来源。`None` 时退化为进程内内存语料。
    store: Option<Arc<MessageStore>>,
    /// 统计窗口：只看最近这段时间的消息。
    window: Duration,
    history: Arc<Mutex<History>>,
    /// 已知命令名。命令调用不该算作语料 —— 否则「词云」自己会变成高频词，
    /// 词云里出现「词云」是自指的噪声。
    commands: Arc<RwLock<Vec<String>>>,
    /// 分词器。`Jieba::new()` 要加载内置词典，只构造一次并跨 Handler 共享。
    jieba: Arc<Jieba>,
    limit: usize,
}

/// 默认统计窗口。
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(30 * 24 * 3600);

impl WordCloudPlugin {
    pub fn new() -> Self {
        Self::with_limit(HISTORY_LIMIT)
    }

    /// 指定语料来源与统计窗口。`store` 为 `None` 时只用内存语料。
    pub fn with_store(store: Option<Arc<MessageStore>>, window: Duration) -> Self {
        Self { store, window, ..Self::with_limit(HISTORY_LIMIT) }
    }

    pub fn with_limit(limit: usize) -> Self {
        Self {
            store: None,
            window: DEFAULT_WINDOW,
            history: Arc::new(Mutex::new(History::new(MAX_SESSIONS))),
            commands: Arc::new(RwLock::new(Vec::new())),
            jieba: Arc::new(Jieba::new()),
            limit,
        }
    }

    /// 注册完成后回填命令表（命令名可从 `RouteInfo.matcher` 推导）。
    pub fn set_commands(&self, commands: Vec<String>) {
        if let Ok(mut guard) = self.commands.write() {
            *guard = commands;
        }
    }

    /// 该消息是否是命令调用（首词命中任一命令）。
    fn is_command(&self, text: &str) -> bool {
        let first = text.split_whitespace().next().unwrap_or("");
        if first.is_empty() {
            return false;
        }
        self.commands
            .read()
            .map(|c| c.iter().any(|cmd| cmd == first))
            .unwrap_or(false)
    }

    fn filter_corpus(&self, texts: Vec<String>) -> Vec<String> {
        texts.into_iter().filter(|t| !self.is_command(t)).collect()
    }

    /// 当前跟踪的会话数（用于验证内存上限）。
    pub fn tracked_sessions(&self) -> usize {
        self.history.lock().map(|h| h.sessions.len()).unwrap_or(0)
    }

    fn record(&self, key: &str, text: &str) {
        let Ok(mut history) = self.history.lock() else { return };
        history.record(key, text, self.limit);
    }

    fn memory_texts(&self, key: &str) -> Vec<String> {
        let raw = self.history.lock().map(|h| h.texts(key)).unwrap_or_default();
        self.filter_corpus(raw)
    }

    /// 取该会话的语料。
    ///
    /// 优先走持久化存储（跨重启、可回溯整个窗口）；未启用存储或读取失败时，
    /// 回退到内存语料 —— 存储故障不该让功能完全不可用。
    async fn gather(&self, ctx: &Ctx) -> Vec<String> {
        let key = ctx.target.key();
        let Some(store) = &self.store else {
            return self.memory_texts(&key);
        };
        let scope = if ctx.target.is_group() { Scope::Group } else { Scope::C2c };
        // 同 store 的 cutoff：避免未检查减法在 release 下回绕。
        let window_secs = i64::try_from(self.window.as_secs()).unwrap_or(i64::MAX);
        let since = now_unix().saturating_sub(window_secs);
        match store.recent_texts(scope, ctx.target.id(), since, MAX_TEXT_ROWS).await {
            Ok(texts) => self.filter_corpus(texts),
            Err(err) => {
                tracing::warn!(error = %err, "读取消息存储失败，回退到内存语料");
                self.memory_texts(&key)
            }
        }
    }

    /// 统计该会话的词频。返回 (词表, 语料条数)。
    pub async fn collect(&self, ctx: &Ctx) -> (Vec<WordItem>, usize) {
        let texts = self.gather(ctx).await;
        let samples = texts.len();

        // 分词是纯 CPU 活，扔到 blocking 线程池，别占住异步运行时。
        let jieba = Arc::clone(&self.jieba);
        let counts = tokio::task::spawn_blocking(move || count_words(&jieba, &texts))
            .await
            .unwrap_or_default();

        (rank(counts), samples)
    }

    /// 只用内存语料统计（同步）。测试与「未启用存储」时使用。
    pub fn collect_memory(&self, key: &str) -> Vec<WordItem> {
        let texts = self.memory_texts(key);
        rank(count_words(&self.jieba, &texts))
    }

    pub fn sample_count(&self, key: &str) -> usize {
        self.history.lock().map(|h| h.len_of(key)).unwrap_or(0)
    }
}

/// 词频 → 词表：过滤停用词、按「重复词优先」筛选、确定性排序。
fn rank(counts: HashMap<String, u32>) -> Vec<WordItem> {
    let all: Vec<WordItem> = counts
        .into_iter()
        .filter(|(w, _)| !is_stopword(w))
        .map(|(w, c)| WordItem::new(w, c))
        .collect();

    // 正常情况只保留出现 >= 2 次的词（滤掉噪声）；
    // 但小群里重复词本来就少，此时退化为「不过滤词频」，
    // 否则新群第一次用「词云」只会看到「样本不足」。
    let repeated: Vec<WordItem> = all.iter().filter(|w| w.weight >= 2).cloned().collect();
    let mut items = if repeated.len() >= MIN_DISTINCT { repeated } else { all };

    // 确定性排序：HashMap 迭代顺序随机，仅按权重排会让同权重词的顺序每次不同。
    items.sort_by(|a, b| b.weight.cmp(&a.weight).then_with(|| a.text.cmp(&b.text)));
    items.truncate(60);
    items
}

fn count_words(jieba: &Jieba, texts: &[String]) -> HashMap<String, u32> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for text in texts {
        for token in tokenize(jieba, text) {
            *counts.entry(token).or_insert(0) += 1;
        }
    }
    counts
}

impl Default for WordCloudPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Handler for WordCloudPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        let key = ctx.target.key();

        // 分支一：词云命令
        if ctx.content().split_whitespace().next() == Some("词云") {
            let (words, samples) = self.collect(ctx).await;
            if words.len() < MIN_DISTINCT {
                // 默认机器人只能收到 **@ 它** 的消息；群主开启「接收所有消息」后
                // 会额外推送 GROUP_MESSAGE_CREATE（全量模式），语料随日常聊天累积。
                // 两种情况都要给出可操作的指引。
                let msg = format!("样本还太少（已收集 {samples} 条，需要至少 {MIN_DISTINCT} 个不同的词）。\n\n群里多聊几句即可；若群主没有开启「接收所有消息」，则需要 **@ 我** 说几句自然语言。");
                let _ = ctx.reply_markdown(msg).await;
                return Handled::Consumed;
            }

            let title = format!("{} 的群聊词云", ctx.sender_name());
            let svg = build_wordcloud_svg(&words, 900, 640, &title);
            tracing::info!(words = words.len(), "渲染词云");
            if let Err(err) = ctx.reply_svg(svg).await {
                tracing::warn!(
                    error = %err,
                    hint = err.hint().unwrap_or("-"),
                    "词云发送失败（渲染 / 上传 / 发送三阶段中的某一环）"
                );
            }
            return Handled::Consumed;
        }

        // 分支二：静默累积语料（只收群聊，且跳过明显是命令的短消息）
        if ctx.is_group() {
            let text = ctx.content();
            if text.chars().count() >= 2 && !text.starts_with('/') && !text.starts_with('／') {
                self.record(&key, text);
            }
        }
        Handled::Next
    }

    fn name(&self) -> &str {
        "词云"
    }
}

/// 中文分词（jieba 精确模式）。
///
/// ⚠️ 早期版本用「CJK 连续串切二元组」的启发式，会把「今天天气真不错」切成
/// 今天 / 天天 / 天气 / 气真 / 真不 / 不错 —— 其中 **气真、真不、天天 都不是词**，
/// 纯粹是滑窗切出来的垃圾，词云看起来就是乱码。
/// 现改用真实词典分词，得到 今天 / 天气 / 不错。
pub fn tokenize(jieba: &Jieba, text: &str) -> Vec<String> {
    // 精确模式 + HMM：搜索模式（cut_for_search）会把长词再切出子词，
    // 反而重新引入「天天」「真不」这类滑窗垃圾，因此不用。
    jieba
        .cut(text, true)
        .into_iter()
        // jieba 0.11 的 cut 返回 Vec<Token>，需要取出其中的 word 字段
        .map(|token| token.word)
        .filter_map(normalize_token)
        .collect()
}

/// 过滤与归一化单个分词结果。
fn normalize_token(raw: &str) -> Option<String> {
    let token = raw.trim();
    if token.is_empty() {
        return None;
    }

    // 单遍扫描代替 `Vec<char>`：本函数对**每个 token** 都会调用，
    // collect 一次就是一次无谓的堆分配（500 条消息 × 约 10 词 ≈ 5000 次）。
    let mut len = 0usize;
    let mut has_cjk = false;
    let mut all_cjk = true;
    for c in token.chars() {
        len += 1;
        if is_cjk(c) {
            has_cjk = true;
        } else {
            all_cjk = false;
        }
    }

    if has_cjk {
        // 中文词：至少两个字，且必须**全部**是 CJK
        //（借此滤掉单字，以及 jieba 切出的「abc中文」这类混合片段）
        if len < 2 || !all_cjk {
            return None;
        }
        Some(token.to_string())
    } else {
        // 拉丁词 / 数字：去掉标点后至少两个字符
        let mut cleaned = String::with_capacity(token.len());
        cleaned.extend(token.chars().filter(|c| c.is_alphanumeric()));
        if cleaned.chars().count() < 2 {
            return None;
        }
        Some(cleaned.to_lowercase())
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF | 0x2_0000..=0x2_FA1F
    )
}

fn is_stopword(word: &str) -> bool {
    const STOP: &[&str] = &[
        "什么", "这个", "那个", "就是", "可以", "没有", "我们", "你们", "他们", "自己",
        "一个", "不是", "还是", "但是", "因为", "所以", "如果", "已经", "现在", "知道",
        "the", "and", "you", "for", "that", "this", "with", "have", "http", "https", "www",
    ];
    STOP.contains(&word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_chinese_into_real_words() {
        let j = Jieba::new();
        let t = tokenize(&j, "群里的大家早上好呀");
        for word in ["群里", "大家", "早上好"] {
            assert!(t.contains(&word.to_string()), "缺少真词 {word}: {t:?}");
        }
        // 单字虚词应被过滤
        assert!(!t.contains(&"的".to_string()), "{t:?}");
        assert!(!t.contains(&"呀".to_string()), "{t:?}");
    }

    /// 回归：这些全是二元组滑窗的产物，不是词。
    #[test]
    fn no_sliding_window_junk() {
        let j = Jieba::new();
        let t = tokenize(&j, "今天天气真不错");
        for junk in ["天天", "气真", "真不"] {
            assert!(!t.contains(&junk.to_string()), "出现伪词 {junk}: {t:?}");
        }
    }

    #[test]
    fn tokenizes_ascii_words() {
        let j = Jieba::new();
        let t = tokenize(&j, "hello world Rust");
        assert!(t.contains(&"hello".to_string()));
        assert!(t.contains(&"world".to_string()));
        assert!(t.contains(&"rust".to_string()));
    }

    #[test]
    fn separates_cjk_and_ascii_runs() {
        let j = Jieba::new();
        let t = tokenize(&j, "我在用 rust 写机器人");
        assert!(t.contains(&"rust".to_string()), "{t:?}");
        assert!(t.contains(&"机器人".to_string()), "{t:?}");
    }

    #[test]
    fn drops_single_cjk_char_and_punctuation() {
        let j = Jieba::new();
        assert!(tokenize(&j, "的 了 ，。！？").is_empty(), "单字与标点不应产生 token");
        assert!(tokenize(&j, "，。！？  ").is_empty());
        assert_eq!(tokenize(&j, "abc"), vec!["abc".to_string()]);
        assert!(tokenize(&j, "a").is_empty(), "单个 ASCII 字符信息量太低");
    }

    #[test]
    fn collects_and_filters_by_frequency() {
        let p = WordCloudPlugin::new();
        for _ in 0..3 {
            p.record("group:G1", "群里的大家早上好");
        }
        p.record("group:G1", "只出现一次");
        let items = p.collect_memory("group:G1");
        let words: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
        assert!(words.contains(&"大家"), "{words:?}");
        assert!(!words.contains(&"出现"), "只出现 1 次的词应被过滤: {words:?}");
        assert!(items[0].weight >= 3);
    }

    #[test]
    fn collect_is_deterministic_across_runs() {
        let build = || {
            let p = WordCloudPlugin::new();
            for _ in 0..3 {
                p.record("group:G", "今天天气不错");
            }
            p.collect_memory("group:G").into_iter().map(|w| w.text).collect::<Vec<_>>()
        };
        assert_eq!(build(), build(), "词表顺序必须稳定，否则渲染缓存失效");
    }

    #[test]
    fn falls_back_to_all_words_when_repeats_are_scarce() {
        let p = WordCloudPlugin::new();
        p.record("group:G", "苹果香蕉橘子");
        p.record("group:G", "电脑键盘鼠标");
        let items = p.collect_memory("group:G");
        assert!(
            items.len() >= MIN_DISTINCT,
            "小样本应退化为不过滤词频，而不是返回空: {items:?}"
        );
    }

    #[test]
    fn command_invocations_are_excluded_from_corpus() {
        let p = WordCloudPlugin::new();
        p.set_commands(vec!["词云".into(), "骰子".into()]);
        p.record("group:G", "词云");
        p.record("group:G", "骰子 3d6");
        p.record("group:G", "群里的大家早上好");
        let words: Vec<String> = p.collect_memory("group:G").into_iter().map(|w| w.text).collect();
        assert!(!words.contains(&"词云".to_string()), "命令调用不该进语料: {words:?}");
        assert!(!words.contains(&"骰子".to_string()), "命令调用不该进语料: {words:?}");
        assert!(words.contains(&"大家".to_string()), "正常聊天要保留: {words:?}");
    }

    #[test]
    fn history_is_bounded() {
        let p = WordCloudPlugin::with_limit(3);
        for i in 0..10 {
            p.record("k", &format!("消息{i}"));
        }
        assert_eq!(p.sample_count("k"), 3);
    }

    #[test]
    fn tracked_sessions_are_capped() {
        let p = WordCloudPlugin::new();
        // 塞入远超上限的会话，验证内存不会无界增长
        for i in 0..(MAX_SESSIONS + 500) {
            p.record(&format!("group:G{i}"), "今天天气不错");
        }
        assert_eq!(p.tracked_sessions(), MAX_SESSIONS, "会话数必须被上限截断");
        // 最老的会话应已被淘汰，最新的仍在
        assert_eq!(p.sample_count("group:G0"), 0);
        assert!(p.sample_count(&format!("group:G{}", MAX_SESSIONS + 499)) > 0);
    }

    #[test]
    fn sessions_are_isolated() {
        let p = WordCloudPlugin::new();
        p.record("group:A", "苹果香蕉橘子");
        p.record("group:B", "电脑键盘鼠标");
        let a = p.collect_memory("group:A");
        assert!(a.iter().all(|i| !i.text.contains("键盘")));
    }
}
