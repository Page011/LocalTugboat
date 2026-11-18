//! Injected wall clock. Every rule that reads "now" takes it from a `Clock`, so tests
//! control time exactly (expiry, Bin days, pairing window, activity trim) without sleeping.
//! Time unit everywhere in core: unix epoch milliseconds, i64.
//!
//! Implemented now (tiny, and every builder's tests need it from day one).

use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};

pub trait Clock: Send + Sync + 'static {
    /// Unix epoch milliseconds.
    fn now_ms(&self) -> i64;
}

/// The real clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            // A clock before 1970 is a broken machine; 0 keeps every comparison defined.
            .unwrap_or(0)
    }
}

/// Test clock: starts at a given time, moves only when told. Clones share the time.
#[derive(Debug, Clone)]
pub struct TestClock(Arc<AtomicI64>);

impl TestClock {
    pub fn new(start_ms: i64) -> TestClock {
        TestClock(Arc::new(AtomicI64::new(start_ms)))
    }
    pub fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst)
    }
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

pub const SECOND_MS: i64 = 1_000;
pub const MINUTE_MS: i64 = 60 * SECOND_MS;
pub const HOUR_MS: i64 = 60 * MINUTE_MS;
pub const DAY_MS: i64 = 24 * HOUR_MS;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_moves_only_when_told_and_clones_share_time() {
        let c = TestClock::new(1000);
        let d = c.clone();
        assert_eq!(c.now_ms(), 1000);
        assert_eq!(c.now_ms(), 1000, "does not tick by itself");
        c.advance(500);
        assert_eq!(d.now_ms(), 1500);
        d.set(7);
        assert_eq!(c.now_ms(), 7);
        let boxed: Arc<dyn Clock> = Arc::new(c);
        assert_eq!(boxed.now_ms(), 7);
    }

    #[test]
    fn system_clock_is_after_2025_and_units_agree() {
        assert!(SystemClock.now_ms() > 1_735_689_600_000);
        assert_eq!(DAY_MS, 86_400_000);
        assert_eq!(HOUR_MS * 24, DAY_MS);
    }
}
