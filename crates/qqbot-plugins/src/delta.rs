//! 三角洲行动（D1–D4）。
//!
//! 数据源是 `kkrb.net`（cq-bot 用的同一家）。**这个站点要求先握手**：
//! 必须依次访问 首页 → `?viewpage=view/overview` → `getMenu`，
//! 之后 `getOVData` 才会返回数据。少了 `getMenu` 那一步会稳定拿到
//! `{"code":-101,"msg":"系统繁忙，请稍后再试"}` —— 实测 6 次全失败，
//! 补上之后 2 次全成功。所以这不是限流，是**会话状态**。
//!
//! 四个子命令对应数据里的四块：
//! - `集市`   → `ariiData`（活动物品的当前价与建议价）
//! - `脑机`   → `bcicData`（容器消耗能量）
//! - `密码`   → `bdData`（各图密码门）
//! - `一图流` → 上面三块合成一张卡片

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

/// 数据缓存多久。与 cq-bot 的 5 分钟一致 ——
/// 密码门是按天更新的，集市价格变动也不快。
const CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct DeltaConfig {
    pub home_url: String,
    pub overview_url: String,
    pub menu_url: String,
    pub data_url: String,
}

impl Default for DeltaConfig {
    fn default() -> Self {
        Self {
            home_url: "https://www.kkrb.net/".into(),
            overview_url: "https://www.kkrb.net/?viewpage=view%2Foverview".into(),
            menu_url: "https://www.kkrb.net/getMenu".into(),
            data_url: "https://www.kkrb.net/getOVData".into(),
        }
    }
}

// ---- 响应类型 ----
// 全部 `#[serde(default)]`：上游随时可能加字段或让某块为空，
// 缺一块不该让整条命令失败。

#[derive(Debug, Clone, Deserialize)]
struct Envelope {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    data: Option<DeltaData>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeltaData {
    #[serde(default, rename = "bdData")]
    passwords: PasswordBlock,
    #[serde(default, rename = "bcicData")]
    containers: Vec<Container>,
    #[serde(default, rename = "ariiData")]
    market: Vec<MarketItem>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PasswordBlock {
    #[serde(default)]
    db: PasswordEntry,
    #[serde(default)]
    cgxg: PasswordEntry,
    #[serde(default)]
    bks: PasswordEntry,
    #[serde(default)]
    htjd: PasswordEntry,
    #[serde(default)]
    cxjy: PasswordEntry,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PasswordEntry {
    #[serde(default)]
    password: String,
    /// `20260930000002` 这种 14 位时间戳。
    #[serde(default)]
    updated: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Container {
    #[serde(default)]
    name: String,
    /// 上游给的是字符串而不是数字。
    #[serde(default)]
    energy: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct MarketItem {
    #[serde(default, rename = "activityName")]
    activity_name: String,
    #[serde(default, rename = "activityTime")]
    activity_time: String,
    #[serde(default, rename = "itemName")]
    item_name: String,
    // 上游把这个字段拼成了 `currectPrice`（少一个 r），照抄，别「顺手修正」。
    //
    // 类型是 `f64` 而不是 `i64`：实测真实数据里出现过 `51937.6`。
    // mock 用整数是测不出这一点的 —— 这正是要打真实接口的原因。
    #[serde(default, rename = "currectPrice")]
    currect_price: f64,
    #[serde(default, rename = "activitySuggestedPrice")]
    suggested_price: f64,
}

/// 解析 `getOVData` 的响应体。
///
/// `code != 1` 时返回错误 —— 上游用 `code` 而不是 HTTP 状态码表达失败，
/// 与 QQ 官方 API 一样，只看状态码会漏掉。
pub fn parse_overview(body: &str) -> Result<DeltaData, String> {
    let env: Envelope =
        serde_json::from_str(body).map_err(|err| format!("解析响应失败：{err}"))?;
    if env.code != 1 {
        return Err(format!("接口返回 code={}：{}", env.code, env.msg));
    }
    env.data.ok_or_else(|| "接口返回 code=1 但没有 data".to_string())
}

/// 价格：整数不带小数点，小数保留两位。
///
/// 上游的价格是浮点，但绝大多数是整数；一律 `{:.2}` 会让界面全是 `.00`。
pub fn format_price(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

/// `20260930000002` → `2026-09-30`。
///
/// 长度不足时原样返回：宁可显示得怪一点，也不要 panic 或者吞掉。
pub fn format_updated(raw: &str) -> String {
    if raw.len() < 8 || !raw.is_char_boundary(8) {
        return raw.to_string();
    }
    format!("{}-{}-{}", &raw[0..4], &raw[4..6], &raw[6..8])
}

/// 密码行的顺序与名称照抄 cq-bot（`DeltaForcePlugin.passwordHandler`）。
fn password_rows(data: &DeltaData) -> Vec<serde_json::Value> {
    let p = &data.passwords;
    [
        ("零号大坝", &p.db),
        ("长弓溪谷", &p.cgxg),
        ("巴 克 什", &p.bks),
        ("航天基地", &p.htjd),
        ("潮汐监狱", &p.cxjy),
    ]
    .into_iter()
    .filter(|(_, e)| !e.password.is_empty())
    .map(|(name, e)| json!({ "label": name, "value": e.password }))
    .collect()
}

fn container_rows(data: &DeltaData) -> Vec<serde_json::Value> {
    data.containers
        .iter()
        .map(|c| json!({ "label": c.name, "value": format!("{} 能量", c.energy) }))
        .collect()
}

fn market_rows(data: &DeltaData) -> Vec<serde_json::Value> {
    data.market
        .iter()
        .map(|m| {
            json!({
                "label": m.item_name,
                "value": format!(
                    "当前 {} / 建议 {}",
                    format_price(m.currect_price),
                    format_price(m.suggested_price)
                ),
            })
        })
        .collect()
}

/// 往 `rows` 里追加一个小标题与它下面的行；没有内容就整段不出现。
///
/// 空段落会留下一个孤零零的标题，比缺一段更难看。
fn push_section(rows: &mut Vec<serde_json::Value>, name: &str, body: Vec<serde_json::Value>) {
    if body.is_empty() {
        return;
    }
    rows.push(json!({ "section": true, "label": name }));
    rows.extend(body);
}

/// 一图流：把密码、集市、脑机拼成一张分段卡片。
///
/// cq-bot 是让 Puppeteer 截整个 `overview.html`；本项目**禁止引入 Chromium**
/// （AGENTS.md 硬性禁令第 7 条，纯 Rust 光栅化是刻意的硬约束），
/// 所以改成用同一份数据自己渲染 —— 内容一样，只是排版由我们决定。
pub fn overview_rows(data: &DeltaData) -> Vec<serde_json::Value> {
    let mut rows = Vec::new();
    push_section(&mut rows, "密码门", password_rows(data));
    push_section(&mut rows, "集市", market_rows(data));
    push_section(&mut rows, "脑机", container_rows(data));
    rows
}

#[derive(Default)]
struct DeltaState {
    /// 会话 cookie。握手时更新，之后所有请求复用。
    cookie: Option<String>,
    cache: Option<(Instant, DeltaData)>,
}

/// `Clone` 是必需的：同一个实例要挂到多条命令路由上。
#[derive(Clone)]
pub struct DeltaPlugin {
    config: DeltaConfig,
    http: reqwest::Client,
    // `tokio::sync::Mutex` 而不是 std 的：它允许跨 await 持有，
    // 正好用来把并发的抓取串起来，避免同时打好几次握手。
    state: Arc<Mutex<DeltaState>>,
}

impl DeltaPlugin {
    pub fn new(config: DeltaConfig, http: reqwest::Client) -> Self {
        Self { config, http, state: Arc::new(Mutex::new(DeltaState::default())) }
    }

    /// 取数据，带 5 分钟缓存。
    async fn data(&self) -> Result<DeltaData, String> {
        let mut state = self.state.lock().await;
        if let Some((at, data)) = &state.cache
            && at.elapsed() < CACHE_TTL
        {
            return Ok(data.clone());
        }

        // 握手三步。缺 getMenu 会稳定拿到 code=-101。
        self.handshake(&mut state).await?;
        let cookie = state.cookie.clone().unwrap_or_default();
        let body = self
            .post(&self.config.data_url, &cookie)
            .await
            .map_err(|err| format!("getOVData 请求失败：{err}"))?;
        let data = parse_overview(&body)?;
        state.cache = Some((Instant::now(), data.clone()));
        Ok(data)
    }

    /// 首页 → overview → getMenu，沿途收集 cookie。
    async fn handshake(&self, state: &mut DeltaState) -> Result<(), String> {
        for url in [&self.config.home_url, &self.config.overview_url] {
            let res = self
                .http
                .get(url)
                .header("User-Agent", UA)
                .header("Cookie", state.cookie.clone().unwrap_or_default())
                .send()
                .await
                .map_err(|err| format!("握手 {url} 失败：{err}"))?;
            absorb_cookies(state, &res);
            // 必须读掉响应体，否则连接可能不释放。
            let _ = res.text().await;
        }
        let cookie = state.cookie.clone().unwrap_or_default();
        let res = self
            .http
            .post(&self.config.menu_url)
            .header("User-Agent", UA)
            .header("Cookie", cookie.clone())
            .header("Origin", ORIGIN)
            .header("Referer", ORIGIN)
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("globalData=false")
            .send()
            .await
            .map_err(|err| format!("getMenu 请求失败：{err}"))?;
        absorb_cookies(state, &res);
        let _ = res.text().await;
        Ok(())
    }

    async fn post(&self, url: &str, cookie: &str) -> Result<String, String> {
        let res = self
            .http
            .post(url)
            .header("User-Agent", UA)
            .header("Cookie", cookie)
            .header("Origin", ORIGIN)
            .header("Referer", ORIGIN)
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("globalData=false")
            .send()
            .await
            .map_err(|err| format!("{err}"))?;
        let status = res.status();
        let body = res.text().await.map_err(|err| format!("{err}"))?;
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        Ok(body)
    }

    /// `template` 是 `card.svg`（两列）或 `sections.svg`（带小标题）。
    async fn render(
        &self,
        ctx: &Ctx,
        template: &str,
        title: &str,
        rows: Vec<serde_json::Value>,
        footer: &str,
    ) {
        if rows.is_empty() {
            let _ = ctx.reply_text("上游没有返回数据，稍后再试").await;
            return;
        }
        let height = 156 + rows.len() * 48 + 40;
        let data = json!({
            "title": title,
            "rows": rows,
            "width": 1000,
            "height": height,
            "footer": footer,
        });
        if let Err(err) = ctx.reply_template(template, data).await {
            tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "三角洲卡片发送失败");
            let _ = ctx.reply_text(format!("卡片生成失败：{err}")).await;
        }
    }

    async fn fetch_or_report(&self, ctx: &Ctx) -> Option<DeltaData> {
        match self.data().await {
            Ok(data) => Some(data),
            Err(reason) => {
                tracing::warn!(error = %reason, "三角洲数据获取失败");
                let _ = ctx.reply_text(format!("获取数据失败：{reason}")).await;
                None
            }
        }
    }
}

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const ORIGIN: &str = "https://www.kkrb.net/";

/// 把响应里的 Set-Cookie 合并进会话。
///
/// 同名覆盖，其余保留 —— 上游会分几次下发 `PHPSESSID` 与其它标记，
/// 整体替换会丢掉先前那几个。
fn absorb_cookies(state: &mut DeltaState, res: &reqwest::Response) {
    let mut jar: Vec<String> = state
        .cookie
        .take()
        .unwrap_or_default()
        .split("; ")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    for value in res.headers().get_all(reqwest::header::SET_COOKIE) {
        let Ok(raw) = value.to_str() else { continue };
        let Some(pair) = raw.split(';').next() else { continue };
        let name = pair.split('=').next().unwrap_or_default();
        jar.retain(|c| !c.starts_with(&format!("{name}=")));
        jar.push(pair.to_string());
    }
    state.cookie = Some(jar.join("; "));
}

#[async_trait]
impl Handler for DeltaPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        let Some(cmd) = ctx.content().split_whitespace().next() else {
            return Handled::Next;
        };
        match cmd {
            "三角洲集市" | "集市" => {
                let Some(data) = self.fetch_or_report(ctx).await else {
                    return Handled::Consumed;
                };
                let footer = data
                    .market
                    .first()
                    .map(|m| format!("{} · {}", m.activity_name, m.activity_time))
                    .unwrap_or_default();
                self.render(ctx, "card.svg", "三角洲集市", market_rows(&data), &footer).await;
                Handled::Consumed
            }
            "三角洲脑机" | "脑机" => {
                let Some(data) = self.fetch_or_report(ctx).await else {
                    return Handled::Consumed;
                };
                self.render(ctx, "card.svg", "三角洲脑机", container_rows(&data), "容器消耗能量")
                    .await;
                Handled::Consumed
            }
            "三角洲密码" | "密码" => {
                let Some(data) = self.fetch_or_report(ctx).await else {
                    return Handled::Consumed;
                };
                let footer = format!("更新于 {}", format_updated(&data.passwords.db.updated));
                self.render(ctx, "card.svg", "三角洲密码门", password_rows(&data), &footer).await;
                Handled::Consumed
            }
            "三角洲一图流" | "一图流" => {
                let Some(data) = self.fetch_or_report(ctx).await else {
                    return Handled::Consumed;
                };
                let footer = format!("更新于 {}", format_updated(&data.passwords.db.updated));
                self.render(ctx, "sections.svg", "三角洲一图流", overview_rows(&data), &footer)
                    .await;
                Handled::Consumed
            }
            _ => Handled::Next,
        }
    }

    fn name(&self) -> &'static str {
        "三角洲行动"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 形状照实测的 `getOVData` 响应裁剪。
    const FIXTURE: &str = concat!(
        r#"{"code":1,"msg":"获取成功","data":{"#,
        r#""bdData":{"db":{"password":"0533","updated":"20260930000002"},"cgxg":{"password":"0637","updated":"20260930000002"},"bks":{"password":"0593","updated":"20260930000002"},"htjd":{"password":"0774","updated":"20260930000002"},"cxjy":{"password":"0352","updated":"20260930000002"},"az3":{"password":"0030"}},"#,
        r#""bcicData":[{"name":"快递箱","pic":"./image/icon/container/kdx.webp","energy":"8"},{"name":"手提箱","pic":"./x.webp","energy":"16"}],"#,
        r#""ariiData":[{"activityName":"研发部门 - 集市","activityTime":"2026/09/25 - 2025/10/02","itemName":"锈迹斑斑的海盗铜币","currectPrice":36046,"activitySuggestedPrice":13052}]"#,
        r#"}}"#,
    );

    #[test]
    fn parses_the_overview_payload() {
        let data = parse_overview(FIXTURE).unwrap();
        assert_eq!(data.passwords.db.password, "0533");
        assert_eq!(data.containers.len(), 2);
        assert_eq!(data.market.len(), 1);
        assert_eq!(data.market[0].item_name, "锈迹斑斑的海盗铜币");
        assert_eq!(data.market[0].currect_price, 36046.0);
        assert_eq!(data.market[0].suggested_price, 13052.0);
    }

    #[test]
    fn a_non_one_code_is_an_error() {
        // 上游用 code 而不是 HTTP 状态码表达失败，只看状态码会漏掉。
        let body = r#"{"code":-101,"msg":"系统繁忙，请稍后再试"}"#;
        let err = parse_overview(body).unwrap_err();
        assert!(err.contains("-101"), "错误里要带 code: {err}");
        assert!(err.contains("系统繁忙"), "也要带上游的原文: {err}");
    }

    #[test]
    fn code_one_without_data_is_an_error() {
        assert!(parse_overview(r#"{"code":1}"#).is_err());
    }

    #[test]
    fn missing_blocks_do_not_break_parsing() {
        // 上游随时可能让某一块为空，缺一块不该让整条命令失败。
        let data = parse_overview(r#"{"code":1,"data":{}}"#).unwrap();
        assert!(data.containers.is_empty());
        assert!(password_rows(&data).is_empty(), "空密码不该产生空行");
    }

    /// 实跑验证：打**真实**站点，走完整的握手。
    ///
    /// ```text
    /// cargo test -p qqbot-plugins --lib -- --ignored live_delta
    /// ```
    ///
    /// 这一条是本次实现里最重要的测试：整件事的难点不在解析，
    /// 而在「必须先 getMenu 再 getOVData」这个握手顺序。
    /// mock 只能证明我按理解的顺序发了请求，证明不了上游认这个顺序。
    #[tokio::test]
    #[ignore = "需要网络"]
    async fn live_delta_handshake_returns_data() {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        let plugin = DeltaPlugin::new(DeltaConfig::default(), http);
        let data = plugin.data().await.expect("握手后应当拿到数据");

        assert!(data.passwords.db.password.len() >= 3, "密码门应当有值");
        assert!(!data.containers.is_empty(), "脑机数据不该为空");
        assert!(!data.market.is_empty(), "集市数据不该为空");
        assert!(!password_rows(&data).is_empty());

        // 第二次应当命中缓存：同一个实例再取一次不该再打网络。
        let again = plugin.data().await.expect("缓存命中");
        assert_eq!(again.passwords.db.password, data.passwords.db.password);
    }

    #[test]
    fn broken_json_is_an_error_not_a_panic() {
        assert!(parse_overview("不是 JSON").is_err());
    }

    #[test]
    fn overview_puts_the_three_blocks_in_one_card() {
        let data = parse_overview(FIXTURE).unwrap();
        let rows = overview_rows(&data);
        let sections: Vec<&str> = rows
            .iter()
            .filter(|r| r["section"] == true)
            .map(|r| r["label"].as_str().unwrap())
            .collect();
        assert_eq!(sections, vec!["密码门", "集市", "脑机"]);
        // 小标题本身不带右值。
        assert!(rows.iter().filter(|r| r["section"] == true).all(|r| r.get("value").is_none()));
        // 三个小标题 + 5 密码 + 1 集市 + 2 脑机。
        assert_eq!(rows.len(), 3 + 5 + 1 + 2);
    }

    #[test]
    fn overview_skips_empty_sections() {
        // 只有密码、没有集市与脑机时，不该留下两个孤零零的标题。
        let data = parse_overview(FIXTURE).unwrap();
        let mut sparse = data.clone();
        sparse.market.clear();
        sparse.containers.clear();
        let rows = overview_rows(&sparse);
        let sections: Vec<&str> = rows
            .iter()
            .filter(|r| r["section"] == true)
            .map(|r| r["label"].as_str().unwrap())
            .collect();
        assert_eq!(sections, vec!["密码门"]);
    }

    #[test]
    fn fractional_prices_keep_two_decimals() {
        // 真实数据里出现过 `51937.6`；mock 用整数时这个分支从没被走到过。
        assert_eq!(format_price(36046.0), "36046");
        assert_eq!(format_price(51937.6), "51937.60");
    }

    #[test]
    fn formats_the_update_stamp() {
        assert_eq!(format_updated("20260930000002"), "2026-09-30");
        assert_eq!(format_updated("20260930"), "2026-09-30");
        // 长度不足时原样返回，不 panic。
        assert_eq!(format_updated("2026"), "2026");
        assert_eq!(format_updated(""), "");
    }

    #[test]
    fn password_rows_keep_the_cq_bot_order() {
        let data = parse_overview(FIXTURE).unwrap();
        let rows = password_rows(&data);
        let labels: Vec<&str> = rows.iter().map(|r| r["label"].as_str().unwrap()).collect();
        assert_eq!(labels, vec!["零号大坝", "长弓溪谷", "巴 克 什", "航天基地", "潮汐监狱"]);
        assert_eq!(rows[0]["value"], "0533");
        // az3 是上游新加的图，cq-bot 没显示，这里也不显示。
        assert!(!rows.iter().any(|r| r["value"] == "0030"));
    }

    #[test]
    fn container_and_market_rows_read_their_fields() {
        let data = parse_overview(FIXTURE).unwrap();
        let c = container_rows(&data);
        assert_eq!(c[0]["label"], "快递箱");
        assert_eq!(c[0]["value"], "8 能量", "energy 上游给的是字符串");
        let m = market_rows(&data);
        assert_eq!(m[0]["value"], "当前 36046 / 建议 13052");
    }
}
