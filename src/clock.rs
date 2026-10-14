//! Injectable time source. Every rate-limit (and later cache freshness)
//! decision reads time from here instead of `Instant::now()`, so tests can
//! move time forward deterministically instead of sleeping.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use governor::nanos::Nanos;

#[derive(Debug)]
enum Inner {
    System {
        start: Instant,
    },
    Fake {
        elapsed_nanos: AtomicU64,
        start_unix: u64,
    },
}

/// Cheap to clone; clones share the same time (a fake clock advanced
/// through one clone is advanced for all of them).
#[derive(Clone, Debug)]
pub struct Clock {
    inner: Arc<Inner>,
}

impl Default for Clock {
    fn default() -> Self {
        Self::system()
    }
}

impl Clock {
    /// The real clock: monotonic time for intervals, system time for
    /// wall-clock values.
    pub fn system() -> Self {
        Self {
            inner: Arc::new(Inner::System {
                start: Instant::now(),
            }),
        }
    }

    /// A clock that only moves when [`Clock::advance`] or
    /// [`Clock::sleep`] is called. `start_unix` is its wall-clock reading
    /// at creation.
    pub fn fake(start_unix: u64) -> Self {
        Self {
            inner: Arc::new(Inner::Fake {
                elapsed_nanos: AtomicU64::new(0),
                start_unix,
            }),
        }
    }

    /// Moves a fake clock forward. No effect on the system clock.
    pub fn advance(&self, by: Duration) {
        if let Inner::Fake { elapsed_nanos, .. } = &*self.inner {
            // u64 nanoseconds cover ~584 years, far beyond any test.
            let by = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
            elapsed_nanos.fetch_add(by, Ordering::AcqRel);
        }
    }

    /// Monotonic time since this clock was created.
    pub fn elapsed(&self) -> Duration {
        match &*self.inner {
            Inner::System { start } => start.elapsed(),
            Inner::Fake { elapsed_nanos, .. } => {
                Duration::from_nanos(elapsed_nanos.load(Ordering::Acquire))
            }
        }
    }

    /// Wall-clock unix seconds.
    pub fn unix_secs(&self) -> u64 {
        match &*self.inner {
            Inner::System { .. } => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            Inner::Fake { start_unix, .. } => start_unix + self.elapsed().as_secs(),
        }
    }

    /// Waits for `duration`. A fake clock advances itself instead, so
    /// throttling delays cost tests no real time.
    pub async fn sleep(&self, duration: Duration) {
        match &*self.inner {
            Inner::System { .. } => tokio::time::sleep(duration).await,
            Inner::Fake { .. } => self.advance(duration),
        }
    }
}

impl governor::clock::Clock for Clock {
    type Instant = Nanos;

    fn now(&self) -> Nanos {
        Nanos::from(self.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_clock_only_moves_when_advanced() {
        let clock = Clock::fake(1_000);
        assert_eq!(clock.elapsed(), Duration::ZERO);
        assert_eq!(clock.unix_secs(), 1_000);
        clock.advance(Duration::from_secs(90));
        assert_eq!(clock.elapsed(), Duration::from_secs(90));
        assert_eq!(clock.unix_secs(), 1_090);
    }

    #[test]
    fn clones_share_fake_time() {
        let a = Clock::fake(0);
        let b = a.clone();
        a.advance(Duration::from_millis(1500));
        assert_eq!(b.elapsed(), Duration::from_millis(1500));
    }

    #[tokio::test]
    async fn fake_sleep_advances_instead_of_waiting() {
        let clock = Clock::fake(0);
        clock.sleep(Duration::from_secs(3)).await;
        assert_eq!(clock.elapsed(), Duration::from_secs(3));
    }

    #[test]
    fn system_clock_reports_plausible_unix_time() {
        // 2024-01-01; any real system clock is past this.
        assert!(Clock::system().unix_secs() > 1_704_067_200);
    }
}
