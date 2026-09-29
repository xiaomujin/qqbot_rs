//! 图语（F3）。
//!
//! `图语 <文本>` 之后，同一个人在同一个会话里发的**下一张图**会被配上这段文字重发一次。
//!
//! 源项目用的是 OneBot 的图片 `summary` 字段（客户端把文字显示在图片下方）。
//! 官方 v2 的 `media` 里**没有**这个字段，所以改用 `media_with_text`：
//! 图和文字在**同一条**消息里，效果接近，而且是官方支持的能力。

use std::sync::Arc;

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use qqbot_media::FileType;
use qqbot_store::ResourceStore;

/// 待用图语的有效期。源项目也是 5 分钟。
const CAPTION_TTL_SECS: i64 = 300;
/// 单张图片大小上限。
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// 解析 `图语 <文本>`。返回 `None` 表示不是这个命令。
pub fn parse_caption(content: &str) -> Option<&str> {
    let trimmed = content.trim();
    let rest = trimmed.strip_prefix("图语")?;
    // 必须有空白分隔，否则 `图语言` 也会被吞掉。
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let text = rest.trim();
    if text.is_empty() { None } else { Some(text) }
}

/// `Clone` 是必需的：同一个实例要挂到命令路由与监听器上。
#[derive(Clone)]
pub struct CaptionPlugin {
    store: Option<Arc<ResourceStore>>,
    http: reqwest::Client,
}

impl CaptionPlugin {
    pub fn new(store: Option<Arc<ResourceStore>>, http: reqwest::Client) -> Self {
        Self { store, http }
    }

    /// `图语 <文本>`：记下来，等下一张图。
    async fn remember(&self, ctx: &Ctx) -> Handled {
        let Some(text) = parse_caption(ctx.content()) else {
            let _ = ctx.reply_text("用法：图语 <文字>，然后发一张图").await;
            return Handled::Consumed;
        };
        let Some(store) = &self.store else {
            let _ = ctx.reply_text("未启用持久化，图语不可用").await;
            return Handled::Consumed;
        };
        let Some(sender) = ctx.sender_openid() else {
            return Handled::Consumed;
        };
        match store.set_caption(ctx.target.id(), sender, text).await {
            Ok(()) => {
                let _ = ctx.reply_text("好，发图吧").await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "记录图语失败");
                let _ = ctx.reply_text("记录失败，稍后再试").await;
            }
        }
        Handled::Consumed
    }

    /// 图片消息：有待用图语就配上重发。
    async fn apply(&self, ctx: &Ctx) -> Handled {
        let Some(store) = &self.store else {
            return Handled::Next;
        };
        let Some(sender) = ctx.sender_openid() else {
            return Handled::Next;
        };
        let caption = match store.take_caption(ctx.target.id(), sender, CAPTION_TTL_SECS).await {
            Ok(Some(text)) => text,
            // 绝大多数图片消息都走这条：没有待用图语，什么也不做。
            Ok(None) => return Handled::Next,
            Err(err) => {
                tracing::warn!(error = %err, "读取图语失败");
                return Handled::Next;
            }
        };

        let Some(attachment) = ctx.message.first_attachment() else {
            return Handled::Next;
        };
        let Some(url) = attachment.url.as_deref().filter(|u| !u.is_empty()) else {
            return Handled::Next;
        };
        let name = attachment
            .filename
            .clone()
            .unwrap_or_else(|| "image.png".to_string());

        match self.download(url).await {
            Ok(bytes) => {
                if let Err(err) = ctx
                    .reply_media_with_text(FileType::Image, &name, &bytes, &caption)
                    .await
                {
                    tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "图语发送失败");
                }
            }
            Err(reason) => {
                tracing::warn!(error = %reason, "图语原图下载失败");
                let _ = ctx.reply_text(format!("图片获取失败：{reason}")).await;
            }
        }
        Handled::Consumed
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        let res = self
            .http
            .get(url)
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
        Ok(bytes)
    }
}

#[async_trait]
impl Handler for CaptionPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        if ctx.content().split_whitespace().next() == Some("图语") {
            return self.remember(ctx).await;
        }
        // 只有带图片的消息才值得查一次待用状态 ——
        // 否则每条群消息都要读一次库，而图语本来就是低频功能。
        if ctx.message.first_attachment().is_none() {
            return Handled::Next;
        }
        self.apply(ctx).await
    }

    fn name(&self) -> &'static str {
        "图语"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_caption_form() {
        assert_eq!(parse_caption("图语 你好"), Some("你好"));
        assert_eq!(parse_caption("  图语   多空格  "), Some("多空格"));
        assert_eq!(parse_caption("图语 带 空格 的 文字"), Some("带 空格 的 文字"));
    }

    #[test]
    fn rejects_lookalikes() {
        // `图语言` / `图语` 不是命令；`图语` 单独出现时给用法提示，由调用方处理。
        for text in ["图语言", "图语", "图语 ", "说图语 你好", ""] {
            assert_eq!(parse_caption(text), None, "不该匹配：{text}");
        }
    }
}
