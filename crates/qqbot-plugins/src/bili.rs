//! B 站链接解析：发视频卡片。
//!
//! 从 cq-bot 的 `BLPlugin` 迁移。源项目同时处理动态 / 专栏，但那两个靠
//! Chromium 截图，归 C2（需要先写 resvg 模板）；这里只做视频卡片。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use qqbot_store::ResourceStore;
use serde::Deserialize;

use crate::timewin::{format_datetime, SHANGHAI_OFFSET};

/// 同一个会话里同一稿件多久之内不重复发卡片。
///
/// 群里同一条链接经常被几个人先后转发，不去重就是刷屏。
const DEDUP_TTL: Duration = Duration::from_secs(300);
/// 最多记住多少个会话，防止 `HashMap` 只增不减。
const DEDUP_MAX_TARGETS: usize = 2048;
/// 封面图大小上限。
const MAX_COVER_BYTES: usize = 32 * 1024 * 1024;

/// B 站插件配置。
// 不派生 Debug：里面的 ResourceStore 没有有意义的 Debug 表示。
#[derive(Clone)]
pub struct BiliConfig {
    /// 稿件信息接口（`bvid` 直接拼在后面）。
    pub view_api: String,
    /// UP 主信息接口（`uid` 直接拼在后面），用于订阅时校验 UID。
    pub card_api: String,
    /// 订阅表。`None` 表示未启用持久化，订阅命令会给出提示而不是静默失败。
    pub store: Option<Arc<ResourceStore>>,
}

impl Default for BiliConfig {
    fn default() -> Self {
        Self {
            view_api: "https://api.bilibili.com/x/web-interface/view?bvid=".into(),
            card_api: "https://api.bilibili.com/x/web-interface/card?mid=".into(),
            store: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct CardResponse {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Option<CardData>,
}

#[derive(Debug, Deserialize)]
struct CardData {
    #[serde(default)]
    card: Option<CardInfo>,
}

#[derive(Debug, Deserialize)]
struct CardInfo {
    #[serde(default)]
    name: String,
}

/// UID 必须是纯数字。
///
/// 先本地挡一道：明显不合法的输入没必要打一次必然失败的请求，
/// 而 B 站对异常请求会返回风控页（HTTP 412），错误信息很难看。
pub fn valid_uid(raw: &str) -> bool {
    !raw.is_empty() && raw.len() <= 20 && raw.chars().all(|c| c.is_ascii_digit())
}

#[derive(Debug, Deserialize)]
struct BiliResponse {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Option<BiliVideo>,
}

#[derive(Debug, Deserialize)]
pub struct BiliVideo {
    #[serde(default)]
    pub bvid: String,
    #[serde(default)]
    pub aid: i64,
    #[serde(default)]
    pub title: String,
    /// 封面图地址。
    #[serde(default)]
    pub pic: String,
    /// 投稿时间（Unix 秒）。
    #[serde(default)]
    pub pubdate: i64,
    #[serde(default)]
    pub owner: BiliOwner,
    #[serde(default)]
    pub stat: BiliStat,
}

#[derive(Debug, Default, Deserialize)]
pub struct BiliOwner {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct BiliStat {
    #[serde(default)]
    pub view: i64,
    #[serde(default)]
    pub danmaku: i64,
    #[serde(default)]
    pub coin: i64,
    #[serde(default)]
    pub like: i64,
    #[serde(default)]
    pub reply: i64,
    #[serde(default)]
    pub share: i64,
}

/// 中文标点。
///
/// 链接后面经常直接跟中文标点而没有空格（`看这个https://b23.tv/abc，不错`），
/// 只按空白切会把标点和后面的字一起吞进 URL。
fn is_cjk_punct(c: char) -> bool {
    matches!(
        c,
        '，' | '。'
            | '！'
            | '？'
            | '、'
            | '；'
            | '：'
            | '（'
            | '）'
            | '“'
            | '”'
            | '‘'
            | '’'
            | '《'
            | '》'
            | '【'
            | '】'
    )
}

/// 从消息里找出 B 站链接。
///
/// 只认两种域名：短链 `b23.tv/` 与完整域名 `bilibili.com/`。
/// 返回的链接**一定带协议头**，方便直接拿去请求。
pub fn find_link(content: &str) -> Option<String> {
    let host_at = ["b23.tv/", "bilibili.com/"]
        .iter()
        .filter_map(|host| content.find(host))
        .min()?;

    // 往前吃协议头；没有就当作裸域名，自己补 `https://`。
    let start = content[..host_at].rfind("http").unwrap_or(host_at);
    let end = content[start..]
        .find(|c: char| c.is_whitespace() || is_cjk_punct(c))
        .map_or(content.len(), |offset| start + offset);

    let raw = content[start..end].trim_end_matches(['/', '.', ',', ';']);
    if raw.is_empty() {
        return None;
    }
    if raw.starts_with("http") { Some(raw.to_string()) } else { Some(format!("https://{raw}")) }
}

/// 从链接里取 BV 号。
///
/// BV 号固定是 `BV` + 10 位字母数字，后面可能粘着查询参数，所以按长度截断。
pub fn extract_bvid(url: &str) -> Option<String> {
    let at = url.find("BV")?;
    let alnum: String = url[at..]
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect();
    (alnum.len() >= 12).then(|| alnum[..12].to_string())
}

/// 大数按「万 / 亿」缩写。
pub fn format_count(n: i64) -> String {
    if n <= 0 {
        return "0".to_string();
    }
    if n < 10_000 {
        n.to_string()
    } else if n < 100_000_000 {
        format!("{:.1}万", n as f64 / 10_000.0)
    } else {
        format!("{:.1}亿", n as f64 / 100_000_000.0)
    }
}

/// 卡片正文。
pub fn format_card(video: &BiliVideo) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", video.title);
    let _ = writeln!(out, "UP：{}", video.owner.name);
    let _ = writeln!(
        out,
        "播放：{} 弹幕：{}",
        format_count(video.stat.view),
        format_count(video.stat.danmaku)
    );
    let _ = writeln!(
        out,
        "投币：{} 点赞：{}",
        format_count(video.stat.coin),
        format_count(video.stat.like)
    );
    let _ = writeln!(
        out,
        "评论：{} 分享：{}",
        format_count(video.stat.reply),
        format_count(video.stat.share)
    );
    if video.pubdate > 0 {
        let _ = writeln!(out, "{}", format_datetime(video.pubdate, SHANGHAI_OFFSET));
    }
    let _ = writeln!(out, "av{}", video.aid);
    let _ = write!(out, "https://www.bilibili.com/video/{}", video.bvid);
    out
}

/// 从封面地址里取文件名。
fn cover_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let base = path.rsplit('/').next().unwrap_or("").trim();
    if base.is_empty() || !base.contains('.') { "cover.jpg".to_string() } else { base.to_string() }
}

/// B 站插件：链接展开 + 订阅管理。
///
/// `Clone` 是必需的：同一个实例要挂到监听器与两条命令路由上。
#[derive(Clone)]
pub struct BiliPlugin {
    config: BiliConfig,
    http: reqwest::Client,
    /// 会话 → (上一次的 BV 号, 时间)。
    seen: Arc<Mutex<HashMap<String, (String, Instant)>>>,
}

impl BiliPlugin {
    pub fn new(config: BiliConfig, http: reqwest::Client) -> Self {
        Self { config, http, seen: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// 该会话最近是否已经发过这个稿件。
    fn is_duplicate(&self, target: &str, bvid: &str) -> bool {
        let Ok(mut seen) = self.seen.lock() else { return false };
        let now = Instant::now();
        if let Some((last, at)) = seen.get(target)
            && last == bvid
            && now.duration_since(*at) < DEDUP_TTL
        {
            return true;
        }
        seen.retain(|_, (_, at)| now.duration_since(*at) < DEDUP_TTL);
        if seen.len() > DEDUP_MAX_TARGETS {
            seen.clear();
        }
        seen.insert(target.to_string(), (bvid.to_string(), now));
        false
    }

    /// `哔哩订阅 <uid>`。
    ///
    /// 与源项目的一处差异：**订阅与退订都要求群管理员**。
    /// 源项目只给订阅加了权限检查，退订谁都能点 —— 那意味着任何人都能
    /// 悄悄拆掉群里其他人配好的订阅。订阅是群级配置，两头都该由管理员管。
    async fn subscribe(&self, ctx: &Ctx) -> Handled {
        if !self.permit(ctx).await {
            return Handled::Consumed;
        }
        let Some(uid) = ctx.arg(0) else {
            let _ = ctx.reply_text("用法：哔哩订阅 <UID>").await;
            return Handled::Consumed;
        };
        if !valid_uid(uid) {
            let _ = ctx.reply_text("UID 必须是纯数字").await;
            return Handled::Consumed;
        }
        let Some(store) = &self.config.store else {
            let _ = ctx.reply_text("未启用持久化，订阅功能不可用").await;
            return Handled::Consumed;
        };

        let name = match self.fetch_up_name(uid).await {
            Ok(name) => name,
            Err(reason) => {
                tracing::warn!(error = %reason, uid, "校验 UID 失败");
                let _ = ctx.reply_text(format!("订阅失败：{reason}")).await;
                return Handled::Consumed;
            }
        };

        let group = ctx.target.id().to_string();
        match store.bili_subscribe(uid, &group, &name).await {
            Ok(true) => {
                let _ = ctx.reply_text(format!("UID: {uid}\n昵称: {name}\n订阅成功~")).await;
            }
            Ok(false) => {
                let _ = ctx.reply_text(format!("UID: {uid}\n昵称: {name}\n本群已经订阅过了")).await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "写入订阅失败");
                let _ = ctx.reply_text("订阅失败：数据库写入出错").await;
            }
        }
        Handled::Consumed
    }

    /// `哔哩退订 <uid>`。
    async fn unsubscribe(&self, ctx: &Ctx) -> Handled {
        if !self.permit(ctx).await {
            return Handled::Consumed;
        }
        let Some(uid) = ctx.arg(0) else {
            let _ = ctx.reply_text("用法：哔哩退订 <UID>").await;
            return Handled::Consumed;
        };
        let Some(store) = &self.config.store else {
            let _ = ctx.reply_text("未启用持久化，订阅功能不可用").await;
            return Handled::Consumed;
        };

        let group = ctx.target.id().to_string();
        match store.bili_unsubscribe(uid, &group).await {
            Ok(true) => {
                let _ = ctx.reply_text(format!("{uid} 退订成功")).await;
            }
            Ok(false) => {
                let _ = ctx.reply_text(format!("{uid} 未订阅")).await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "删除订阅失败");
                let _ = ctx.reply_text("退订失败：数据库写入出错").await;
            }
        }
        Handled::Consumed
    }

    /// 群管理员校验。
    ///
    /// 不通过时**回一条说明**：静默无视会让用户以为机器人没收到命令，
    /// 而实际上他只是没权限。
    async fn permit(&self, ctx: &Ctx) -> bool {
        if !ctx.is_group() {
            let _ = ctx.reply_text("订阅是群功能，请在群里使用").await;
            return false;
        }
        if !ctx.is_admin() {
            let _ = ctx.reply_text("只有群管理员能管理订阅").await;
            return false;
        }
        true
    }

    /// 校验 UID 并取昵称。
    async fn fetch_up_name(&self, uid: &str) -> Result<String, String> {
        let url = format!("{}{uid}", self.config.card_api);
        let res = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .header("Referer", "https://space.bilibili.com/")
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) qqbot-rs")
            .send()
            .await
            .map_err(|err| format!("请求失败：{err}"))?;
        let status = res.status();
        if !status.is_success() {
            return Err(format!("接口返回 HTTP {status}"));
        }
        let body = res.text().await.map_err(|err| format!("读取响应失败：{err}"))?;
        let parsed: CardResponse =
            serde_json::from_str(&body).map_err(|err| format!("解析响应失败：{err}"))?;
        if parsed.code != 0 {
            return Err(if parsed.message.is_empty() {
                "该 UID 不存在".to_string()
            } else {
                parsed.message
            });
        }
        let name = parsed
            .data
            .and_then(|d| d.card)
            .map(|c| c.name)
            .unwrap_or_default();
        if name.trim().is_empty() {
            return Err("该 UID 不存在".to_string());
        }
        Ok(name)
    }

    /// 把短链解析成真实地址。
    async fn resolve_short(&self, url: &str) -> Result<String, String> {
        let res = self
            .http
            .get(url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) qqbot-rs")
            .send()
            .await
            .map_err(|err| format!("解析短链失败：{err}"))?;
        Ok(res.url().to_string())
    }

    async fn fetch_video(&self, bvid: &str) -> Result<BiliVideo, String> {
        let url = format!("{}{bvid}", self.config.view_api);
        let res = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) qqbot-rs")
            .send()
            .await
            .map_err(|err| format!("请求稿件信息失败：{err}"))?;
        let status = res.status();
        if !status.is_success() {
            return Err(format!("稿件接口返回 HTTP {status}"));
        }
        let body = res.text().await.map_err(|err| format!("读取响应失败：{err}"))?;
        let parsed: BiliResponse =
            serde_json::from_str(&body).map_err(|err| format!("解析响应失败：{err}"))?;
        if parsed.code != 0 {
            return Err(if parsed.message.is_empty() {
                format!("接口返回 code={}", parsed.code)
            } else {
                parsed.message
            });
        }
        parsed.data.ok_or_else(|| "接口没有返回稿件数据".to_string())
    }

    async fn download_cover(&self, url: &str) -> Result<Vec<u8>, String> {
        let res = self
            .http
            .get(url)
            .header("Referer", "https://www.bilibili.com/")
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) qqbot-rs")
            .send()
            .await
            .map_err(|err| format!("下载封面失败：{err}"))?;
        let status = res.status();
        if !status.is_success() {
            return Err(format!("封面返回 HTTP {status}"));
        }
        let bytes = res
            .bytes()
            .await
            .map_err(|err| format!("读取封面失败：{err}"))?
            .to_vec();
        if bytes.is_empty() {
            return Err("封面内容为空".to_string());
        }
        if bytes.len() > MAX_COVER_BYTES {
            return Err(format!("封面过大（{} 字节）", bytes.len()));
        }
        Ok(bytes)
    }
}

#[async_trait]
impl Handler for BiliPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        match ctx.content().split_whitespace().next().unwrap_or_default() {
            "哔哩订阅" => return self.subscribe(ctx).await,
            "哔哩退订" => return self.unsubscribe(ctx).await,
            _ => {}
        }

        let Some(link) = find_link(ctx.content()) else {
            return Handled::Next;
        };

        // 短链要先跟一次重定向才知道指向哪。
        let resolved = if link.contains("b23.tv") {
            match self.resolve_short(&link).await {
                Ok(url) => url,
                Err(reason) => {
                    tracing::warn!(error = %reason, "B 站短链解析失败");
                    return Handled::Next;
                }
            }
        } else {
            link
        };

        // 不是视频（动态 / 专栏）就交给后面的插件 —— C2 还没做，
        // 这里返回 Next 而不是 Consumed，免得把链接消息吞掉却不回应。
        let Some(bvid) = extract_bvid(&resolved) else {
            return Handled::Next;
        };

        if self.is_duplicate(&ctx.target.key(), &bvid) {
            tracing::debug!(bvid, "同一会话刚刚发过这张卡片，跳过");
            return Handled::Consumed;
        }

        let video = match self.fetch_video(&bvid).await {
            Ok(video) => video,
            Err(reason) => {
                tracing::warn!(error = %reason, bvid, "获取稿件信息失败");
                return Handled::Next;
            }
        };

        let text = format_card(&video);
        // 封面拿不到不算失败：文字部分照样有价值。
        match self.download_cover(&video.pic).await {
            Ok(bytes) => {
                let name = cover_name(&video.pic);
                if let Err(err) = ctx
                    .reply_media_with_text(qqbot_media::FileType::Image, &name, &bytes, &text)
                    .await
                {
                    tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "B 站卡片发送失败");
                }
            }
            Err(reason) => {
                tracing::warn!(error = %reason, "封面下载失败，退化为纯文本");
                if let Err(err) = ctx.reply_text(text).await {
                    tracing::warn!(error = %err, "B 站卡片发送失败");
                }
            }
        }
        Handled::Consumed
    }

    fn name(&self) -> &'static str {
        "B 站卡片"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_must_be_pure_digits() {
        assert!(valid_uid("703007996"));
        assert!(valid_uid("1"));
        for bad in ["", "abc", "123abc", "-1", "12 34", "123456789012345678901"] {
            assert!(!valid_uid(bad), "不该接受：{bad}");
        }
    }

    #[test]
    fn finds_both_link_forms() {
        assert_eq!(find_link("看看这个"), None);
        assert_eq!(
            find_link("https://www.bilibili.com/video/BV1mokxBtEZh").as_deref(),
            Some("https://www.bilibili.com/video/BV1mokxBtEZh")
        );
        assert_eq!(find_link("https://b23.tv/abc123").as_deref(), Some("https://b23.tv/abc123"));
        // 裸域名要补协议头。
        assert_eq!(find_link("b23.tv/abc").as_deref(), Some("https://b23.tv/abc"));
    }

    #[test]
    fn stops_at_chinese_punctuation() {
        // 链接后面直接跟中文标点、没有空格 —— 很常见的粘贴形式。
        assert_eq!(
            find_link("看这个https://b23.tv/abc123，挺有意思").as_deref(),
            Some("https://b23.tv/abc123")
        );
        assert_eq!(
            find_link("https://b23.tv/abc123。").as_deref(),
            Some("https://b23.tv/abc123")
        );
    }

    #[test]
    fn extracts_the_bvid_and_tolerates_query_params() {
        assert_eq!(
            extract_bvid("https://www.bilibili.com/video/BV1mokxBtEZh").as_deref(),
            Some("BV1mokxBtEZh")
        );
        assert_eq!(
            extract_bvid("https://www.bilibili.com/video/BV1mokxBtEZh?p=2&t=3").as_deref(),
            Some("BV1mokxBtEZh")
        );
        assert_eq!(extract_bvid("https://www.bilibili.com/read/cv19282068"), None);
        assert_eq!(extract_bvid("https://t.bilibili.com/1151722276789420041"), None);
    }

    #[test]
    fn formats_counts_with_units() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(-5), "0", "负数不该显示成负的播放量");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(9_999), "9999");
        assert_eq!(format_count(12_345), "1.2万");
        assert_eq!(format_count(123_456_789), "1.2亿");
    }

    #[test]
    fn renders_the_card_text() {
        let video = BiliVideo {
            bvid: "BV1mokxBtEZh".into(),
            aid: 115914909417877,
            title: "标题".into(),
            pic: "https://i0.hdslb.com/bfs/archive/abc.jpg".into(),
            pubdate: 0,
            owner: BiliOwner { name: "UP主".into() },
            stat: BiliStat { view: 12_345, danmaku: 67, coin: 8, like: 9, reply: 10, share: 11 },
        };
        let text = format_card(&video);
        assert!(text.starts_with("标题\nUP：UP主\n"), "{text}");
        assert!(text.contains("播放：1.2万 弹幕：67"), "{text}");
        assert!(text.contains("投币：8 点赞：9"), "{text}");
        assert!(text.ends_with("https://www.bilibili.com/video/BV1mokxBtEZh"), "{text}");
        // pubdate 为 0 时不该打出一个 1970 年的时间。
        assert!(!text.contains("1970"), "{text}");
    }

    #[test]
    fn takes_the_cover_file_name() {
        assert_eq!(cover_name("https://i0.hdslb.com/bfs/archive/abc.jpg"), "abc.jpg");
        assert_eq!(cover_name("https://x/y.jpg?x=1"), "y.jpg");
        assert_eq!(cover_name(""), "cover.jpg");
        assert_eq!(cover_name("https://x/noext"), "cover.jpg");
    }
}
