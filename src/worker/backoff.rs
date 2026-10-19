//! How long the worker leaves a failing candidate source alone. Pure: the
//! caller says what time it is, so tests need no sleeping.

use std::time::{Duration, Instant};

const FIRST: Duration = Duration::from_secs(30);
const CAP: Duration = Duration::from_secs(15 * 60);

/// Doubles the wait after every failure, up to a cap, and starts over after
/// a success.
#[derive(Debug, Clone)]
pub struct Backoff {
    first: Duration,
    cap: Duration,
    failures: u32,
    until: Option<Instant>,
}

impl Backoff {
    pub fn new(first: Duration, cap: Duration) -> Self {
        Self {
            first,
            cap,
            failures: 0,
            until: None,
        }
    }

    /// 30 seconds, doubling, at most 15 minutes: for the candidate source.
    pub fn for_source() -> Self {
        Self::new(FIRST, CAP)
    }

    /// Records a failure at `now`; returns how long to stay away.
    pub fn failure(&mut self, now: Instant) -> Duration {
        let factor = 2u32.saturating_pow(self.failures.min(24));
        let wait = self.first.saturating_mul(factor).min(self.cap);
        self.failures = self.failures.saturating_add(1);
        self.until = Some(now + wait);
        wait
    }

    pub fn success(&mut self) {
        self.failures = 0;
        self.until = None;
    }

    /// How much longer to stay away, or `None` when the wait is over.
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.until?
            .checked_duration_since(now)
            .filter(|left| !left.is_zero())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn it_doubles_from_thirty_seconds() {
        let now = Instant::now();
        let mut backoff = Backoff::for_source();
        let waits: Vec<Duration> = (0..4).map(|_| backoff.failure(now)).collect();
        assert_eq!(waits, [secs(30), secs(60), secs(120), secs(240)]);
    }

    #[test]
    fn it_stops_growing_at_fifteen_minutes() {
        let now = Instant::now();
        let mut backoff = Backoff::for_source();
        let waits: Vec<Duration> = (0..8).map(|_| backoff.failure(now)).collect();
        assert_eq!(waits[5], secs(900));
        assert!(waits[5..].iter().all(|w| *w == secs(900)));
        // Many failures later it still holds, with no overflow.
        for _ in 0..200 {
            assert_eq!(backoff.failure(now), secs(900));
        }
    }

    #[test]
    fn a_success_starts_it_over() {
        let now = Instant::now();
        let mut backoff = Backoff::for_source();
        backoff.failure(now);
        backoff.failure(now);
        backoff.success();
        assert_eq!(backoff.remaining(now), None);
        assert_eq!(backoff.failure(now), secs(30));
    }

    #[test]
    fn it_counts_down_to_the_end_of_the_wait() {
        let start = Instant::now();
        let mut backoff = Backoff::for_source();
        assert_eq!(backoff.remaining(start), None);
        backoff.failure(start);
        assert_eq!(backoff.remaining(start), Some(secs(30)));
        assert_eq!(backoff.remaining(start + secs(10)), Some(secs(20)));
        assert_eq!(backoff.remaining(start + secs(30)), None);
        assert_eq!(backoff.remaining(start + secs(31)), None);
    }
}
