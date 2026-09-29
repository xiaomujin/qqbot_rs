use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};

/// 骰子插件。
///
/// 用法：骰子（默认 1d100）、骰子 3d6、roll 2d20。
pub struct DicePlugin;

const MAX_COUNT: u32 = 20;
const MAX_FACES: u32 = 1000;

#[async_trait]
impl Handler for DicePlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        let spec = ctx.arg(0).unwrap_or("1d100");
        let (count, faces) = parse_spec(spec).unwrap_or((1, 100));

        let rolls: Vec<u32> = {
            // rand 0.10 起 random_range 由 RngExt 提供
            use rand::RngExt;
            let mut rng = rand::rng();
            (0..count).map(|_| rng.random_range(1..=faces)).collect()
        };
        let sum: u32 = rolls.iter().sum();

        let detail = if rolls.len() == 1 {
            String::new()
        } else {
            format!("\n{}", rolls.iter().map(u32::to_string).collect::<Vec<_>>().join(" + "))
        };
        let text = format!("🎲 {}d{} = **{}**{}", count, faces, sum, detail);

        // 必须用 Markdown（msg_type=2）：纯文本消息里 `**加粗**` 只会原样显示。
        if let Err(err) = ctx.reply_markdown(text).await {
            tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "骰子回复失败");
        }
        Handled::Consumed
    }

    fn name(&self) -> &str {
        "骰子"
    }
}

/// 解析 NdM 规格，越界即钳制，绝不 panic。
pub fn parse_spec(spec: &str) -> Option<(u32, u32)> {
    let (n, m) = spec.split_once(['d', 'D'])?;
    let count: u32 = n.trim().parse().ok()?;
    let faces: u32 = m.trim().parse().ok()?;
    if count == 0 || faces == 0 {
        return None;
    }
    Some((count.min(MAX_COUNT), faces.min(MAX_FACES)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_specs() {
        assert_eq!(parse_spec("3d6"), Some((3, 6)));
        assert_eq!(parse_spec("1D100"), Some((1, 100)));
        assert_eq!(parse_spec("2d20"), Some((2, 20)));
    }

    #[test]
    fn clamps_abusive_input() {
        assert_eq!(parse_spec("9999d99999"), Some((MAX_COUNT, MAX_FACES)));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_spec("abc"), None);
        assert_eq!(parse_spec("0d6"), None);
        assert_eq!(parse_spec("3d0"), None);
        assert_eq!(parse_spec(""), None);
    }
}
