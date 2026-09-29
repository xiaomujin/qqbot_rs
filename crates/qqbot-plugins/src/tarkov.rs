//! 塔科夫相关命令。
//!
//! 从 cq-bot 的 `BulletPlugin` 迁移而来。该插件在源项目里同时承担
//! 时间换算、BOSS 刷新率、跳蚤市场、查任务、查子弹 —— 集中放这里，
//! 免得每加一个命令就多一个只含几十行的文件。

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use qqbot_store::now_unix;

use crate::timewin::MOSCOW_OFFSET;

const DAY: i64 = 86_400;

/// 游戏内时间流速相对现实的倍数。
const GAME_SPEED: i64 = 7;
/// 两个可选出发时间相差的小时数。
const CLOCK_SPLIT_SECS: i64 = 12 * 3600;

/// 塔科夫游戏内时刻（一天中的秒数），返回 `(左, 右)`。
///
/// 游戏内时间流速是现实的 **7 倍**（`now × 7`），再按**莫斯科时区**取时刻；
/// 两个值相差 12 小时，对应游戏里可选的两个出发时间。
///
/// 源实现乘的是 `epochMilli`，这里乘秒 —— 两者对「一天中的时刻」完全等价，
/// 而且少了 1000 倍，不至于把中间结果推到 i64 边界附近。
pub fn tarkov_clock(now_secs: i64) -> (i64, i64) {
    let game = now_secs.saturating_mul(GAME_SPEED) + MOSCOW_OFFSET;
    ((game - CLOCK_SPLIT_SECS).rem_euclid(DAY), game.rem_euclid(DAY))
}

/// 格式化后的 `(左, 右)`，形如 `03:00:00`。
pub fn tarkov_time(now_secs: i64) -> (String, String) {
    let (left, right) = tarkov_clock(now_secs);
    (format_clock(left), format_clock(right))
}

fn format_clock(secs: i64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

pub struct TarkovPlugin;

impl Default for TarkovPlugin {
    fn default() -> Self {
        Self
    }
}

impl TarkovPlugin {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Handler for TarkovPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        if ctx.content() != "塔科夫时间" {
            return Handled::Next;
        }
        // 与源项目一致：两行裸时刻，不带标签。
        let (left, right) = tarkov_time(now_unix());
        let _ = ctx.reply_text(format!("{left}\n{right}")).await;
        Handled::Consumed
    }

    fn name(&self) -> &'static str {
        "塔科夫时间"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_gives_a_known_clock() {
        // 现实 0 点 → 游戏内 0 点，加莫斯科 +3 得 03:00:00；
        // 另一个值早 12 小时，取模后是 15:00:00。
        assert_eq!(tarkov_time(0), ("15:00:00".to_string(), "03:00:00".to_string()));
    }

    #[test]
    fn the_two_clocks_are_twelve_hours_apart() {
        for now in [0, 1_700_000_000, 1_759_000_000, 2_000_000_000] {
            let (left, right) = tarkov_clock(now);
            assert_eq!(
                (left - right).rem_euclid(DAY),
                CLOCK_SPLIT_SECS,
                "两个出发时间必须相差 12 小时（now={now}）"
            );
        }
    }

    #[test]
    fn clock_advances_seven_times_faster() {
        // 现实过 1 小时，游戏内应当过 7 小时。
        let (_, before) = tarkov_clock(1_700_000_000);
        let (_, after) = tarkov_clock(1_700_000_000 + 3600);
        assert_eq!((after - before).rem_euclid(DAY), 7 * 3600);
    }

    #[test]
    fn clock_is_zero_padded() {
        assert_eq!(format_clock(0), "00:00:00");
        assert_eq!(format_clock(59), "00:00:59");
        assert_eq!(format_clock(3600 + 120 + 3), "01:02:03");
        assert_eq!(format_clock(DAY - 1), "23:59:59");
    }
}
