//! 番剧更新日历（E4）。
//!
//! 源项目用 Chromium 截图 `agedm.io/update`。这里改成**抓取 + 复用 `card.svg`**：
//! 纯 Rust 渲染是项目的硬约束（无 Chromium、无 Node、单二进制）。
//!
//! 页面没有内嵌 JSON（不是 Next/Nuxt），所以只能按 HTML 结构提取。
//! 用正则而不是引入 HTML 解析库：只需要标题与集数两个字段，
//! 而页面结构一旦变化，正则和解析库都得改 —— 多一个依赖不解决问题。

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use regex::Regex;
use serde_json::json;

/// 番剧插件配置。
#[derive(Debug, Clone)]
pub struct BangumiConfig {
    /// 一周更新页。
    pub update_url: String,
}

impl Default for BangumiConfig {
    fn default() -> Self {
        Self { update_url: "https://www.agedm.io/update".into() }
    }
}

/// 卡片最多列多少部。
///
/// 实测页面一次给 **130 部**左右，全画出来高度上万像素，没人看得清。
const MAX_ROWS: usize = 20;

/// 提取用的正则。
///
/// 每个条目长这样（属性顺序与空白会变，所以只锚定类名与内容）：
/// ```html
/// <span class="video_item--info rounded-1 text-truncate">第12集(完结)</span>
/// ... <a href=".../detail/20260179" class="... stretched-link">标题</a>
/// ```
const ITEM_PATTERN: &str =
    r#"class="video_item--info[^"]*">([^<]*)</span>[\s\S]*?stretched-link">([^<]*)</a>"#;

/// 解掉最常见的几个 HTML 实体。
///
/// **单次扫描**，不是串行 `replace`：串行替换会把 `&amp;lt;` 先变成 `&lt;`
/// 再变成 `<`，而正确结果是 `&lt;` —— 用户看到的就是字面的 `&lt;`。
/// 换句话说，串行替换会**双重解码**。
fn unescape(raw: &str) -> String {
    const ENTITIES: [(&str, char); 6] = [
        ("&amp;", '&'),
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&quot;", '"'),
        ("&#39;", '\''),
        ("&nbsp;", ' '),
    ];
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        match ENTITIES.iter().find(|(entity, _)| tail.starts_with(entity)) {
            Some((entity, ch)) => {
                out.push(*ch);
                rest = &tail[entity.len()..];
            }
            None => {
                // 不是已知实体就原样保留那个 `&`，继续往后扫。
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// 从更新页 HTML 里提取 `(标题, 集数)`。
pub fn parse_updates(html: &str) -> Vec<(String, String)> {
    let Ok(pattern) = Regex::new(ITEM_PATTERN) else {
        return Vec::new();
    };
    pattern
        .captures_iter(html)
        .map(|caps| (unescape(&caps[2]), unescape(&caps[1])))
        .filter(|(title, _)| !title.is_empty())
        .collect()
}

/// 是否是番剧命令。源项目的正则是 `^(今日|每日|最新)番剧$`。
pub fn is_bangumi_query(content: &str) -> bool {
    matches!(content.trim(), "今日番剧" | "每日番剧" | "最新番剧")
}

/// `Clone` 是必需的：同一个实例要挂到三条命令路由上。
#[derive(Clone)]
pub struct BangumiPlugin {
    config: BangumiConfig,
    http: reqwest::Client,
}

impl BangumiPlugin {
    pub fn new(config: BangumiConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    async fn fetch(&self) -> Result<String, String> {
        let res = self
            .http
            .get(&self.config.update_url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) qqbot-rs")
            .send()
            .await
            .map_err(|err| format!("请求失败：{err}"))?;
        let status = res.status();
        if !status.is_success() {
            return Err(format!("页面返回 HTTP {status}"));
        }
        res.text().await.map_err(|err| format!("读取页面失败：{err}"))
    }
}

#[async_trait]
impl Handler for BangumiPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        if !is_bangumi_query(ctx.content()) {
            return Handled::Next;
        }

        let html = match self.fetch().await {
            Ok(html) => html,
            Err(reason) => {
                tracing::warn!(error = %reason, "获取番剧更新失败");
                let _ = ctx.reply_text(format!("获取番剧更新失败：{reason}")).await;
                return Handled::Consumed;
            }
        };

        let updates = parse_updates(&html);
        if updates.is_empty() {
            // 提取为空几乎只有一种原因：站点改版。给出可操作的提示，
            // 而不是回一张空卡片让人以为没更新。
            tracing::warn!(bytes = html.len(), "番剧页面没有解析出条目，站点结构可能变了");
            let _ = ctx.reply_text("没有解析出番剧条目，页面结构可能变了").await;
            return Handled::Consumed;
        }

        let total = updates.len();
        let shown = total.min(MAX_ROWS);
        let rows: Vec<serde_json::Value> = updates
            .iter()
            .take(shown)
            .map(|(title, episode)| json!({ "label": title, "value": episode }))
            .collect();

        // 与 card.svg 的排版对齐：首行 y=156，行距 48，页脚在 height-24。
        let height = 156 + shown * 48 + 40;
        let footer = if total > shown {
            format!("共 {total} 部，仅显示前 {shown} 部")
        } else {
            format!("共 {total} 部")
        };
        let data = json!({
            "title": "番剧一周更新",
            "rows": rows,
            "width": 900,
            "height": height,
            "footer": footer,
        });

        if let Err(err) = ctx.reply_template("card.svg", data).await {
            tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "番剧卡片发送失败");
            // 渲染失败也要回话：静默失败会让用户以为机器人没收到命令。
            let _ = ctx.reply_text(format!("卡片生成失败：{err}")).await;
        }
        Handled::Consumed
    }

    fn name(&self) -> &'static str {
        "番剧日历"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 形状照实测的 agedm.io/update 片段。
    const FIXTURE: &str = r#"
<div class="col g-2 position-relative">
  <div class="video_item">
    <span class="video_item--info rounded-1 text-truncate">第12集(完结)</span>
  </div>
  <div class="video_item-title text-truncate text-center py-2">
    <a href="http://www.agedm.io/detail/20260179" class="link-light text-decoration-none stretched-link">柔光魔女股份有限公司 第二季</a>
  </div>
</div>
<div class="col g-2 position-relative">
  <div class="video_item">
    <span class="video_item--info rounded-1 text-truncate">第01集</span>
  </div>
  <div class="video_item-title text-truncate text-center py-2">
    <a href="http://www.agedm.io/detail/20260260" class="link-light text-decoration-none stretched-link">转生贵族 &amp; 鉴定</a>
  </div>
</div>
"#;

    #[test]
    fn parses_title_and_episode_pairs() {
        let items = parse_updates(FIXTURE);
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].0, "柔光魔女股份有限公司 第二季");
        assert_eq!(items[0].1, "第12集(完结)");
        assert_eq!(items[1].1, "第01集");
    }

    #[test]
    fn unescapes_html_entities_in_titles() {
        let items = parse_updates(FIXTURE);
        assert_eq!(items[1].0, "转生贵族 & 鉴定", "标题里的 &amp; 要还原");
    }

    #[test]
    fn unescape_handles_amp_first() {
        // `&amp;lt;` 应当还原成 `&lt;` 而不是 `<` —— 顺序错了就会双重解码。
        assert_eq!(unescape("&amp;lt;"), "&lt;");
        assert_eq!(unescape("&lt;b&gt;"), "<b>");
        assert_eq!(unescape("  x  "), "x");
    }

    #[test]
    fn empty_or_broken_html_yields_nothing() {
        assert!(parse_updates("").is_empty());
        assert!(parse_updates("<html><body>改版了</body></html>").is_empty());
    }

    #[test]
    fn recognizes_the_three_command_forms() {
        for text in ["今日番剧", "每日番剧", "最新番剧", "  今日番剧  "] {
            assert!(is_bangumi_query(text), "应当识别：{text}");
        }
        for text in ["番剧", "今日番剧列表", "看今日番剧", ""] {
            assert!(!is_bangumi_query(text), "不该识别：{text}");
        }
    }
}
