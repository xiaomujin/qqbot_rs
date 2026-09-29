//! 时间窗计算：固定偏移下的日历数学。
//!
//! **刻意不引入 `chrono` / `time`。** 本机器人只需要两个固定偏移时区
//! （`Asia/Shanghai` = UTC+8、`Europe/Moscow` = UTC+3，两者都早已取消夏令时），
//! 以及「今天 / 本周 / 本月 / 本年」的起点。
//!
//! 这用 Howard Hinnant 的两个公式（`civil_from_days` / `days_from_civil`）
//! 就能算清，约 40 行、可完整单测，不值得为此拖进一个时区数据库。
//!
//! 公式出处：<https://howardhinnant.github.io/date_algorithms.html>（公有领域）。

/// 北京时间偏移（秒）。中国自 1991 年起不再使用夏令时。
pub const SHANGHAI_OFFSET: i64 = 8 * 3600;
/// 莫斯科时间偏移（秒）。俄罗斯自 2014 年起不再使用夏令时。
pub const MOSCOW_OFFSET: i64 = 3 * 3600;

const DAY: i64 = 86_400;

/// 词云统计窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Today,
    ThisWeek,
    ThisMonth,
    ThisYear,
}

impl Window {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "今日" => Some(Self::Today),
            "本周" => Some(Self::ThisWeek),
            "本月" => Some(Self::ThisMonth),
            "本年" => Some(Self::ThisYear),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Today => "今日",
            Self::ThisWeek => "本周",
            Self::ThisMonth => "本月",
            Self::ThisYear => "本年",
        }
    }

    /// 该窗口在 `offset` 时区下的起点（Unix 秒）。
    ///
    /// 起点按**当地日历**算：`今日` 是当地 0 点，`本周` 是当地周一 0 点。
    /// 直接按 UTC 切会让国内用户在早上 8 点前看到「昨天」的数据。
    pub fn since(self, now: i64, offset: i64) -> i64 {
        let local_day = (now + offset).div_euclid(DAY);
        let start_day = match self {
            Self::Today => local_day,
            Self::ThisWeek => local_day - weekday_monday_offset(local_day),
            Self::ThisMonth => {
                let (y, m, _) = civil_from_days(local_day);
                days_from_civil(y, m, 1)
            }
            Self::ThisYear => {
                let (y, _, _) = civil_from_days(local_day);
                days_from_civil(y, 1, 1)
            }
        };
        start_day * DAY - offset
    }
}

/// 从今天回溯到本周一需要几天。
///
/// 1970-01-01 是**周四**，所以 `(day + 4).rem_euclid(7)` 得到 0=周日 … 6=周六；
/// 再折成「距周一几天」：周一 → 0，周日 → 6。
fn weekday_monday_offset(day: i64) -> i64 {
    let weekday = (day + 4).rem_euclid(7);
    (weekday + 6).rem_euclid(7)
}

/// 格式化为 `YYYY-MM-DD HH:MM:SS`（按 `offset` 指定的固定时区）。
pub fn format_datetime(now: i64, offset: i64) -> String {
    let local = now + offset;
    let (y, m, d) = civil_from_days(local.div_euclid(DAY));
    let secs = local.rem_euclid(DAY);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Unix 天数 → (年, 月, 日)。Hinnant 的 `civil_from_days`。
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (年, 月, 日) → Unix 天数。Hinnant 的 `days_from_civil`。
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_round_trips() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn known_dates_match() {
        // 2000-01-01 是第 10957 天；这天也是闰年规则的经典样本。
        assert_eq!(days_from_civil(2000, 1, 1), 10_957);
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
        assert_eq!(days_from_civil(2024, 2, 29), 19_782, "2024 是闰年");
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(days_from_civil(2026, 9, 29), 20_725);
    }

    #[test]
    fn round_trips_over_five_decades() {
        // 逐日往返，覆盖闰年、世纪年与月长差异。
        for days in -3_000..21_000 {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
    }

    #[test]
    fn today_starts_at_local_midnight() {
        // 2026-09-29 12:00 UTC = 当地 20:00，当日起点应当是当地 00:00。
        let now = 20_725 * DAY + 12 * 3600;
        let since = Window::Today.since(now, SHANGHAI_OFFSET);
        assert_eq!(since, 20_725 * DAY - SHANGHAI_OFFSET);
        assert_eq!(since % DAY, DAY - SHANGHAI_OFFSET, "当地 0 点即 UTC 前一天 16 点");
    }

    #[test]
    fn today_before_local_dawn_still_uses_local_day() {
        // UTC 00:30 时当地已经是 08:30，同一天；若按 UTC 切会切到前一天。
        let now = 20_725 * DAY + 30 * 60;
        assert_eq!(Window::Today.since(now, SHANGHAI_OFFSET), 20_725 * DAY - SHANGHAI_OFFSET);
    }

    #[test]
    fn week_starts_on_monday() {
        // 2026-09-29 是周二，本周一应当是 09-28。
        let (y, m, d) = civil_from_days(20_725);
        assert_eq!((y, m, d), (2026, 9, 29));
        let now = 20_725 * DAY + 6 * 3600;
        let since = Window::ThisWeek.since(now, SHANGHAI_OFFSET);
        assert_eq!(since, days_from_civil(2026, 9, 28) * DAY - SHANGHAI_OFFSET);
    }

    #[test]
    fn sunday_belongs_to_the_week_that_started_monday() {
        // 2026-10-04 是周日，本周一应当是 09-28 —— 而不是次日。
        let sunday = days_from_civil(2026, 10, 4);
        let since = Window::ThisWeek.since(sunday * DAY + 3600, SHANGHAI_OFFSET);
        assert_eq!(since, days_from_civil(2026, 9, 28) * DAY - SHANGHAI_OFFSET);
    }

    #[test]
    fn month_and_year_start_at_first_day() {
        let now = 20_725 * DAY + 6 * 3600;
        assert_eq!(
            Window::ThisMonth.since(now, SHANGHAI_OFFSET),
            days_from_civil(2026, 9, 1) * DAY - SHANGHAI_OFFSET
        );
        assert_eq!(
            Window::ThisYear.since(now, SHANGHAI_OFFSET),
            days_from_civil(2026, 1, 1) * DAY - SHANGHAI_OFFSET
        );
    }

    #[test]
    fn window_labels_round_trip() {
        for w in [Window::Today, Window::ThisWeek, Window::ThisMonth, Window::ThisYear] {
            assert_eq!(Window::parse(w.label()), Some(w));
        }
        assert_eq!(Window::parse("昨天"), None);
    }

    #[test]
    fn datetime_is_zero_padded_and_uses_the_offset() {
        // 1970-01-01 00:00:00 UTC → 北京 08:00:00
        assert_eq!(format_datetime(0, SHANGHAI_OFFSET), "1970-01-01 08:00:00");
        assert_eq!(format_datetime(0, MOSCOW_OFFSET), "1970-01-01 03:00:00");
        // 跨日：UTC 16:00 已经是北京的次日 0 点。
        let utc_16 = 20_725 * DAY + 16 * 3600;
        assert_eq!(format_datetime(utc_16, SHANGHAI_OFFSET), "2026-09-30 00:00:00");
        assert_eq!(format_datetime(utc_16, MOSCOW_OFFSET), "2026-09-29 19:00:00");
    }

    #[test]
    fn moscow_offset_is_three_hours() {
        // 源项目的「塔科夫时间」用莫斯科时区，固定 +3。
        assert_eq!(MOSCOW_OFFSET, 3 * 3600);
    }
}
