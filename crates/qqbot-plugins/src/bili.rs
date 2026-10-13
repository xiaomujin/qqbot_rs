//! B 站链接解析：发视频卡片。
//!
//! 卡片是 Markdown（`msg_type=2`）：封面直接写 B 站的公网 URL，由平台自己下载转存 ——
//! 我们不再下载再上传，因为 `msg_type=7` 的 `content` 是纯文本，塞不进 markdown，
//! 而 `msg_type=2` 又不能带富媒体 `file_info`，两者只能二选一。
//! 版式约定见 `docs/qq-bot-api-v2-protocol.md` 的「Markdown 消息」一节。
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

/// 卡片正文（Markdown）。
///
/// 版式：二级标题 → 封面 → `UP：昵称` → 六个数据分三行（每行左右两个）
/// → 行内链接 → BV 号一行 → 块引用收尾的投稿时间。
///
/// 每一条都是为**手机屏**定的（约定见 `docs/qq-bot-api-v2-protocol.md`
/// 的「Markdown 消息」一节）：手机一行大约只放得下 30 个显示单位（汉字算 2），
/// 超了就会折行，而断点多半落在分隔符上，看着像被截断 —— 所以数据每行只放两个，
/// UP 与时间不合成一行，长 URL 改成行内链接。
pub fn format_card(video: &BiliVideo) -> String {
    let title = sanitize_title(&video.title);
    let mut out = String::new();
    let _ = writeln!(out, "## {title}");

    let cover = https_url(&video.pic);
    if !cover.is_empty() {
        let _ = writeln!(out, "![{title} #320px #180px]({cover})");
    }

    // 空一行：markdown 里单个换行不保证断段。
    out.push('\n');
    let name = video.owner.name.trim();
    if !name.is_empty() {
        let _ = writeln!(out, "UP：{name}");
    }
    // 六个数据每行两个：一行三个是 30 个单位上下，手机上必然折行。
    let stat = &video.stat;
    let _ = writeln!(
        out,
        "播放 {} · 弹幕 {}",
        format_count(stat.view),
        format_count(stat.danmaku)
    );
    let _ = writeln!(
        out,
        "投币 {} · 点赞 {}",
        format_count(stat.coin),
        format_count(stat.like)
    );
    let _ = writeln!(
        out,
        "评论 {} · 分享 {}",
        format_count(stat.reply),
        format_count(stat.share)
    );

    // 正文里不再出现长 URL（它在手机上要占两行）：链接走行内语法，
    // BV 号另起一行，方便直接复制去 App 里搜。
    out.push('\n');
    let _ = writeln!(out, "[在 B 站观看](https://www.bilibili.com/video/{})", video.bvid);
    let _ = writeln!(out, "{}", video.bvid);

    // 投稿时间收在最底下，用块引用压住。
    if video.pubdate > 0 {
        out.push('\n');
        let _ = writeln!(out, "> {}", format_minute(video.pubdate, SHANGHAI_OFFSET));
    }

    // 末尾不留换行：多一个空行没有意义，还会让「最后一行是什么」不好断言。
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// 标题里的半角方括号会把 `![标题 …](url)` 提前闭合，整张图就不渲染了。
///
/// 换成全角而不是反斜杠转义：官方文档没写支持 `\[` 转义，而全角在中文标题里
/// 视觉几乎无差别。`*` `_` `~` 不处理 —— 它们顶多让几个字变斜体，弄不坏结构。
fn sanitize_title(raw: &str) -> String {
    raw.trim()
        .chars()
        .map(|c| match c {
            '[' => '［',
            ']' => '］',
            // 标题里真出现换行会把卡片切成两段，压成空格。
            '\n' | '\r' => ' ',
            other => other,
        })
        .collect()
}

/// 封面地址升到 https。
///
/// 接口给的是 `http://`（实测 9 个真实稿件全是），而平台转存发生在 https 会话里，
/// 混合内容被拦的概率不为零；`https://` 变体实测同样 200，改写零成本。
fn https_url(raw: &str) -> String {
    let raw = raw.trim();
    match raw.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => raw.to_string(),
    }
}

/// `YYYY-MM-DD HH:MM`。
///
/// 秒对「什么时候投的稿」没有信息量，还多占手机屏三个字符。
/// `format_datetime` 固定输出 19 个 ASCII 字节，截断安全；格式若变，下面的单测会先炸。
fn format_minute(ts: i64, offset: i64) -> String {
    let mut text = format_datetime(ts, offset);
    text.truncate(16);
    text
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
        // 卡片接口要 Referer + Accept，所以自己拼 RequestBuilder 再交给 http::send。
        let res = crate::http::send(
            self.http
                .get(&url)
                .header("Accept", "application/json")
                .header("Referer", "https://space.bilibili.com/"),
        )
        .await
        .map_err(|err| err.to_string())?;
        let body = res.text().await.map_err(|err| err.to_string())?;
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
    ///
    /// 必须走 http::send 拿原始 Response —— fetch_text 拿不到重定向后的最终 URL。
    async fn resolve_short(&self, url: &str) -> Result<String, String> {
        let res = crate::http::send(self.http.get(url))
            .await
            .map_err(|err| err.to_string())?;
        Ok(res.url().to_string())
    }

    async fn fetch_video(&self, bvid: &str) -> Result<BiliVideo, String> {
        let url = format!("{}{bvid}", self.config.view_api);
        let res = crate::http::send(self.http.get(&url).header("Accept", "application/json"))
            .await
            .map_err(|err| err.to_string())?;
        let body = res.text().await.map_err(|err| err.to_string())?;
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

        // 一条 markdown 消息搞定：封面是正文里的一行，平台转存失败也不会
        // 影响标题/数据/链接（`force_verify_image_resource` 默认 false），
        // 所以这里**不做**兜底重发 —— 与其它 markdown 插件一致。
        let md = format_card(&video);
        if let Err(err) = ctx.reply_markdown(md).await {
            tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "B 站卡片发送失败");
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

    /// 1700000000 = 2023-11-14 22:13:20 UTC，+8 小时即上海时区的次日清晨。
    fn sample() -> BiliVideo {
        BiliVideo {
            bvid: "BV1mokxBtEZh".into(),
            aid: 115914909417877,
            title: "标题".into(),
            pic: "https://i0.hdslb.com/bfs/archive/abc.jpg".into(),
            pubdate: 1_700_000_000,
            owner: BiliOwner { name: "UP主".into() },
            stat: BiliStat { view: 12_345, danmaku: 67, coin: 8, like: 9, reply: 10, share: 11 },
        }
    }

    #[test]
    fn renders_the_card_as_markdown() {
        assert_eq!(
            format_card(&sample()),
            "## 标题\n\
             ![标题 #320px #180px](https://i0.hdslb.com/bfs/archive/abc.jpg)\n\
             \n\
             UP：UP主\n\
             播放 1.2万 · 弹幕 67\n\
             投币 8 · 点赞 9\n\
             评论 10 · 分享 11\n\
             \n\
             [在 B 站观看](https://www.bilibili.com/video/BV1mokxBtEZh)\n\
             BV1mokxBtEZh\n\
             \n\
             > 2023-11-15 06:13"
        );
    }

    #[test]
    fn formats_the_time_to_the_minute() {
        assert_eq!(format_minute(1_700_000_000, SHANGHAI_OFFSET), "2023-11-15 06:13");
    }

    #[test]
    fn rewrites_the_http_cover_to_https() {
        // 接口给的封面是 http://（实测 9 个真实稿件全是），必须升到 https。
        let mut video = sample();
        video.pic = "http://i1.hdslb.com/bfs/archive/x.jpg".into();
        assert!(format_card(&video).contains("(https://i1.hdslb.com/bfs/archive/x.jpg)"));
        // 已经是 https 的不能被降级成 http。
        video.pic = "https://i1.hdslb.com/x.jpg".into();
        assert!(format_card(&video).contains("(https://i1.hdslb.com/x.jpg)"));
    }

    #[test]
    fn omits_the_image_line_when_the_cover_is_missing() {
        let mut video = sample();
        video.pic = "  ".into();
        let text = format_card(&video);
        assert!(!text.contains("!["), "{text}");
        assert!(text.starts_with("## 标题\n\nUP："), "{text}");
    }

    #[test]
    fn brackets_in_the_title_cannot_break_the_image_line() {
        // `[教程] xxx` 里的 `]` 会把 `![标题 …](url)` 提前闭合，图片就不渲染了。
        let mut video = sample();
        video.title = "[教程] 如何 [白嫖]".into();
        let text = format_card(&video);
        assert!(text.contains("## ［教程］ 如何 ［白嫖］"), "{text}");
        assert!(text.contains("![［教程］ 如何 ［白嫖］ #320px #180px]("), "{text}");
    }

    #[test]
    fn drops_the_time_when_pubdate_is_unknown() {
        let mut video = sample();
        video.pubdate = 0;
        let text = format_card(&video);
        assert!(text.contains("UP：UP主\n"), "{text}");
        // pubdate 为 0 时不该打出一个 1970 年的时间，也不该留下空的块引用。
        assert!(!text.contains("1970"), "{text}");
        assert!(!text.contains("> "), "{text}");
    }
}
