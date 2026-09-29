use std::time::Duration;

/// 指数退避 + 抖动。
///
/// 抖动不依赖 `rand`：用系统时间的纳秒位做熵源，足以打散多个分片的重连时刻。
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    current: Duration,
    attempt: u32,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self { base, max, current: base, attempt: 0 }
    }

    /// 连接成功后重置。
    pub fn reset(&mut self) {
        self.attempt = 0;
        self.current = self.base;
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// 取下一次延迟（并推进状态）。
    pub fn next_delay(&mut self) -> Duration {
        self.attempt = self.attempt.saturating_add(1);
        let d = self.current.min(self.max);
        self.current = self.current.saturating_mul(2).min(self.max);
        jitter(d)
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(Duration::from_secs(1), Duration::from_secs(60))
    }
}

fn jitter(d: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|x| x.subsec_nanos())
        .unwrap_or(0);
    // 0.5 ~ 1.0 倍
    let factor = 0.5 + (nanos % 1000) as f64 / 2000.0;
    d.mul_f64(factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_then_caps() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(8));
        for _ in 0..12 {
            let d = b.next_delay();
            assert!(d <= Duration::from_secs(8), "超过上限: {d:?}");
            assert!(d >= Duration::from_millis(400), "抖动下限异常: {d:?}");
        }
        assert!(b.attempt() == 12);
    }

    #[test]
    fn reset_clears_state() {
        let mut b = Backoff::default();
        b.next_delay();
        b.next_delay();
        b.reset();
        assert_eq!(b.attempt(), 0);
    }
}
