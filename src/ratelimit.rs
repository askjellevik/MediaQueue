//! Rate limiting:
//!
//! - [`KeyedLimiter`]: governor's keyed GCRA limiter for request rates
//!   (every `/api` call per IP, searches per member), with a hard key cap.
//! - [`FailureTracker`]: per-IP lockouts with backoff for wrong access
//!   codes and wrong admin logins.
//! - [`GlobalThrottle`]: slows every login attempt down once failures
//!   across all IPs pile up, instead of blocking anyone outright.
//!
//! All state is in memory and bounded. Every internal failure (a poisoned
//! lock, a full key table, a closed semaphore) is treated as "limited":
//! nothing here can fail open.

use std::{
    collections::HashMap,
    hash::Hash,
    net::IpAddr,
    num::NonZeroU32,
    sync::{Arc, Mutex},
    time::Duration,
};

use governor::{
    Quota, RateLimiter,
    clock::Clock as _,
    middleware::NoOpMiddleware,
    nanos::Nanos,
    state::{
        StateStore,
        keyed::{DashMapStateStore, ShrinkableKeyedStateStore},
    },
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{
    clientip::bucket_key,
    clock::Clock,
    config::{RateLimitConfig, ThrottleConfig},
    notify::{Message, Notifier},
};

/// Retry-After used when a limiter fails closed for a reason other than a
/// known refill time (key table full, poisoned lock). One maintenance
/// interval: that is when space is next freed.
const FAIL_CLOSED_RETRY: Duration = Duration::from_secs(60);

/// A limit was hit; the client may retry after this many whole seconds
/// (always at least 1, as the `Retry-After` header needs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryAfter(pub u64);

impl From<Duration> for RetryAfter {
    fn from(wait: Duration) -> Self {
        let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
        RetryAfter(secs.max(1))
    }
}

fn non_zero(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap_or(NonZeroU32::MIN)
}

// ---------------------------------------------------------------------
// Keyed request-rate limiter
// ---------------------------------------------------------------------

/// governor's DashMap store behind an `Arc`, so [`KeyedLimiter`] can see
/// which keys exist (for the cap) while governor owns the rate state.
struct SharedStore<K: Hash + Eq + Clone>(Arc<DashMapStateStore<K>>);

impl<K: Hash + Eq + Clone> StateStore for SharedStore<K> {
    type Key = K;

    fn measure_and_replace<T, F, E>(&self, key: &K, f: F) -> Result<T, E>
    where
        F: Fn(Option<Nanos>) -> Result<(T, Nanos), E>,
    {
        <DashMapStateStore<K> as StateStore>::measure_and_replace(&self.0, key, f)
    }
}

impl<K: Hash + Eq + Clone> ShrinkableKeyedStateStore<K> for SharedStore<K> {
    fn retain_recent(&self, drop_below: Nanos) {
        ShrinkableKeyedStateStore::retain_recent(&*self.0, drop_below);
    }

    fn shrink_to_fit(&self) {
        ShrinkableKeyedStateStore::shrink_to_fit(&*self.0);
    }

    fn len(&self) -> usize {
        ShrinkableKeyedStateStore::len(&*self.0)
    }

    fn is_empty(&self) -> bool {
        ShrinkableKeyedStateStore::is_empty(&*self.0)
    }
}

/// A governor keyed limiter that never tracks more than `cap` keys. A new
/// key arriving while the table is full is limited rather than stored, so
/// a flood of distinct IPs cannot grow memory without bound.
pub struct KeyedLimiter<K: Hash + Eq + Clone> {
    limiter: RateLimiter<K, SharedStore<K>, Clock, NoOpMiddleware<Nanos>>,
    keys: Arc<DashMapStateStore<K>>,
    cap: usize,
    clock: Clock,
}

impl<K: Hash + Eq + Clone> KeyedLimiter<K> {
    pub fn new(quota: Quota, cap: usize, clock: Clock) -> Self {
        let keys: Arc<DashMapStateStore<K>> = Arc::default();
        let limiter = RateLimiter::new(quota, SharedStore(keys.clone()), clock.clone());
        Self {
            limiter,
            keys,
            cap,
            clock,
        }
    }

    pub fn check(&self, key: &K) -> Result<(), RetryAfter> {
        // Two new keys racing past this check can overshoot the cap by at
        // most the number of in-flight requests (64), which stays bounded.
        if !self.keys.contains_key(key) && self.keys.len() >= self.cap {
            return Err(FAIL_CLOSED_RETRY.into());
        }
        self.limiter
            .check_key(key)
            .map_err(|not_until| not_until.wait_time_from(self.clock.now()).into())
    }

    /// Drops keys whose bucket is full again (indistinguishable from a new
    /// key) and releases the freed memory.
    pub fn maintain(&self) {
        self.limiter.retain_recent();
        self.limiter.shrink_to_fit();
    }

    pub fn len(&self) -> usize {
        self.limiter.len()
    }

    pub fn is_empty(&self) -> bool {
        self.limiter.is_empty()
    }
}

// ---------------------------------------------------------------------
// Per-IP failure tracker
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct FailureEntry {
    count: u32,
    window_start: Duration,
    locked_until: Option<Duration>,
    /// Index into the lockout list for the next lockout.
    level: usize,
    last_failure: Duration,
}

impl FailureEntry {
    /// The level only decays after a quiet period that starts when the
    /// last lockout ends, so sitting out a 24 h lockout does not by itself
    /// reset the attacker to the 15 min tier.
    fn quiet_since(&self) -> Duration {
        self.locked_until
            .map_or(self.last_failure, |until| until.max(self.last_failure))
    }
}

/// Counts wrong codes or passwords per IP (IPv6 by /64). `max` failures
/// within `window` lock the IP out; each further lockout uses the next,
/// longer duration. Lives in a std `Mutex`: every critical section is a
/// few map operations and never awaits.
pub struct FailureTracker {
    entries: Mutex<HashMap<IpAddr, FailureEntry>>,
    max: u32,
    window: Duration,
    lockouts: Vec<Duration>,
    level_reset: Duration,
    cap: usize,
    clock: Clock,
}

impl FailureTracker {
    pub fn new(cfg: &RateLimitConfig, clock: Clock) -> Self {
        let lockouts: Vec<Duration> = cfg
            .lockout_secs
            .iter()
            .map(|s| Duration::from_secs(*s))
            .collect();
        let level_reset = lockouts.iter().copied().max().unwrap_or_default();
        Self {
            entries: Mutex::new(HashMap::new()),
            max: cfg.fail_max.max(1),
            window: Duration::from_secs(cfg.fail_window_secs),
            lockouts,
            level_reset,
            cap: cfg.key_cap,
            clock,
        }
    }

    /// `Err` while `ip` is locked out — or, failing closed, when the table
    /// is full and `ip` is not in it, or the lock is poisoned.
    pub fn check(&self, ip: IpAddr) -> Result<(), RetryAfter> {
        let key = bucket_key(ip);
        let now = self.clock.elapsed();
        let Ok(entries) = self.entries.lock() else {
            tracing::error!("failure tracker lock poisoned; failing closed");
            return Err(FAIL_CLOSED_RETRY.into());
        };
        match entries.get(&key) {
            Some(entry) => match entry.locked_until {
                Some(until) if until > now => Err((until - now).into()),
                _ => Ok(()),
            },
            None if entries.len() >= self.cap => Err(FAIL_CLOSED_RETRY.into()),
            None => Ok(()),
        }
    }

    /// Records one failure. Returns the lockout length if this failure
    /// started a lockout.
    pub fn record_failure(&self, ip: IpAddr) -> Option<Duration> {
        let key = bucket_key(ip);
        let now = self.clock.elapsed();
        // A poisoned map still holds plain counters; keep recording.
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !entries.contains_key(&key) && entries.len() >= self.cap {
            // `check` already fails closed for this key; nothing to store.
            return None;
        }
        let entry = entries.entry(key).or_insert(FailureEntry {
            count: 0,
            window_start: now,
            locked_until: None,
            level: 0,
            last_failure: now,
        });

        if entry.level > 0 && now.saturating_sub(entry.quiet_since()) >= self.level_reset {
            entry.level = 0;
        }
        if now.saturating_sub(entry.window_start) >= self.window {
            entry.count = 0;
            entry.window_start = now;
        }
        entry.count += 1;
        entry.last_failure = now;

        if entry.count < self.max {
            return None;
        }
        let lockout = self
            .lockouts
            .get(entry.level)
            .or(self.lockouts.last())
            .copied()
            .unwrap_or(FAIL_CLOSED_RETRY);
        entry.locked_until = Some(now + lockout);
        entry.level = (entry.level + 1).min(self.lockouts.len().saturating_sub(1));
        entry.count = 0;
        entry.window_start = now;
        Some(lockout)
    }

    /// Forgets IPs that are neither locked, inside a counting window, nor
    /// still carrying a backoff level.
    pub fn maintain(&self) {
        let now = self.clock.elapsed();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, e| {
            let locked = e.locked_until.is_some_and(|until| until > now);
            let counting = e.count > 0 && now.saturating_sub(e.window_start) < self.window;
            let remembered = e.level > 0 && now.saturating_sub(e.quiet_since()) < self.level_reset;
            locked || counting || remembered
        });
        entries.shrink_to_fit();
    }

    pub fn len(&self) -> usize {
        self.entries.lock().map(|e| e.len()).unwrap_or(self.cap)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------
// Global throttle
// ---------------------------------------------------------------------

/// The sliding window is kept as this many fixed buckets: one minute each
/// for the one-hour code window, 15 s each for the 15-minute admin window.
const WINDOW_BUCKETS: usize = 60;

struct ThrottleState {
    counts: [u32; WINDOW_BUCKETS],
    /// Which bucket number each slot currently holds; a slot whose id has
    /// fallen out of the window counts as zero.
    ids: [u64; WINDOW_BUCKETS],
    active: bool,
    last_alert: Option<Duration>,
}

/// The throttle switched on or off and an alert is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrottleChange {
    pub active: bool,
    pub failures: u32,
}

/// Counts failures from every IP over a sliding window. Above `tier1`
/// failures every attempt waits `delay1` before it is checked, above
/// `tier2` it waits `delay2`. Waiting attempts each hold a permit; when
/// none is free the attempt gets a 429 instead, which caps guessing
/// throughput at `permits` attempts per delay.
pub struct GlobalThrottle {
    name: &'static str,
    cfg: ThrottleConfig,
    bucket_width: Duration,
    delay1: Duration,
    delay2: Duration,
    alert_interval: Duration,
    state: Mutex<ThrottleState>,
    permits: Arc<Semaphore>,
    clock: Clock,
}

impl GlobalThrottle {
    pub fn new(
        name: &'static str,
        cfg: ThrottleConfig,
        limits: &RateLimitConfig,
        clock: Clock,
    ) -> Self {
        let window = Duration::from_secs(cfg.window_secs);
        Self {
            name,
            cfg,
            bucket_width: (window / WINDOW_BUCKETS as u32).max(Duration::from_millis(1)),
            delay1: Duration::from_millis(limits.delay1_ms),
            delay2: Duration::from_millis(limits.delay2_ms),
            alert_interval: Duration::from_secs(limits.alert_interval_secs),
            state: Mutex::new(ThrottleState {
                counts: [0; WINDOW_BUCKETS],
                ids: [0; WINDOW_BUCKETS],
                active: false,
                last_alert: None,
            }),
            permits: Arc::new(Semaphore::new(limits.delay_permits as usize)),
            clock,
        }
    }

    fn bucket_id(&self, now: Duration) -> u64 {
        (now.as_nanos() / self.bucket_width.as_nanos()) as u64
    }

    fn failures_in_window(&self, state: &ThrottleState, current: u64) -> u32 {
        let oldest = current.saturating_sub(WINDOW_BUCKETS as u64 - 1);
        state
            .ids
            .iter()
            .zip(state.counts.iter())
            .filter(|(id, _)| **id >= oldest && **id <= current)
            .map(|(_, count)| *count)
            .sum()
    }

    /// Re-evaluates on/off. Always logs a change; returns it only when an
    /// alert is also due (at most one per `alert_interval`).
    fn evaluate(&self, state: &mut ThrottleState, now: Duration) -> Option<ThrottleChange> {
        let failures = self.failures_in_window(state, self.bucket_id(now));
        let active = failures > self.cfg.tier1;
        if active == state.active {
            return None;
        }
        state.active = active;
        tracing::warn!(
            throttle = self.name,
            active,
            failures,
            window_secs = self.cfg.window_secs,
            "global login throttle {}",
            if active { "turned on" } else { "turned off" }
        );
        let due = state
            .last_alert
            .is_none_or(|last| now.saturating_sub(last) >= self.alert_interval);
        if !due {
            return None;
        }
        state.last_alert = Some(now);
        Some(ThrottleChange { active, failures })
    }

    pub fn record_failure(&self) -> Option<ThrottleChange> {
        let now = self.clock.elapsed();
        let id = self.bucket_id(now);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = (id % WINDOW_BUCKETS as u64) as usize;
        if state.ids[slot] != id {
            state.ids[slot] = id;
            state.counts[slot] = 0;
        }
        state.counts[slot] = state.counts[slot].saturating_add(1);
        self.evaluate(&mut state, now)
    }

    /// How long the next attempt must wait, if at all. A poisoned lock
    /// fails closed to the longest delay.
    pub fn current_delay(&self) -> (Option<Duration>, Option<ThrottleChange>) {
        let now = self.clock.elapsed();
        let Ok(mut state) = self.state.lock() else {
            tracing::error!(
                throttle = self.name,
                "throttle lock poisoned; failing closed"
            );
            return (Some(self.delay2), None);
        };
        let change = self.evaluate(&mut state, now);
        let failures = self.failures_in_window(&state, self.bucket_id(now));
        let delay = if failures > self.cfg.tier2 {
            Some(self.delay2)
        } else if failures > self.cfg.tier1 {
            Some(self.delay1)
        } else {
            None
        };
        (delay, change)
    }

    /// The semaphore delayed attempts hold a permit from.
    pub fn permits(&self) -> &Arc<Semaphore> {
        &self.permits
    }
}

// ---------------------------------------------------------------------
// Login limiter: failure tracker + global throttle + alerts
// ---------------------------------------------------------------------

/// Everything that guards one login endpoint. Call [`before_attempt`]
/// before checking credentials and [`record_failure`] when they are wrong.
///
/// [`before_attempt`]: LoginLimiter::before_attempt
/// [`record_failure`]: LoginLimiter::record_failure
pub struct LoginLimiter {
    label: &'static str,
    pub failures: FailureTracker,
    pub throttle: GlobalThrottle,
    notifier: Notifier,
    clock: Clock,
}

impl LoginLimiter {
    fn new(
        label: &'static str,
        throttle_cfg: ThrottleConfig,
        cfg: &RateLimitConfig,
        clock: Clock,
        notifier: Notifier,
    ) -> Self {
        Self {
            label,
            failures: FailureTracker::new(cfg, clock.clone()),
            throttle: GlobalThrottle::new(label, throttle_cfg, cfg, clock.clone()),
            notifier,
            clock,
        }
    }

    /// Rejects a locked-out IP, then waits out the global throttle delay
    /// if one is active. The returned permit (if any) must be held until
    /// the attempt has been checked.
    pub async fn before_attempt(
        &self,
        ip: IpAddr,
    ) -> Result<Option<OwnedSemaphorePermit>, RetryAfter> {
        if let Err(retry) = self.failures.check(ip) {
            tracing::info!(%ip, limiter = self.label, "login attempt rejected: locked out");
            return Err(retry);
        }
        let (delay, change) = self.throttle.current_delay();
        self.alert(change);
        let Some(delay) = delay else {
            return Ok(None);
        };
        // A closed semaphore is as good as a full one: limited.
        let Ok(permit) = self.throttle.permits().clone().try_acquire_owned() else {
            tracing::info!(%ip, limiter = self.label, "login attempt rejected: throttle full");
            return Err(delay.into());
        };
        self.clock.sleep(delay).await;
        Ok(Some(permit))
    }

    pub fn record_failure(&self, ip: IpAddr) {
        if let Some(lockout) = self.failures.record_failure(ip) {
            tracing::warn!(
                %ip,
                limiter = self.label,
                lockout_secs = lockout.as_secs(),
                "too many failed login attempts; ip locked out"
            );
        }
        let change = self.throttle.record_failure();
        self.alert(change);
    }

    fn alert(&self, change: Option<ThrottleChange>) {
        let Some(change) = change else {
            return;
        };
        let (title, message, tag) = if change.active {
            (
                format!("{}: forsinkelse slått på", self.label),
                format!(
                    "{} mislykkede forsøk i løpet av {} minutter. Forsøk blir nå forsinket; \
                     riktige koder fungerer fortsatt.",
                    change.failures,
                    self.throttle.cfg.window_secs / 60
                ),
                "warning",
            )
        } else {
            (
                format!("{}: forsinkelse slått av", self.label),
                format!(
                    "Mislykkede forsøk er nede på {}. Forsøk blir ikke lenger forsinket.",
                    change.failures
                ),
                "white_check_mark",
            )
        };
        self.notifier.send(Message {
            title,
            message,
            tags: vec![tag],
            click: None,
            priority: change.active.then_some(4),
        });
    }

    fn maintain(&self) {
        self.failures.maintain();
        let (_, change) = self.throttle.current_delay();
        self.alert(change);
    }
}

// ---------------------------------------------------------------------
// All limiters
// ---------------------------------------------------------------------

/// Every limiter the app uses, built once from config and shared through
/// `AppState`.
///
/// The search handler calls [`RateLimits::check_search`] before any TMDB or
/// database work. The create-request quota (per minute, plus an optional
/// daily limit) is counted from `request_events` in the database so it
/// survives restarts, and so needs no in-memory limiter here.
pub struct RateLimits {
    api: KeyedLimiter<IpAddr>,
    search: KeyedLimiter<i64>,
    pub code_login: LoginLimiter,
    pub admin_login: LoginLimiter,
}

impl RateLimits {
    pub fn new(cfg: &RateLimitConfig, clock: Clock, notifier: Notifier) -> Self {
        let api_quota = Quota::per_minute(non_zero(cfg.api_per_minute));
        let search_quota = Quota::per_second(non_zero(cfg.search_per_second))
            .allow_burst(non_zero(cfg.search_burst));
        Self {
            api: KeyedLimiter::new(api_quota, cfg.key_cap, clock.clone()),
            search: KeyedLimiter::new(search_quota, cfg.key_cap, clock.clone()),
            code_login: LoginLimiter::new(
                "Tilgangskode",
                cfg.code_throttle,
                cfg,
                clock.clone(),
                notifier.clone(),
            ),
            admin_login: LoginLimiter::new(
                "Admin-innlogging",
                cfg.admin_throttle,
                cfg,
                clock,
                notifier,
            ),
        }
    }

    /// The global per-IP limit on every `/api` call.
    pub fn check_api(&self, ip: IpAddr) -> Result<(), RetryAfter> {
        self.api.check(&bucket_key(ip))
    }

    /// Per-member search limit.
    pub fn check_search(&self, member_id: i64) -> Result<(), RetryAfter> {
        self.search.check(&member_id)
    }

    pub fn api_keys(&self) -> usize {
        self.api.len()
    }

    /// Run once a minute: evicts idle keys and notices a throttle turning
    /// off even when no attempts arrive.
    pub fn maintain(&self) {
        self.api.maintain();
        self.search.maintain();
        self.code_login.maintain();
        self.admin_login.maintain();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> Clock {
        Clock::fake(1_700_000_000)
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn v4(n: u32) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::from_bits(0x0a00_0000 + n))
    }

    fn limits(cfg: &RateLimitConfig, clock: &Clock) -> RateLimits {
        let notifier = Notifier::new(reqwest::Client::new(), Default::default());
        RateLimits::new(cfg, clock.clone(), notifier)
    }

    // --- RetryAfter ---------------------------------------------------

    #[test]
    fn retry_after_rounds_up_to_whole_seconds_and_is_never_zero() {
        assert_eq!(RetryAfter::from(Duration::ZERO), RetryAfter(1));
        assert_eq!(RetryAfter::from(Duration::from_millis(200)), RetryAfter(1));
        assert_eq!(RetryAfter::from(Duration::from_secs(2)), RetryAfter(2));
        assert_eq!(RetryAfter::from(Duration::from_millis(2001)), RetryAfter(3));
    }

    // --- Keyed limiters -----------------------------------------------

    #[test]
    fn search_burst_refill_and_retry_after() {
        let clock = clock();
        let rl = limits(&RateLimitConfig::default(), &clock);

        for _ in 0..10 {
            rl.check_search(1).unwrap();
        }
        // Refill is 5/s: the next cell is 200 ms away, rounded up to 1 s.
        assert_eq!(rl.check_search(1), Err(RetryAfter(1)));
        // Another member has its own bucket.
        rl.check_search(2).unwrap();

        clock.advance(Duration::from_millis(200));
        rl.check_search(1).unwrap();
        assert!(rl.check_search(1).is_err());

        // A full second refills five cells, no more.
        clock.advance(Duration::from_secs(1));
        for _ in 0..5 {
            rl.check_search(1).unwrap();
        }
        assert!(rl.check_search(1).is_err());
    }

    #[test]
    fn api_limit_allows_120_per_minute_per_ip() {
        let clock = clock();
        let rl = limits(&RateLimitConfig::default(), &clock);
        let a = ip("203.0.113.1");

        for _ in 0..120 {
            rl.check_api(a).unwrap();
        }
        assert_eq!(rl.check_api(a), Err(RetryAfter(1)));
        rl.check_api(ip("203.0.113.2")).unwrap();

        clock.advance(Duration::from_secs(30));
        for _ in 0..60 {
            rl.check_api(a).unwrap();
        }
        assert!(rl.check_api(a).is_err());
    }

    #[test]
    fn retry_after_reflects_slow_refill() {
        let clock = clock();
        let cfg = RateLimitConfig {
            api_per_minute: 3,
            ..Default::default()
        };
        let rl = limits(&cfg, &clock);
        let a = ip("203.0.113.1");
        for _ in 0..3 {
            rl.check_api(a).unwrap();
        }
        // 3 per minute refills one every 20 s.
        assert_eq!(rl.check_api(a), Err(RetryAfter(20)));
        clock.advance(Duration::from_secs(15));
        assert_eq!(rl.check_api(a), Err(RetryAfter(5)));
        clock.advance(Duration::from_secs(5));
        rl.check_api(a).unwrap();
    }

    #[test]
    fn ipv6_addresses_in_one_64_share_an_api_bucket() {
        let clock = clock();
        let cfg = RateLimitConfig {
            api_per_minute: 2,
            ..Default::default()
        };
        let rl = limits(&cfg, &clock);
        rl.check_api(ip("2001:db8:1:2::1")).unwrap();
        rl.check_api(ip("2001:db8:1:2:ffff::9")).unwrap();
        assert!(rl.check_api(ip("2001:db8:1:2::abcd")).is_err());
        // A different /64 is a different bucket.
        rl.check_api(ip("2001:db8:1:3::1")).unwrap();
    }

    #[test]
    fn key_table_never_exceeds_cap_and_new_keys_beyond_it_are_limited() {
        let clock = clock();
        let limiter: KeyedLimiter<u32> =
            KeyedLimiter::new(Quota::per_second(non_zero(1)), 10_000, clock.clone());

        let mut limited = 0;
        for key in 0..100_000u32 {
            if limiter.check(&key).is_err() {
                limited += 1;
            }
            assert!(limiter.len() <= 10_000, "cap exceeded at key {key}");
        }
        assert_eq!(limiter.len(), 10_000);
        assert_eq!(limited, 90_000);
        assert_eq!(limiter.check(&100_001), Err(RetryAfter(60)));

        // Keys already in the table are still served normally.
        clock.advance(Duration::from_secs(1));
        limiter.check(&5).unwrap();

        // Once idle keys are evicted there is room again.
        clock.advance(Duration::from_secs(5));
        limiter.maintain();
        assert!(limiter.is_empty());
        limiter.check(&100_001).unwrap();
    }

    // --- Failure tracker ----------------------------------------------

    #[test]
    fn five_failures_lock_out_with_growing_backoff() {
        let clock = clock();
        let tracker = FailureTracker::new(&RateLimitConfig::default(), clock.clone());
        let a = ip("198.51.100.7");

        for _ in 0..4 {
            assert_eq!(tracker.record_failure(a), None);
            tracker.check(a).unwrap();
        }
        assert_eq!(tracker.record_failure(a), Some(Duration::from_secs(900)));
        assert_eq!(tracker.check(a), Err(RetryAfter(900)));
        // Other IPs are unaffected.
        tracker.check(ip("198.51.100.8")).unwrap();

        clock.advance(Duration::from_secs(600));
        assert_eq!(tracker.check(a), Err(RetryAfter(300)));
        clock.advance(Duration::from_secs(300));
        tracker.check(a).unwrap();

        for _ in 0..4 {
            tracker.record_failure(a);
        }
        assert_eq!(tracker.record_failure(a), Some(Duration::from_secs(3600)));
        clock.advance(Duration::from_secs(3600));
        for _ in 0..4 {
            tracker.record_failure(a);
        }
        assert_eq!(tracker.record_failure(a), Some(Duration::from_secs(86_400)));
        // The longest tier repeats.
        clock.advance(Duration::from_secs(86_400));
        for _ in 0..4 {
            tracker.record_failure(a);
        }
        assert_eq!(tracker.record_failure(a), Some(Duration::from_secs(86_400)));
    }

    #[test]
    fn failures_outside_the_window_do_not_add_up() {
        let clock = clock();
        let tracker = FailureTracker::new(&RateLimitConfig::default(), clock.clone());
        let a = ip("198.51.100.7");
        for _ in 0..4 {
            tracker.record_failure(a);
        }
        clock.advance(Duration::from_secs(901));
        for _ in 0..4 {
            assert_eq!(tracker.record_failure(a), None);
        }
        tracker.check(a).unwrap();
    }

    #[test]
    fn backoff_level_resets_after_a_quiet_day_after_the_lockout() {
        let clock = clock();
        let tracker = FailureTracker::new(&RateLimitConfig::default(), clock.clone());
        let a = ip("198.51.100.7");
        for _ in 0..5 {
            tracker.record_failure(a);
        }
        // Lockout ends after 15 min; a quiet day must follow from then.
        clock.advance(Duration::from_secs(900 + 86_400 - 1));
        for _ in 0..4 {
            tracker.record_failure(a);
        }
        assert_eq!(tracker.record_failure(a), Some(Duration::from_secs(3600)));

        clock.advance(Duration::from_secs(3600 + 86_400));
        for _ in 0..4 {
            tracker.record_failure(a);
        }
        assert_eq!(tracker.record_failure(a), Some(Duration::from_secs(900)));
    }

    #[test]
    fn ipv6_addresses_in_one_64_share_a_failure_count() {
        let clock = clock();
        let tracker = FailureTracker::new(&RateLimitConfig::default(), clock);
        for i in 1..=5u16 {
            tracker.record_failure(IpAddr::V6(std::net::Ipv6Addr::new(
                0x2001, 0xdb8, 0, 0, i, 0, 0, i,
            )));
        }
        assert!(tracker.check(ip("2001:db8::ffff")).is_err());
        tracker.check(ip("2001:db8:0:1::1")).unwrap();
    }

    #[test]
    fn failure_table_is_capped_and_fails_closed_for_new_ips() {
        let clock = clock();
        let tracker = FailureTracker::new(&RateLimitConfig::default(), clock.clone());
        for n in 0..100_000 {
            tracker.record_failure(v4(n));
            assert!(tracker.len() <= 10_000);
        }
        assert_eq!(tracker.len(), 10_000);
        // A new IP is limited, a tracked one with room left is not.
        assert_eq!(tracker.check(v4(100_000)), Err(RetryAfter(60)));
        tracker.check(v4(1)).unwrap();

        clock.advance(Duration::from_secs(901));
        tracker.maintain();
        assert!(tracker.is_empty());
        tracker.check(v4(100_000)).unwrap();
    }

    #[test]
    fn maintain_keeps_locked_and_backoff_entries() {
        let clock = clock();
        let tracker = FailureTracker::new(&RateLimitConfig::default(), clock.clone());
        let a = ip("198.51.100.7");
        for _ in 0..5 {
            tracker.record_failure(a);
        }
        clock.advance(Duration::from_secs(3600));
        tracker.maintain();
        assert_eq!(tracker.len(), 1, "backoff level must be remembered");
        clock.advance(Duration::from_secs(86_400));
        tracker.maintain();
        assert!(tracker.is_empty());
    }

    #[test]
    fn poisoned_failure_tracker_fails_closed() {
        let tracker = Arc::new(FailureTracker::new(&RateLimitConfig::default(), clock()));
        let t = tracker.clone();
        let _ = std::thread::spawn(move || {
            let _guard = t.entries.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert!(tracker.entries.is_poisoned());
        assert_eq!(tracker.check(ip("203.0.113.1")), Err(RetryAfter(60)));
    }

    // --- Global throttle ----------------------------------------------

    fn code_throttle(clock: &Clock) -> GlobalThrottle {
        let cfg = RateLimitConfig::default();
        GlobalThrottle::new("test", cfg.code_throttle, &cfg, clock.clone())
    }

    #[test]
    fn throttle_tiers_follow_failures_in_the_last_hour() {
        let clock = clock();
        let throttle = code_throttle(&clock);
        for _ in 0..60 {
            throttle.record_failure();
        }
        assert_eq!(throttle.current_delay().0, None);
        throttle.record_failure();
        assert_eq!(throttle.current_delay().0, Some(Duration::from_secs(1)));
        for _ in 61..300 {
            throttle.record_failure();
        }
        assert_eq!(throttle.current_delay().0, Some(Duration::from_secs(1)));
        throttle.record_failure();
        assert_eq!(throttle.current_delay().0, Some(Duration::from_secs(3)));

        // The window slides: an hour later every failure has aged out.
        clock.advance(Duration::from_secs(3600));
        assert_eq!(throttle.current_delay().0, None);
    }

    #[test]
    fn throttle_window_slides_minute_by_minute() {
        let clock = clock();
        let throttle = code_throttle(&clock);
        for _ in 0..40 {
            throttle.record_failure();
        }
        clock.advance(Duration::from_secs(30 * 60));
        for _ in 0..21 {
            throttle.record_failure();
        }
        assert!(throttle.current_delay().0.is_some());
        // The first 40 age out after an hour; the later 21 remain.
        clock.advance(Duration::from_secs(31 * 60));
        assert_eq!(throttle.current_delay().0, None);
        let state = throttle.state.lock().unwrap();
        let current = throttle.bucket_id(clock.elapsed());
        assert_eq!(throttle.failures_in_window(&state, current), 21);
    }

    #[test]
    fn throttle_reports_on_and_off_once_and_alerts_at_most_hourly() {
        let clock = clock();
        let throttle = code_throttle(&clock);
        let changes: Vec<_> = (0..100).filter_map(|_| throttle.record_failure()).collect();
        assert_eq!(
            changes,
            vec![ThrottleChange {
                active: true,
                failures: 61
            }]
        );

        // Turning off 61 minutes later is a change, and an hour has passed.
        clock.advance(Duration::from_secs(61 * 60));
        let (delay, change) = throttle.current_delay();
        assert_eq!(delay, None);
        assert_eq!(
            change,
            Some(ThrottleChange {
                active: false,
                failures: 0
            })
        );

        // Flapping back on within the hour logs but does not alert again.
        let changes: Vec<_> = (0..61).filter_map(|_| throttle.record_failure()).collect();
        assert!(changes.is_empty());
        assert!(throttle.state.lock().unwrap().active);
    }

    #[test]
    fn poisoned_throttle_fails_closed_to_longest_delay() {
        let throttle = Arc::new(code_throttle(&clock()));
        let t = throttle.clone();
        let _ = std::thread::spawn(move || {
            let _guard = t.state.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert_eq!(throttle.current_delay().0, Some(Duration::from_secs(3)));
    }

    #[test]
    fn admin_throttle_uses_its_own_window_and_tiers() {
        let clock = clock();
        let cfg = RateLimitConfig::default();
        let throttle = GlobalThrottle::new("admin", cfg.admin_throttle, &cfg, clock.clone());
        for _ in 0..31 {
            throttle.record_failure();
        }
        assert_eq!(throttle.current_delay().0, Some(Duration::from_secs(1)));
        for _ in 31..151 {
            throttle.record_failure();
        }
        assert_eq!(throttle.current_delay().0, Some(Duration::from_secs(3)));
        clock.advance(Duration::from_secs(15 * 60));
        assert_eq!(throttle.current_delay().0, None);
    }

    // --- Login limiter ------------------------------------------------

    #[tokio::test]
    async fn delayed_attempt_waits_on_the_clock_and_holds_a_permit() {
        let clock = clock();
        let rl = limits(&RateLimitConfig::default(), &clock);
        for n in 0..61 {
            rl.code_login.record_failure(v4(n));
        }
        let before = clock.elapsed();
        let permit = rl.code_login.before_attempt(v4(1000)).await.unwrap();
        assert!(permit.is_some());
        assert_eq!(clock.elapsed() - before, Duration::from_secs(1));
        assert_eq!(rl.code_login.throttle.permits().available_permits(), 31);
        drop(permit);
        assert_eq!(rl.code_login.throttle.permits().available_permits(), 32);
    }

    #[tokio::test]
    async fn full_or_closed_semaphore_is_limited_not_bypassed() {
        let clock = clock();
        let rl = limits(&RateLimitConfig::default(), &clock);
        for n in 0..301 {
            rl.code_login.record_failure(v4(n));
        }
        let held = rl
            .code_login
            .throttle
            .permits()
            .clone()
            .acquire_many_owned(32)
            .await
            .unwrap();
        assert_eq!(
            rl.code_login.before_attempt(v4(1000)).await.unwrap_err(),
            RetryAfter(3)
        );
        drop(held);

        rl.code_login.throttle.permits().close();
        assert!(rl.code_login.before_attempt(v4(1000)).await.is_err());
    }

    #[tokio::test]
    async fn code_and_admin_limiters_are_independent() {
        let clock = clock();
        let rl = limits(&RateLimitConfig::default(), &clock);
        let a = ip("198.51.100.7");
        for _ in 0..5 {
            rl.admin_login.record_failure(a);
        }
        assert!(rl.admin_login.before_attempt(a).await.is_err());
        assert!(rl.code_login.before_attempt(a).await.unwrap().is_none());
    }
}
