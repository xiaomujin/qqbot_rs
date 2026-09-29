use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};

/// 骰子插件。
///
/// **两套语法并存**，因为它们回答的是不同的问题：
///
/// - `骰子` / `骰子 3d6` / `roll 2d20` —— NdM 记法，能表达「掷 N 次、每次 M 面」，
///   结果会列出每一颗骰子；
/// - `.r 100` / `.r 5 10` —— 区间记法（源项目 cq-bot 的语法），
///   「在 [a,b] 里取一个整数」，只有一个结果。
///
/// 把 `.r 100` 硬映射成 `1d100` 看着像，其实不等价：
/// `.r 5 10` 根本没有对应的 NdM 写法。
pub struct DicePlugin;

const MAX_COUNT: u32 = 20;
const MAX_FACES: u32 = 1000;

/// 区间记法允许的绝对值上限。
///
/// 不是洁癖：`max - min + 1` 在 i64 上会溢出，
/// 而 `rand` 的区间一旦回绕就是 panic 或错误结果。
const MAX_BOUND: i64 = 1_000_000_000;

/// 识别 `.r` / `。r` 前缀，返回其后的参数部分。
///
/// 两种句点都认：中文输入法下 `.` 很容易打成 `。`。
/// 但 `r` 后面必须是空白、数字、负号或结尾 —— 否则 `。roll` 这类
/// 正常中文句子也会被吞掉（`。` 本来就是句末标点）。
fn range_command(content: &str) -> Option<&str> {
    let rest = content.trim().strip_prefix(['.', '。'])?;
    let after = rest.strip_prefix(['r', 'R'])?;
    match after.chars().next() {
        None => Some(""),
        Some(c) if c.is_whitespace() || c.is_ascii_digit() || c == '-' => Some(after.trim()),
        _ => None,
    }
}

/// 解析区间记法的参数。
///
/// `[N]` 等价 `[1, N]`；`[A, B]` 取 `[A, B]` 闭区间，**两数自动排序** ——
/// 源项目也这么做，用户不必记住谁大谁小。
pub fn parse_range(args: &[&str]) -> Option<(i64, i64)> {
    let parse = |raw: &str| -> Option<i64> {
        let value: i64 = raw.parse().ok()?;
        (value.abs() <= MAX_BOUND).then_some(value)
    };
    match args {
        [single] => {
            let n = parse(single)?;
            Some((n.min(1), n.max(1)))
        }
        [a, b, ..] => {
            let x = parse(a)?;
            let y = parse(b)?;
            Some((x.min(y), x.max(y)))
        }
        [] => None,
    }
}

/// 区间记法的用法提示，与源项目文案一致。
const RANGE_USAGE: &str = "请正确输入指令\n[.r 数字] 或者 [.r 数字 数字]";

#[async_trait]
impl Handler for DicePlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        // 区间记法：`.r 100` / `.r 5 10`
        if let Some(rest) = range_command(ctx.content()) {
            let args: Vec<&str> = rest.split_whitespace().collect();
            let text = match parse_range(&args) {
                Some((min, max)) => {
                    use rand::RngExt;
                    let value = rand::rng().random_range(min..=max);
                    format!("范围：[{min}-{max}]\n结果：{value}")
                }
                None => RANGE_USAGE.to_string(),
            };
            if let Err(err) = ctx.reply_text(text).await {
                tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "骰子回复失败");
            }
            return Handled::Consumed;
        }

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

    fn name(&self) -> &'static str {
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
    fn recognizes_the_range_command() {
        assert_eq!(range_command(".r 100"), Some("100"));
        assert_eq!(range_command("。r 5 10"), Some("5 10"));
        assert_eq!(range_command(".R 5"), Some("5"));
        assert_eq!(range_command(".r5"), Some("5"));
        assert_eq!(range_command(".r"), Some(""));
        assert_eq!(range_command("  .r  -5 5 "), Some("-5 5"));
    }

    #[test]
    fn range_command_does_not_swallow_ordinary_text() {
        // `。` 本来就是句末标点，`。roll` / `。really` 都是正常句子。
        for text in ["。roll", "。really", ".x 1", "r 100", "。", ".", ""] {
            assert_eq!(range_command(text), None, "不该匹配：{text}");
        }
    }

    #[test]
    fn single_number_means_one_to_n() {
        assert_eq!(parse_range(&["100"]), Some((1, 100)));
        assert_eq!(parse_range(&["0"]), Some((0, 1)), "0 也应当能算出区间");
        assert_eq!(parse_range(&["-5"]), Some((-5, 1)));
    }

    #[test]
    fn two_numbers_are_sorted() {
        assert_eq!(parse_range(&["5", "10"]), Some((5, 10)));
        assert_eq!(parse_range(&["10", "5"]), Some((5, 10)), "用户不必记住谁大谁小");
        assert_eq!(parse_range(&["-5", "5"]), Some((-5, 5)));
    }

    #[test]
    fn range_rejects_non_numbers_and_absurd_bounds() {
        assert_eq!(parse_range(&[]), None);
        assert_eq!(parse_range(&["abc"]), None);
        assert_eq!(parse_range(&["1", "abc"]), None);
        // 超过上限会让 `max - min + 1` 在 i64 上回绕。
        assert_eq!(parse_range(&["99999999999"]), None);
        assert_eq!(parse_range(&["-99999999999"]), None);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_spec("abc"), None);
        assert_eq!(parse_spec("0d6"), None);
        assert_eq!(parse_spec("3d0"), None);
        assert_eq!(parse_spec(""), None);
    }
}
