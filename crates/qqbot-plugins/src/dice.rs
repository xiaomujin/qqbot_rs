use std::borrow::Cow;

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};

/// 骰子插件。
///
/// **所有入口（`骰子` / `roll` / `r` / `.r` / `。r`）都接受两种记法**：
///
/// - NdM 记法 `3d6`（数量可省：`d6` = `1d6`）：能表达「掷 N 次、每次 M 面」，
///   结果会列出每一颗骰子；
/// - 区间记法 `100` / `5 10`：在 `[a,b]` 里取一个整数（源项目 cq-bot 的 `.r` 语法）。
///
/// 判定只看参数里有没有 `d`：有就是 NdM，纯整数就是区间。
///
/// 改动前是**两套语法各认一半** —— `骰子` / `roll` / `r` 只认 NdM，`.r` 只认区间。
/// 同一个问题换个写法就得到不同结果，其中最坏的一种是**静默算错**：
/// `r 10 100` 只取第一个参数、把 `100` 丢掉，用户以为在掷 10~100，实际在掷 1d100。
///
/// 实测来源（2026-10-08，同一个群连发四条）：`@机器人 .r 2d7` 回用法、
/// `@机器人 .r 50` 出区间、`r 10 100` 静默出 1d100、`r 2d10` 出 NdM。
/// 四条差异看着像「@ 和不 @ 不一样」，其实只是写法不同 —— 所以统一成一套。
pub struct DicePlugin;

const MAX_COUNT: u32 = 20;
const MAX_FACES: u32 = 1000;

/// 区间记法允许的绝对值上限。
///
/// 不是洁癖：`max - min + 1` 在 i64 上会溢出，
/// 而 `rand` 的区间一旦回绕就是 panic 或错误结果。
const MAX_BOUND: i64 = 1_000_000_000;

/// 用法提示。两种记法各给一个例子：NdM 用规范入口 `骰子`，区间用 `.r` ——
/// 群里实际有人一直用 `.r`（cq-bot 的老语法），提示里得出现他习惯的那个写法。
///
/// 走 Markdown，所以示例用 `**加粗**` 而不是方括号 —— 方括号在 Markdown 里是链接语法。
const USAGE: &str = "请正确输入指令\n**骰子 3d6** 或者 **.r 1 100**";

/// 解析出来的掷骰请求。
///
/// **不对外公开**：能造出它的 `parse_roll` 是私有的，公开它只是无用的 API 面积。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Roll {
    /// NdM：掷 `count` 次 `faces` 面，结果取和。
    Ndm { count: u32, faces: u32 },
    /// 区间：`[min, max]` 内取一个整数。
    Range { min: i64, max: i64 },
}

/// 去掉正文里**任意位置**的 `<@openid>` 提及。
///
/// 全局的 `MessageEvent::trimmed()` 只剥**开头**的提及，而 QQ 把提及插在光标处：
/// 先打字再点 @ 就落在**结尾**（库里真实样本：`我第一次下载给的6块<@28800DF7…>`）。
/// 这种消息的参数会被污染成 `2d10<@…>`，NdM 解析失败后**掉回默认的 1d100** ——
/// 从用户角度看就是「@ 和不 @ 结果不一样」。
///
/// 只在骰子内部清，**不动全局 `trimmed()`**：别的插件（词云语料、资源关键词）
/// 依赖「正文原样」的语义，改成剥任意位置会牵连它们。
///
/// 返回 `Cow`：绝大多数消息里根本没有提及，那时直接借用，连一次分配都省掉。
fn strip_mentions(text: &str) -> Cow<'_, str> {
    if !text.contains("<@") {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        // 没有闭合尖括号就不猜：原样留下（`<@unterminated` 不是提及，是正文）。
        let Some(end) = rest[start + 2..].find('>') else {
            break;
        };
        out.push_str(&rest[..start]);
        // 提及当空白：`r 2d10<@X>` 与 `r 2d10` 等价。
        out.push(' ');
        rest = &rest[start + 2 + end + 1..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// 取出参数部分：`.r` / `。r` 前缀之后，或命令 token 之后。
///
/// 两种句点都认：中文输入法下 `.` 很容易打成 `。`。
fn arg_text(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix(['.', '。']).and_then(|r| r.strip_prefix(['r', 'R'])) {
        return rest;
    }
    match text.split_whitespace().next() {
        // 调用方已 trim，首个 token 必定从 0 开始，按字节长度切片落在字符边界上。
        Some(cmd) => &text[cmd.len()..],
        None => "",
    }
}

/// 解析一条骰子消息。返回 `None` 表示参数看不懂（回用法提示）。
///
/// 规则：**含 `d`/`D` → NdM；否则 1~2 个纯整数 → 区间；无参数 → 1d100**。
fn parse_roll(text: &str) -> Option<Roll> {
    let cleaned = strip_mentions(text);
    let args: Vec<&str> = arg_text(cleaned.trim()).split_whitespace().collect();
    match args.split_first() {
        // 无参数：与改动前 `骰子` 的默认一致。
        None => Some(Roll::Ndm { count: 1, faces: 100 }),
        Some((first, _)) if first.contains(['d', 'D']) => {
            parse_spec(first).map(|(count, faces)| Roll::Ndm { count, faces })
        }
        Some((first, rest)) => {
            let bounds: Vec<&str> = std::iter::once(*first).chain(rest.iter().copied()).collect();
            parse_range(&bounds).map(|(min, max)| Roll::Range { min, max })
        }
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
            // 多给的参数**忽略**（`r 1 2 3` → `[1,2]`）：三个数没有公认语义，
            // 而这是改动前 `.r` 的既有行为，不趁这次顺手改掉。
            let x = parse(a)?;
            let y = parse(b)?;
            Some((x.min(y), x.max(y)))
        }
        [] => None,
    }
}

/// 解析 NdM 规格，越界即钳制，绝不 panic。
///
/// 数量可省略：`d6` 等价 `1d6`（标准记法）。改动前这种写法会**静默**掉回 1d100。
pub fn parse_spec(spec: &str) -> Option<(u32, u32)> {
    let (n, m) = spec.split_once(['d', 'D'])?;
    // 空数量 = 一颗。面数仍然必须有，`d` / `d0` 一律拒绝。
    let count: u32 = match n.trim() {
        "" => 1,
        digits => digits.parse().ok()?,
    };
    let faces: u32 = m.trim().parse().ok()?;
    if count == 0 || faces == 0 {
        return None;
    }
    Some((count.min(MAX_COUNT), faces.min(MAX_FACES)))
}

/// 把结果渲染成 Markdown。
///
/// NdM 与区间走**同一条版式**：`🎲 <规格> = **<结果>**`，多颗骰子的明细另起一行。
/// 改动前 NdM 是 Markdown、区间是纯文本，同一个功能两副面孔。
fn render(roll: Roll) -> String {
    // rand 0.10 起 random_range 由 RngExt 提供
    use rand::RngExt;

    match roll {
        Roll::Ndm { count, faces } => {
            let mut rng = rand::rng();
            let rolls: Vec<u32> = (0..count).map(|_| rng.random_range(1..=faces)).collect();
            let sum: u32 = rolls.iter().sum();
            let detail = if rolls.len() == 1 {
                String::new()
            } else {
                format!("\n{}", rolls.iter().map(u32::to_string).collect::<Vec<_>>().join(" + "))
            };
            format!("🎲 {count}d{faces} = **{sum}**{detail}")
        }
        Roll::Range { min, max } => {
            let value = rand::rng().random_range(min..=max);
            format!("🎲 [{min}-{max}] = **{value}**")
        }
    }
}

#[async_trait]
impl Handler for DicePlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        let text = match parse_roll(ctx.content()) {
            Some(roll) => render(roll),
            None => USAGE.to_string(),
        };

        // 必须用 Markdown（msg_type=2）：纯文本消息里 `**加粗**` 只会原样显示。
        if let Err(err) = ctx.reply.markdown(text).await {
            tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), "骰子回复失败");
        }
        Handled::Consumed
    }

    fn name(&self) -> &'static str {
        "骰子"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 机器人 openid 按群下发，形状固定为 `<@32位十六进制>`。
    const MENTION: &str = "<@9FC256316DBE9D176956F7672CA573F5>";

    #[test]
    fn parses_standard_specs() {
        assert_eq!(parse_spec("3d6"), Some((3, 6)));
        assert_eq!(parse_spec("1D100"), Some((1, 100)));
        assert_eq!(parse_spec("2d20"), Some((2, 20)));
    }

    #[test]
    fn omitted_count_means_one_die() {
        // 标准记法：`d6` 就是 `1d6`。改动前这里会静默掉回 1d100。
        assert_eq!(parse_spec("d6"), Some((1, 6)));
        assert_eq!(parse_spec("D20"), Some((1, 20)));
        assert_eq!(parse_roll(".r d6"), Some(Roll::Ndm { count: 1, faces: 6 }));
        assert_eq!(parse_roll("骰子 D100"), Some(Roll::Ndm { count: 1, faces: 100 }));
        // 面数缺失或为 0 仍然拒绝
        assert_eq!(parse_spec("d"), None);
        assert_eq!(parse_spec("d0"), None);
        assert_eq!(parse_spec("0d6"), None, "数量 0 依然非法");
    }

    #[test]
    fn strip_mentions_borrows_when_there_is_no_mention() {
        // 快路径：没有 `<@` 就不该分配。
        assert!(matches!(strip_mentions("骰子 3d6"), Cow::Borrowed(_)));
        assert!(matches!(strip_mentions("r 2d10<@X>"), Cow::Owned(_)));
    }

    #[test]
    fn clamps_abusive_input() {
        assert_eq!(parse_spec("9999d99999"), Some((MAX_COUNT, MAX_FACES)));
    }

    #[test]
    fn every_entry_point_accepts_both_syntaxes() {
        let ndm = |count: u32, faces: u32| Some(Roll::Ndm { count, faces });
        let range = |min: i64, max: i64| Some(Roll::Range { min, max });

        // NdM：命令写法
        assert_eq!(parse_roll("骰子 3d6"), ndm(3, 6));
        assert_eq!(parse_roll("roll 2d20"), ndm(2, 20));
        assert_eq!(parse_roll("r 2d10"), ndm(2, 10));
        // NdM：`.r` 写法 —— 改动前这里回的是「请正确输入指令」
        assert_eq!(parse_roll(".r 2d7"), ndm(2, 7));
        assert_eq!(parse_roll("。r2d7"), ndm(2, 7));

        // 区间：`.r` 写法
        assert_eq!(parse_roll(".r 50"), range(1, 50));
        assert_eq!(parse_roll("。r 5 10"), range(5, 10));
        // 区间：命令写法 —— 改动前 `r 10 100` 会静默算成 1d100
        assert_eq!(parse_roll("骰子 100"), range(1, 100));
        assert_eq!(parse_roll("r 10 100"), range(10, 100));
        assert_eq!(parse_roll("roll 10 5"), range(5, 10), "两数自动排序");

        // 无参数：与改动前 `骰子` 的默认一致
        assert_eq!(parse_roll("骰子"), ndm(1, 100));
        assert_eq!(parse_roll("r"), ndm(1, 100));
        assert_eq!(parse_roll(".r"), ndm(1, 100));
    }

    #[test]
    fn mentions_anywhere_do_not_pollute_arguments() {
        // 先打字再点 @：提及落在结尾，参数会被粘上 `<@…>`。
        assert_eq!(parse_roll(&format!("r 2d10{MENTION}")), Some(Roll::Ndm { count: 2, faces: 10 }));
        assert_eq!(parse_roll(&format!(".r 50{MENTION}")), Some(Roll::Range { min: 1, max: 50 }));
        // 开头，以及和命令贴在一起（中间没有空格）
        assert_eq!(
            parse_roll(&format!("{MENTION} .r 5 10")),
            Some(Roll::Range { min: 5, max: 10 })
        );
        assert_eq!(parse_roll(&format!("{MENTION}骰子 3d6")), Some(Roll::Ndm { count: 3, faces: 6 }));
        // 正文中间的提及同理
        assert_eq!(parse_roll(&format!("骰子{MENTION} 3d6")), Some(Roll::Ndm { count: 3, faces: 6 }));
    }

    #[test]
    fn strip_mentions_keeps_unterminated_text() {
        // 没有闭合尖括号时不能吃掉正文，也不能死循环。
        assert_eq!(strip_mentions("喊 <@unterminated 来"), "喊 <@unterminated 来");
        assert_eq!(strip_mentions("a<@X>b<@Y>c"), "a b c");
    }

    #[test]
    fn malformed_arguments_ask_for_usage() {
        for text in ["骰子 abc", ".r abc", "r x 5", "骰子 3d", "roll 0d6", "。r 1.5"] {
            assert_eq!(parse_roll(text), None, "不该当成有效参数：{text}");
        }
    }

    #[test]
    fn both_syntaxes_render_in_one_layout() {
        // 结果随机，所以只盯版式骨架。
        let single = render(Roll::Ndm { count: 1, faces: 6 });
        assert!(single.starts_with("🎲 1d6 = **"), "{single}");
        assert!(single.ends_with("**"), "{single}");
        assert!(!single.contains('\n'), "单颗骰子不该有明细行: {single}");

        let many = render(Roll::Ndm { count: 3, faces: 6 });
        assert_eq!(many.lines().count(), 2, "多颗骰子第二行列明细: {many}");
        assert!(many.contains(" + "), "{many}");

        let range = render(Roll::Range { min: 5, max: 10 });
        assert!(range.starts_with("🎲 [5-10] = **"), "{range}");
        assert!(range.ends_with("**"), "{range}");
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
