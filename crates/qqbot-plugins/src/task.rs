//! 塔科夫任务查询（B4）与数据导入（B5）。
//!
//! 数据来自 `json.tarkov.dev/regular/tasks` + `/traders` —— 与弹药（B6）同一份
//! 静态 JSON，GraphQL 后端挂掉时照样能用。
//!
//! **中文名尽力而为地取自 GraphQL。** cq-bot 的 `QUERY_TASKS` 就是
//! `tasks(lang: zh)`，打的是同一个 `api.tarkov.dev/graphql`。
//! 那个后端自 2026-09 起对所有查询返回 422，所以这一步失败是**正常情况**：
//! 失败就退回只有 slug 的静态 JSON，其它字段一个不少。
//!
//! ⚠️ **GraphQL 不可用时名字只有英文 slug**，原因同 `ammo.rs`：静态 JSON 的 `name` 是
//! 翻译键。这里用 `normalizedName`（`gunsmith-part-1`）与商人的 `normalizedName`
//! （`prapor`）。**任务目标的文字也是翻译键**，所以只存条数、不存文字。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use qqbot_store::{ResourceStore, TarkovTask};
use serde::Deserialize;
use serde_json::json;

use crate::ammo::{has_non_ascii, query_tokens};

/// 一次最多列几条。
const MAX_ROWS: usize = 14;

/// 任务插件配置。
#[derive(Clone)]
pub struct TaskConfig {
    pub tasks_url: String,
    pub traders_url: String,
    /// 中文名的来源。与 cq-bot 的 `QUERY_TASKS` 同一个接口。
    ///
    /// 它挂着的时候（2026-09 起一直 422）任务只有 slug，检索仍然可用。
    pub graphql_url: String,
    pub store: Option<Arc<ResourceStore>>,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            tasks_url: "https://json.tarkov.dev/regular/tasks".into(),
            traders_url: "https://json.tarkov.dev/regular/traders".into(),
            graphql_url: "https://api.tarkov.dev/graphql".into(),
            store: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct TasksResponse {
    #[serde(default)]
    data: Option<TasksData>,
}

#[derive(Debug, Deserialize)]
struct TasksData {
    #[serde(default)]
    tasks: HashMap<String, TaskEntry>,
}

#[derive(Debug, Deserialize)]
struct TaskEntry {
    #[serde(default, rename = "normalizedName")]
    normalized_name: String,
    #[serde(default)]
    trader: String,
    #[serde(default, rename = "minPlayerLevel")]
    min_player_level: i64,
    #[serde(default, rename = "kappaRequired")]
    kappa_required: bool,
    #[serde(default, rename = "lightkeeperRequired")]
    lightkeeper_required: bool,
    #[serde(default)]
    experience: i64,
    #[serde(default)]
    objectives: Vec<serde_json::Value>,
    #[serde(default, rename = "wikiLink")]
    wiki_link: String,
}

/// 只要 id 与中文名。cq-bot 的 `QUERY_TASKS` 也是 `tasks(lang: zh)`。
const QUERY_TASK_NAMES: &str = "{ tasks(lang: zh) { id name } }";

#[derive(Debug, Deserialize)]
struct NamesResponse {
    #[serde(default)]
    data: Option<NamesData>,
}

#[derive(Debug, Deserialize)]
struct NamesData {
    #[serde(default)]
    tasks: Vec<NameEntry>,
}

#[derive(Debug, Deserialize)]
struct NameEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
}

/// 把 GraphQL 返回的中文名并进任务表，返回填上了几条。
///
/// 按 **id** 对齐而不是按名字 —— 名字正是这里要替换的东西。
/// 对不上的条目保持 `None`（上游加了新任务而我们还没同步时会这样）。
pub fn merge_names(tasks: &mut [TarkovTask], body: &str) -> Result<usize, String> {
    let parsed: NamesResponse =
        serde_json::from_str(body).map_err(|err| format!("解析中文名失败：{err}"))?;
    let names = parsed.data.ok_or("中文名响应缺少 data")?.tasks;
    let map: HashMap<String, String> = names
        .into_iter()
        .filter(|n| !n.id.is_empty() && !n.name.trim().is_empty())
        .map(|n| (n.id, n.name))
        .collect();

    let mut filled = 0;
    for task in tasks.iter_mut() {
        if let Some(zh) = map.get(&task.id) {
            task.name_zh = Some(zh.clone());
            filled += 1;
        }
    }
    Ok(filled)
}

#[derive(Debug, Deserialize)]
struct TradersResponse {
    #[serde(default)]
    data: Option<HashMap<String, TraderEntry>>,
}

#[derive(Debug, Deserialize)]
struct TraderEntry {
    #[serde(default, rename = "normalizedName")]
    normalized_name: String,
}

/// 解析任务与商人两份 JSON，把商人 id 换成人看得懂的 slug。
///
/// 两份一起解析而不是分两次导入：任务里的 `trader` 是**裸 id**，
/// 没有商人表就只能显示一串十六进制。
pub fn parse_tasks(tasks_body: &str, traders_body: &str) -> Result<Vec<TarkovTask>, String> {
    let traders: TradersResponse =
        serde_json::from_str(traders_body).map_err(|err| format!("解析商人数据失败：{err}"))?;
    let trader_names: HashMap<String, String> = traders
        .data
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, t)| !t.normalized_name.trim().is_empty())
        .map(|(id, t)| (id, t.normalized_name))
        .collect();

    let parsed: TasksResponse =
        serde_json::from_str(tasks_body).map_err(|err| format!("解析任务数据失败：{err}"))?;
    let tasks = parsed.data.ok_or("任务响应缺少 data")?.tasks;

    let mut out: Vec<TarkovTask> = tasks
        .into_iter()
        .filter_map(|(id, task)| {
            let normalized_name = task.normalized_name.trim().to_string();
            if normalized_name.is_empty() {
                return None;
            }
            Some(TarkovTask {
                id,
                normalized_name,
                // 中文名要另外去 GraphQL 取，这里先留空。
                name_zh: None,
                // 解析不到商人就留空：上游加新商人时不该让整个导入失败。
                trader: trader_names.get(&task.trader).cloned().unwrap_or_default(),
                min_level: task.min_player_level,
                is_kappa: task.kappa_required,
                is_lightkeeper: task.lightkeeper_required,
                experience: task.experience,
                objectives: task.objectives.len() as i64,
                wiki_link: task.wiki_link,
            })
        })
        .collect();

    out.sort_by(|a, b| a.normalized_name.cmp(&b.normalized_name));
    Ok(out)
}

/// 卡片行：左边任务名，右边「商人 · 等级 · 标记」。
pub fn format_rows(items: &[TarkovTask]) -> Vec<serde_json::Value> {
    items
        .iter()
        .map(|t| {
            // `&str` 而不是 `String`：这几个片段要么是借用，要么是字面量，
            // 只有等级需要一次格式化，没必要每个都分配。
            let level = format!("{}级", t.min_level);
            let mut parts: Vec<&str> = Vec::new();
            if !t.trader.is_empty() {
                parts.push(&t.trader);
            }
            parts.push(&level);
            if t.is_kappa {
                parts.push("卡帕");
            }
            if t.is_lightkeeper {
                parts.push("灯塔");
            }
            json!({ "label": t.normalized_name, "value": parts.join(" · ") })
        })
        .collect()
}

/// `Clone` 是必需的：同一个实例要挂到两条命令路由上。
#[derive(Clone)]
pub struct TaskPlugin {
    config: TaskConfig,
    http: reqwest::Client,
}

impl TaskPlugin {
    pub fn new(config: TaskConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    /// `更新任务`：整表重导。
    async fn reload(&self, ctx: &Ctx) -> Handled {
        if !ctx.is_group() || !ctx.is_admin() {
            let _ = ctx.reply_text("只有群管理员能更新任务数据").await;
            return Handled::Consumed;
        }
        let Some(store) = &self.config.store else {
            let _ = ctx.reply_text("未启用持久化，任务功能不可用").await;
            return Handled::Consumed;
        };

        let (tasks_body, traders_body) = match tokio::try_join!(
            self.fetch(&self.config.tasks_url),
            self.fetch(&self.config.traders_url)
        ) {
            Ok(pair) => pair,
            Err(reason) => {
                tracing::warn!(error = %reason, "下载任务数据失败");
                let _ = ctx.reply_text(format!("下载失败：{reason}")).await;
                return Handled::Consumed;
            }
        };

        let mut items = match parse_tasks(&tasks_body, &traders_body) {
            Ok(items) if !items.is_empty() => items,
            Ok(_) => {
                let _ = ctx.reply_text("解析出 0 条任务，上游格式可能变了").await;
                return Handled::Consumed;
            }
            Err(reason) => {
                tracing::warn!(error = %reason, "解析任务数据失败");
                let _ = ctx.reply_text(format!("解析失败：{reason}")).await;
                return Handled::Consumed;
            }
        };

        // 中文名**尽力而为**：cq-bot 的 `QUERY_TASKS` 走的就是这个接口，
        // 而它自 2026-09 起对所有查询返回 422。失败不影响其它字段。
        let zh = match self.fetch_names().await {
            Ok(body) => match merge_names(&mut items, &body) {
                Ok(n) => format!("，其中 {n} 条带中文名"),
                Err(reason) => {
                    tracing::warn!(error = %reason, "任务中文名解析失败");
                    "，中文名不可用".to_string()
                }
            },
            Err(reason) => {
                tracing::warn!(error = %reason, "任务中文名获取失败，只按 slug 检索");
                "，中文名不可用".to_string()
            }
        };

        match store.replace_tasks(items).await {
            Ok(count) => {
                let _ = ctx.reply_text(format!("任务数据已更新，共 {count} 条{zh}")).await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "写入任务数据失败");
                let _ = ctx.reply_text("写入失败，稍后再试").await;
            }
        }
        Handled::Consumed
    }

    /// `查任务 <关键词>`。
    async fn search(&self, ctx: &Ctx) -> Handled {
        let query = ctx.content().trim().strip_prefix("查任务").unwrap_or("").trim();
        let tokens = query_tokens(query);
        if tokens.is_empty() {
            let _ = ctx.reply_text("用法：查任务 <名称片段>，例如 查任务 gunsmith").await;
            return Handled::Consumed;
        }
        let Some(store) = &self.config.store else {
            let _ = ctx.reply_text("未启用持久化，任务功能不可用").await;
            return Handled::Consumed;
        };

        let items = match store.search_tasks(tokens, MAX_ROWS).await {
            Ok(items) => items,
            Err(err) => {
                tracing::warn!(error = %err, "检索任务失败");
                let _ = ctx.reply_text("查询失败，稍后再试").await;
                return Handled::Consumed;
            }
        };

        if items.is_empty() {
            let hint = match store.task_count().await {
                Ok(0) => "任务数据还没导入，请管理员发「更新任务」",
                // 上游静态数据里**没有中文**（`name` 是翻译键），只有 GraphQL
                // 的 `lang: zh` 会解析它，而那个后端挂着。打了中文要说明白，
                // 否则用户会以为是自己打错了。
                _ if has_non_ascii(query) => {
                    "任务名只有英文 slug（如 first-in-line）—— 上游静态数据里没有中文，"
                }
                _ => "没有匹配的任务，换个关键词试试",
            };
            let _ = ctx.reply_text(hint).await;
            return Handled::Consumed;
        }

        let rows = format_rows(&items);
        let height = 156 + rows.len() * 48 + 40;
        let data = json!({
            "title": format!("任务 · {query}"),
            "rows": rows,
            "width": 1000,
            "height": height,
            "footer": format!("{} 条结果 · 商人 / 等级 / 标记", items.len()),
        });
        if let Err(err) = ctx.reply_template("card.svg", data).await {
            tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "任务卡片发送失败");
            let _ = ctx.reply_text(format!("卡片生成失败：{err}")).await;
        }
        Handled::Consumed
    }

    /// 取中文名。**失败是预期内的** —— 那个后端一直在 422。
    async fn fetch_names(&self) -> Result<String, String> {
        let res = self
            .http
            .post(&self.config.graphql_url)
            .header("Accept", "application/json")
            .json(&serde_json::json!({ "query": QUERY_TASK_NAMES }))
            .send()
            .await
            .map_err(|err| format!("请求失败：{err}"))?;
        let status = res.status();
        let body = res.text().await.map_err(|err| format!("读取失败：{err}"))?;
        if !status.is_success() {
            let snippet: String = body.chars().take(120).collect();
            return Err(format!("HTTP {status}：{snippet}"));
        }
        Ok(body)
    }

    async fn fetch(&self, url: &str) -> Result<String, String> {
        let res = self
            .http
            .get(url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) qqbot-rs")
            .send()
            .await
            .map_err(|err| format!("{url} 请求失败：{err}"))?;
        let status = res.status();
        if !status.is_success() {
            return Err(format!("{url} 返回 HTTP {status}"));
        }
        res.text().await.map_err(|err| format!("{url} 读取失败：{err}"))
    }
}

#[async_trait]
impl Handler for TaskPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        match ctx.content().split_whitespace().next().unwrap_or_default() {
            "更新任务" => self.reload(ctx).await,
            "查任务" => self.search(ctx).await,
            _ => Handled::Next,
        }
    }

    fn name(&self) -> &'static str {
        "塔科夫任务"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRADERS: &str = r#"{"data":{"t1":{"normalizedName":"prapor"},"t2":{"normalizedName":""}}}"#;

    fn tasks_json() -> String {
        concat!(
            r#"{"data":{"tasks":{"#,
            r#""k1":{"normalizedName":"gunsmith-part-1","trader":"t1","minPlayerLevel":5,"kappaRequired":true,"lightkeeperRequired":false,"experience":3000,"objectives":[{},{}],"wikiLink":"https://x/1"},"#,
            r#""k2":{"normalizedName":"first-in-line","trader":"t9","minPlayerLevel":1,"kappaRequired":false,"lightkeeperRequired":true,"experience":500,"objectives":[]},"#,
            r#""k3":{"normalizedName":"","minPlayerLevel":1}"#,
            r#"}}}"#,
        )
        .to_string()
    }

    #[test]
    fn resolves_trader_ids_and_sorts() {
        let items = parse_tasks(&tasks_json(), TRADERS).unwrap();
        assert_eq!(items.len(), 2, "没有 slug 的条目应当被丢掉: {items:?}");
        assert_eq!(items[0].normalized_name, "first-in-line", "应当按名称排序");
        assert_eq!(items[1].normalized_name, "gunsmith-part-1");
        assert_eq!(items[1].trader, "prapor", "商人 id 要换成 slug");
    }

    #[test]
    fn unknown_trader_becomes_empty_not_an_error() {
        // 上游加新商人时不该让整个导入失败。
        let items = parse_tasks(&tasks_json(), TRADERS).unwrap();
        assert_eq!(items[0].trader, "");
    }

    #[test]
    fn maps_the_structured_fields() {
        let items = parse_tasks(&tasks_json(), TRADERS).unwrap();
        let g = items.iter().find(|t| t.normalized_name == "gunsmith-part-1").unwrap();
        assert_eq!(g.min_level, 5);
        assert!(g.is_kappa);
        assert!(!g.is_lightkeeper);
        assert_eq!(g.experience, 3000);
        assert_eq!(g.objectives, 2, "目标文字是翻译键，只存条数");
        assert_eq!(g.wiki_link, "https://x/1");
    }

    /// 实跑验证：打**真实**接口（任务 + 商人两份）。
    ///
    /// ```text
    /// cargo test -p qqbot-plugins --lib -- --ignored live_tasks
    /// ```
    #[tokio::test]
    #[ignore = "需要网络"]
    async fn live_tasks_endpoint_parses() {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap();
        let mut bodies = Vec::new();
        for url in [
            "https://json.tarkov.dev/regular/tasks",
            "https://json.tarkov.dev/regular/traders",
        ] {
            bodies.push(
                http.get(url)
                    .header("User-Agent", "Mozilla/5.0 qqbot-rs")
                    .send()
                    .await
                    .unwrap_or_else(|e| panic!("{url} 请求失败：{e}"))
                    .text()
                    .await
                    .unwrap_or_else(|e| panic!("{url} 读取失败：{e}")),
            );
        }
        let items = parse_tasks(&bodies[0], &bodies[1]).expect("解析失败");
        assert!(items.len() > 400, "应当解析出四百个以上任务，实际 {}", items.len());
        // 商人解析必须真的生效 —— 否则界面上只会是一串十六进制 id。
        let resolved = items.iter().filter(|t| !t.trader.is_empty()).count();
        assert!(resolved > 400, "应当有四百个以上任务解析出商人，实际 {resolved}");
        let kappa = items.iter().filter(|t| t.is_kappa).count();
        assert!(kappa > 0, "应当有卡帕任务");
    }

    #[test]
    fn merges_chinese_names_by_id() {
        let mut items = parse_tasks(&tasks_json(), TRADERS).unwrap();
        assert!(items.iter().all(|t| t.name_zh.is_none()), "静态 JSON 里没有中文");

        // 形状照实测的 `tasks(lang: zh)` 响应。
        let body = r#"{"data":{"tasks":[{"id":"k1","name":"彻夜难眠"},{"id":"k2","name":"第一梯队"}]}}"#;
        assert_eq!(merge_names(&mut items, body).unwrap(), 2);

        let by_slug = |s: &str| items.iter().find(|t| t.normalized_name == s).unwrap();
        assert_eq!(by_slug("gunsmith-part-1").name_zh.as_deref(), Some("彻夜难眠"));
        assert_eq!(by_slug("first-in-line").name_zh.as_deref(), Some("第一梯队"));
    }

    #[test]
    fn merge_ignores_unknown_and_empty_entries() {
        let mut items = parse_tasks(&tasks_json(), TRADERS).unwrap();
        // 上游有而我们没有的 id、以及空名字，都该被忽略而不是写成空串。
        let body = r#"{"data":{"tasks":[{"id":"不存在","name":"幽灵"},{"id":"k1","name":"   "},{"id":"k2","name":"第一梯队"}]}}"#;
        assert_eq!(merge_names(&mut items, body).unwrap(), 1);
        let by_slug = |s: &str| items.iter().find(|t| t.normalized_name == s).unwrap();
        assert_eq!(by_slug("first-in-line").name_zh.as_deref(), Some("第一梯队"));
        assert!(by_slug("gunsmith-part-1").name_zh.is_none(), "空名字不该写成空串");
    }

    #[test]
    fn merge_reports_broken_input() {
        let mut items = parse_tasks(&tasks_json(), TRADERS).unwrap();
        assert!(merge_names(&mut items, "不是 JSON").is_err());
        assert!(merge_names(&mut items, r#"{"data":null}"#).is_err());
    }

    #[test]
    fn broken_input_is_an_error_not_a_panic() {
        assert!(parse_tasks("不是 JSON", TRADERS).is_err());
        assert!(parse_tasks(&tasks_json(), "不是 JSON").is_err());
        assert!(parse_tasks(r#"{"data":null}"#, TRADERS).is_err());
    }

    #[test]
    fn rows_show_trader_level_and_flags() {
        let items = parse_tasks(&tasks_json(), TRADERS).unwrap();
        let rows = format_rows(&items);
        assert_eq!(rows[0]["label"], "first-in-line");
        assert_eq!(rows[0]["value"], "1级 · 灯塔", "没有商人时不该出现空的分隔符");
        assert_eq!(rows[1]["value"], "prapor · 5级 · 卡帕");
    }
}
