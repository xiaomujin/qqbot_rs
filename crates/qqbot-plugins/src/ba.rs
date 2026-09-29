//! 蔚蓝档案（BA）图片查询。
//!
//! 从 cq-bot 的 `BaPlugin` 迁移。源项目把「总力战」「日历」也放在这里，
//! 但那两个靠 Chromium 截图（E1/E2 已列入不做），所以本模块只有图片查询。

use std::fmt::Write as _;

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use serde::Deserialize;

/// BA 插件配置。
///
/// 两个地址都可注入，否则端到端测试只能打真实接口 ——
/// 那既慢又不稳定，还会在 CI 里制造无关失败。
#[derive(Debug, Clone)]
pub struct BaConfig {
    /// 查询接口（不含 `name` 参数）。
    pub api: String,
    /// 图片 CDN 前缀。接口返回的 `content` 是相对路径，拼在它后面。
    pub cdn: String,
}

impl Default for BaConfig {
    fn default() -> Self {
        Self {
            api: "https://arona.diyigemt.com/api/v2/image?name=".into(),
            cdn: "https://arona.cdn.diyigemt.com/image/s".into(),
        }
    }
}

/// 一条查询最多发几张图。
///
/// 群聊被动回复窗口只有 **5 次**，而模糊搜索结果动辄 8 条。
/// 真发 8 张会有一半被服务端拒掉，用户看到的是「发到一半断了」。
const MAX_IMAGES: usize = 3;

/// 单张图片大小上限。实测常见图 2.3MB，留足余量但挡住异常大文件。
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// 接口响应。
#[derive(Debug, Deserialize)]
struct BaResponse {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Vec<BaImage>,
}

/// 一条结果。
#[derive(Debug, Deserialize)]
pub struct BaImage {
    #[serde(default)]
    pub name: String,
    /// `file`（图片）或 `plain`（纯文本）。
    #[serde(default, rename = "type")]
    pub kind: String,
    /// `file` 时是相对路径，`plain` 时是正文。
    #[serde(default)]
    pub content: String,
}

/// 解析 `ba <名>`。`None` 表示不是这个命令。
///
/// 源项目的正则是 `^(?i)ba (?<text>.+)`：大小写不敏感、必须有空格、
/// 后面必须有内容。照抄这个严格度 —— 全量模式下 `ba` 开头的英文句子并不少见。
pub fn parse_query(content: &str) -> Option<&str> {
    let trimmed = content.trim();
    let (head, rest) = trimmed.split_at_checked(3)?;
    if !head.eq_ignore_ascii_case("ba ") {
        return None;
    }
    let name = rest.trim();
    if name.is_empty() { None } else { Some(name) }
}

/// 从接口返回的路径里取文件名：`/student_rank/爱丽丝.png` → `爱丽丝.png`。
pub fn file_name_from_path(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path).trim();
    if base.is_empty() { "ba.png".to_string() } else { base.to_string() }
}

/// 模糊搜索时列出候选名字。
pub fn format_candidates(images: &[BaImage]) -> String {
    let mut out = String::from("是想问什么呢：\n");
    for image in images {
        let _ = writeln!(out, "{}", image.name);
    }
    out
}

pub struct BaPlugin {
    config: BaConfig,
    http: reqwest::Client,
}

impl BaPlugin {
    pub fn new(config: BaConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    async fn fetch(&self, name: &str) -> Result<BaResponse, String> {
        // 名字要转义：用户可能输入 `&`、空格、日文假名。
        let url = format!("{}{}", self.config.api, urlencode(name));
        let res = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|err| format!("请求失败：{err}"))?;
        let status = res.status();
        if !status.is_success() {
            return Err(format!("接口返回 HTTP {status}"));
        }
        let body = res.text().await.map_err(|err| format!("读取响应失败：{err}"))?;
        serde_json::from_str(&body).map_err(|err| format!("解析响应失败：{err}"))
    }

    async fn download(&self, path: &str) -> Result<(String, Vec<u8>), String> {
        let url = format!("{}{}", self.config.cdn, urlencode_path(path));
        let res = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|err| format!("下载失败：{err}"))?;
        let status = res.status();
        if !status.is_success() {
            return Err(format!("下载返回 HTTP {status}"));
        }
        let bytes = res
            .bytes()
            .await
            .map_err(|err| format!("读取图片失败：{err}"))?
            .to_vec();
        if bytes.is_empty() {
            return Err("图片内容为空".to_string());
        }
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(format!("图片过大（{} 字节）", bytes.len()));
        }
        Ok((file_name_from_path(path), bytes))
    }
}

/// 百分号编码查询参数。
///
/// 只编码**非** unreserved 字符，中文名字会被编成 UTF-8 百分号序列。
/// 不用 `form_urlencoded` 是因为它对空格给 `+`，而这里接口要的是 `%20`。
fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() * 3);
    for byte in raw.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(*byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// 编码路径。与查询参数的区别：`/` 要保留，否则 CDN 上的层级就没了。
fn urlencode_path(path: &str) -> String {
    path.split('/')
        .map(urlencode)
        .collect::<Vec<_>>()
        .join("/")
}

#[async_trait]
impl Handler for BaPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        let Some(name) = parse_query(ctx.content()) else {
            return Handled::Next;
        };

        let response = match self.fetch(name).await {
            Ok(response) => response,
            Err(reason) => {
                tracing::warn!(error = %reason, name, "BA 图片查询失败");
                let _ = ctx.reply_text(format!("查询失败：{reason}")).await;
                return Handled::Consumed;
            }
        };

        match response.code {
            // 命中：`file` 发图、`plain` 发文本。
            200 => {
                for image in response.data.iter().filter(|i| i.kind.eq_ignore_ascii_case("plain")) {
                    let _ = ctx.reply_text(&image.content).await;
                }
                let files: Vec<&BaImage> = response
                    .data
                    .iter()
                    .filter(|i| i.kind.eq_ignore_ascii_case("file"))
                    .take(MAX_IMAGES)
                    .collect();
                if files.is_empty() {
                    let _ = ctx.reply_text("爱丽丝什么都没有找到~").await;
                }
                for image in files {
                    match self.download(&image.content).await {
                        Ok((file_name, bytes)) => {
                            if let Err(err) = ctx.reply_image_named(&file_name, &bytes).await {
                                tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "BA 图片发送失败");
                            }
                        }
                        Err(reason) => {
                            tracing::warn!(error = %reason, "BA 图片下载失败");
                            let _ = ctx.reply_text(format!("图片获取失败：{reason}")).await;
                        }
                    }
                }
            }
            // 101 = 模糊搜索，列出候选让人重问。
            101 => {
                let _ = ctx.reply_text(format_candidates(&response.data)).await;
            }
            _ => {
                let message = if response.message.is_empty() {
                    format!("接口返回 code={}", response.code)
                } else {
                    response.message.clone()
                };
                let _ = ctx.reply_text(message).await;
            }
        }
        Handled::Consumed
    }

    fn name(&self) -> &'static str {
        "BA 图片"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_query_form() {
        assert_eq!(parse_query("ba 爱丽丝"), Some("爱丽丝"));
        assert_eq!(parse_query("BA 阿露"), Some("阿露"));
        assert_eq!(parse_query("  ba  日富美  "), Some("日富美"));
        assert_eq!(parse_query("Ba 白子"), Some("白子"));
    }

    #[test]
    fn rejects_lookalikes() {
        // 全量模式下 `ba` 开头的英文句子很常见，不能误触发。
        for text in ["ba", "ba ", "ba日历", "baa 爱丽丝", "b 爱丽丝", "买ba 股", ""] {
            assert_eq!(parse_query(text), None, "不该匹配：{text}");
        }
    }

    #[test]
    fn takes_the_basename_of_a_cdn_path() {
        assert_eq!(file_name_from_path("/student_rank/爱丽丝.png"), "爱丽丝.png");
        assert_eq!(file_name_from_path("a/b/c.jpg"), "c.jpg");
        assert_eq!(file_name_from_path(""), "ba.png");
        assert_eq!(file_name_from_path("/"), "ba.png");
    }

    #[test]
    fn encodes_chinese_and_keeps_path_separators() {
        assert_eq!(urlencode("爱丽丝"), "%E7%88%B1%E4%B8%BD%E4%B8%9D");
        assert_eq!(urlencode("a b&c"), "a%20b%26c");
        // `/` 必须保留，否则 CDN 上的层级就没了。
        assert_eq!(urlencode_path("/student_rank/爱丽丝.png"), "/student_rank/%E7%88%B1%E4%B8%BD%E4%B8%9D.png");
    }

    #[test]
    fn parses_a_hit() {
        let raw = r#"{"code":200,"message":"OK","data":[{"name":"爱丽丝","type":"file","content":"/student_rank/爱丽丝.png"}]}"#;
        let parsed: BaResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.code, 200);
        assert_eq!(parsed.data.len(), 1);
        assert_eq!(parsed.data[0].kind, "file");
        assert_eq!(parsed.data[0].content, "/student_rank/爱丽丝.png");
    }

    #[test]
    fn parses_a_fuzzy_search_and_lists_names() {
        let raw = r#"{"code":101,"message":"Fuzzy Search","data":[{"name":"汉堡","type":"file","content":"/a.png"},{"name":"水汉堡","type":"file","content":"/b.png"}]}"#;
        let parsed: BaResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.code, 101);
        let text = format_candidates(&parsed.data);
        assert!(text.starts_with("是想问什么呢："), "{text}");
        assert!(text.contains("汉堡") && text.contains("水汉堡"), "{text}");
    }

    #[test]
    fn missing_optional_fields_do_not_break_parsing() {
        let parsed: BaResponse = serde_json::from_str(r#"{"code":500}"#).unwrap();
        assert_eq!(parsed.code, 500);
        assert!(parsed.data.is_empty());
    }
}
