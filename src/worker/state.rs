//! The worker's local counters: how many torrents it added today, which
//! torrents it owns, and whether Real-Debrid has told it to stay away.
//!
//! One small JSON file, written to a temp file in the same directory and
//! renamed, so a crash leaves either the old or the new content. The file
//! fails closed: if it exists but cannot be trusted (unreadable, corrupt,
//! another version, a date in the future) the caps count as reached and
//! nothing is ever written over it, so the admin can look at what is there.
//! It holds counters and torrent ids only, never the token.

use std::{
    fs,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "counters.json";
const VERSION: u32 = 1;

const OWNED_MAX: usize = 200;
const OWNED_MAX_AGE_SECS: u64 = 7 * 24 * 3600;
const PENDING_DELETE_MAX: usize = 200;

const RATE_LIMIT_FIRST_SECS: u64 = 15 * 60;
const BREAKER_MAX_SECS: u64 = 6 * 3600;
/// What `breaker_open` reports after a bad token: far more than any run,
/// and gone with the process.
const UNTIL_RESTART: Duration = Duration::from_secs(7 * 24 * 3600);

pub trait Clock: Send + Sync + 'static {
    fn now_unix(&self) -> u64;
}

#[derive(Debug, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
}

/// Why Real-Debrid is being left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trip {
    /// Error 34 or HTTP 429: 15 minutes, doubling per consecutive trip, at
    /// most 6 hours.
    RateLimit,
    /// Error 9 or HTTP 403: 6 hours.
    Forbidden,
    /// Error 8 or HTTP 401: until the worker restarts. Not persisted.
    Auth,
}

/// The daily caps the store checks against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_adds: u32,
    pub max_uncached: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_adds: 5,
            max_uncached: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owned {
    pub id: String,
    pub hash: String,
    pub at: u64,
}

/// The file's content, exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub version: u32,
    /// UTC date, `YYYY-MM-DD`, the counters below belong to.
    pub date: String,
    pub adds: u32,
    pub uncached: u32,
    pub breaker_until: u64,
    pub breaker_trips: u32,
    pub owned: Vec<Owned>,
    pub pending_delete: Vec<String>,
}

impl State {
    fn fresh(date: String) -> Self {
        Self {
            version: VERSION,
            date,
            adds: 0,
            uncached: 0,
            breaker_until: 0,
            breaker_trips: 0,
            owned: Vec::new(),
            pending_delete: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("the counters file cannot be trusted")]
    Unreadable,
    #[error("the daily cap is reached")]
    CapReached,
    #[error("the counters file could not be written: {0:?}")]
    Write(io::ErrorKind),
}

struct Cell {
    state: State,
    unreadable: bool,
    auth_failed: bool,
    /// Which trip opened the breaker; not persisted, so after a restart an
    /// open breaker reads as a rate limit.
    last_trip: Option<Trip>,
}

struct Inner {
    path: PathBuf,
    clock: Arc<dyn Clock>,
    limits: Limits,
    cell: Mutex<Cell>,
}

/// Cheap to clone; every clone shares the same counters.
#[derive(Clone)]
pub struct StateStore {
    inner: Arc<Inner>,
}

impl StateStore {
    pub fn open(dir: &Path, clock: Arc<dyn Clock>) -> Self {
        Self::open_with(dir, clock, Limits::default())
    }

    pub fn open_with(dir: &Path, clock: Arc<dyn Clock>, limits: Limits) -> Self {
        let path = dir.join(FILE_NAME);
        let today = utc_date(clock.now_unix());
        let (state, unreadable) = match load(dir, &path, &today) {
            Ok(state) => (state, false),
            Err(why) => {
                tracing::error!(
                    why,
                    "counters file cannot be trusted; caps count as reached"
                );
                (State::fresh(today), true)
            }
        };
        Self {
            inner: Arc::new(Inner {
                path,
                clock,
                limits,
                cell: Mutex::new(Cell {
                    state,
                    unreadable,
                    auth_failed: false,
                    last_trip: None,
                }),
            }),
        }
    }

    pub fn now(&self) -> u64 {
        self.inner.clock.now_unix()
    }

    /// True when the file cannot be trusted. The caller raises the
    /// `counters_unreadable` alert.
    pub fn unreadable(&self) -> bool {
        self.lock_rolled().0.unreadable
    }

    pub fn adds_left(&self) -> u32 {
        let (cell, _) = self.lock_rolled();
        if cell.unreadable {
            return 0;
        }
        self.inner.limits.max_adds.saturating_sub(cell.state.adds)
    }

    pub fn uncached_left(&self) -> u32 {
        let (cell, _) = self.lock_rolled();
        if cell.unreadable {
            return 0;
        }
        self.inner
            .limits
            .max_uncached
            .saturating_sub(cell.state.uncached)
    }

    /// No further add is allowed today: either cap is used up, or the file
    /// cannot be trusted. Every add may turn out uncached, so the uncached
    /// cap stops adds too.
    pub fn caps_reached(&self) -> bool {
        self.adds_left() == 0 || self.uncached_left() == 0
    }

    /// Counts one add and writes the file; call it before `addMagnet`. An
    /// error means the add must not happen.
    pub fn record_add(&self) -> Result<(), StateError> {
        let max = self.inner.limits.max_adds;
        self.update(|state, _| {
            if state.adds >= max {
                return Err(StateError::CapReached);
            }
            state.adds += 1;
            Ok(())
        })
    }

    /// Counts one torrent that was not cached and had to be deleted.
    pub fn record_uncached(&self) -> Result<(), StateError> {
        self.update(|state, _| {
            state.uncached = state.uncached.saturating_add(1);
            Ok(())
        })
    }

    /// How long Real-Debrid is to be left alone, if at all.
    pub fn breaker_open(&self, now: u64) -> Option<Duration> {
        let (cell, _) = self.lock_rolled();
        if cell.auth_failed {
            return Some(UNTIL_RESTART);
        }
        cell.state
            .breaker_until
            .checked_sub(now)
            .filter(|left| *left > 0)
            .map(Duration::from_secs)
    }

    /// The trip that keeps Real-Debrid closed right now, if any.
    pub fn breaker_trip(&self, now: u64) -> Option<Trip> {
        let (cell, _) = self.lock_rolled();
        if cell.auth_failed {
            return Some(Trip::Auth);
        }
        (cell.state.breaker_until > now).then(|| cell.last_trip.unwrap_or(Trip::RateLimit))
    }

    /// Opens the breaker. It takes effect in memory even when the file
    /// cannot be written; the error says the write failed. A trip while the
    /// breaker is already open (a call that was in flight) changes nothing.
    pub fn trip(&self, trip: Trip, now: u64) -> Result<(), StateError> {
        let (mut cell, _) = self.lock_rolled();
        if trip == Trip::Auth {
            cell.auth_failed = true;
            cell.last_trip = Some(Trip::Auth);
            return Ok(());
        }
        if cell.auth_failed || cell.state.breaker_until > now {
            return Ok(());
        }
        match trip {
            Trip::RateLimit => {
                let wait = RATE_LIMIT_FIRST_SECS
                    .saturating_mul(1u64 << cell.state.breaker_trips.min(10))
                    .min(BREAKER_MAX_SECS);
                cell.state.breaker_until = now.saturating_add(wait);
                cell.state.breaker_trips = cell.state.breaker_trips.saturating_add(1);
            }
            Trip::Forbidden => {
                cell.state.breaker_until = now.saturating_add(BREAKER_MAX_SECS);
            }
            Trip::Auth => {}
        }
        cell.last_trip = Some(trip);
        self.persist(&cell)
    }

    /// A call succeeded: the next rate-limit trip starts at 15 minutes again.
    pub fn record_success(&self) {
        let (mut cell, _) = self.lock_rolled();
        if cell.state.breaker_trips == 0 {
            return;
        }
        cell.state.breaker_trips = 0;
        if let Err(e) = self.persist(&cell) {
            tracing::warn!(error = %e, "could not reset the breaker count");
        }
    }

    /// Remembers that this worker created `id`. Only owned ids are ever
    /// deleted.
    pub fn own(&self, id: &str, hash: &str) -> Result<(), StateError> {
        self.update(|state, now| {
            state.owned.retain(|o| o.id != id);
            state.owned.push(Owned {
                id: id.to_string(),
                hash: hash.to_string(),
                at: now,
            });
            let oldest_kept = now.saturating_sub(OWNED_MAX_AGE_SECS);
            state.owned.retain(|o| o.at >= oldest_kept);
            if state.owned.len() > OWNED_MAX {
                let excess = state.owned.len() - OWNED_MAX;
                state.owned.drain(..excess);
            }
            Ok(())
        })
    }

    pub fn owns(&self, id: &str) -> bool {
        self.lock_rolled().0.state.owned.iter().any(|o| o.id == id)
    }

    /// The infohash this worker added `id` for, if it owns `id`.
    pub fn owned_hash(&self, id: &str) -> Option<String> {
        let (cell, _) = self.lock_rolled();
        cell.state
            .owned
            .iter()
            .find(|o| o.id == id)
            .map(|o| o.hash.clone())
    }

    pub fn forget(&self, id: &str) -> Result<(), StateError> {
        self.update(|state, _| {
            state.owned.retain(|o| o.id != id);
            Ok(())
        })
    }

    /// A torrent that could not be deleted now; retried later.
    pub fn pending_delete_add(&self, id: &str) -> Result<(), StateError> {
        self.update(|state, _| {
            if !state.pending_delete.iter().any(|p| p == id) {
                state.pending_delete.push(id.to_string());
                if state.pending_delete.len() > PENDING_DELETE_MAX {
                    state.pending_delete.remove(0);
                }
            }
            Ok(())
        })
    }

    pub fn has_pending_delete(&self) -> bool {
        !self.lock_rolled().0.state.pending_delete.is_empty()
    }

    /// The ids to retry, still on the list: an id leaves it only through
    /// [`Self::pending_delete_done`], after its deletion went through.
    pub fn pending_delete_ids(&self) -> Vec<String> {
        self.lock_rolled().0.state.pending_delete.clone()
    }

    pub fn pending_delete_done(&self, id: &str) -> Result<(), StateError> {
        self.update(|state, _| {
            state.pending_delete.retain(|p| p != id);
            Ok(())
        })
    }

    /// Applies `change` to a copy, writes it, and only then keeps it, so
    /// memory never gets ahead of the file.
    fn update<T>(
        &self,
        change: impl FnOnce(&mut State, u64) -> Result<T, StateError>,
    ) -> Result<T, StateError> {
        let (mut cell, now) = self.lock_rolled();
        if cell.unreadable {
            return Err(StateError::Unreadable);
        }
        let mut next = cell.state.clone();
        let out = change(&mut next, now)?;
        write_atomic(&self.inner.path, &next).map_err(|e| StateError::Write(e.kind()))?;
        cell.state = next;
        Ok(out)
    }

    fn persist(&self, cell: &Cell) -> Result<(), StateError> {
        if cell.unreadable {
            return Err(StateError::Unreadable);
        }
        write_atomic(&self.inner.path, &cell.state).map_err(|e| StateError::Write(e.kind()))
    }

    /// Locks the cell after rolling the date: a new day zeroes the daily
    /// counters, a date in the future (the clock moved back, or the file is
    /// from elsewhere) makes the file untrusted.
    fn lock_rolled(&self) -> (MutexGuard<'_, Cell>, u64) {
        let now = self.inner.clock.now_unix();
        let today = utc_date(now);
        let mut cell = self
            .inner
            .cell
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if cell.state.date < today {
            cell.state.date = today;
            cell.state.adds = 0;
            cell.state.uncached = 0;
        } else if cell.state.date > today && !cell.unreadable {
            tracing::error!("counters file is dated in the future; caps count as reached");
            cell.unreadable = true;
        }
        (cell, now)
    }
}

/// The reason is a fixed phrase: it goes to the log.
fn load(dir: &Path, path: &Path, today: &str) -> Result<State, &'static str> {
    match fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        _ => return Err("state directory missing or not a directory"),
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(State::fresh(today.to_string()));
        }
        Err(_) => return Err("counters file cannot be read"),
    };
    let state: State = serde_json::from_slice(&bytes).map_err(|_| "counters file is corrupt")?;
    if state.version != VERSION {
        return Err("counters file has an unknown version");
    }
    if !is_date(&state.date) {
        return Err("counters file has a malformed date");
    }
    if state.date.as_str() > today {
        return Err("counters file is dated in the future");
    }
    Ok(state)
}

fn write_atomic(path: &Path, state: &State) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&json)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        // Best effort: the rename itself already happened.
        let _ = fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
}

/// `YYYY-MM-DD` in UTC (days-from-civil, Howard Hinnant's algorithm).
fn utc_date(unix: u64) -> String {
    let z = (unix / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Test helpers shared with the other worker tests.
#[cfg(test)]
pub(crate) mod testutil {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    pub(crate) struct FakeClock(AtomicU64);

    impl FakeClock {
        pub(crate) fn at(unix: u64) -> Arc<Self> {
            Arc::new(Self(AtomicU64::new(unix)))
        }
        pub(crate) fn set(&self, unix: u64) {
            self.0.store(unix, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now_unix(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    /// A fresh directory under the system temp dir, removed on drop.
    pub(crate) struct Dir(pub PathBuf);

    impl Dir {
        pub(crate) fn new() -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "mediaqueue-state-test-{nanos}-{}",
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        pub(crate) fn file(&self) -> PathBuf {
            self.0.join(FILE_NAME)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{Dir, FakeClock};
    use super::*;

    // 2026-10-20 12:00:00 UTC
    const NOON: u64 = 1_792_497_600;
    const DAY: u64 = 86_400;
    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn open(dir: &Dir, clock: &Arc<FakeClock>) -> StateStore {
        StateStore::open(&dir.0, clock.clone())
    }

    #[test]
    fn utc_date_matches_known_days() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(NOON), "2026-10-20");
        assert_eq!(utc_date(951_782_400), "2000-02-29");
        assert_eq!(utc_date(NOON + DAY * 12), "2026-11-01");
    }

    #[test]
    fn missing_file_is_zeros_and_not_written_until_needed() {
        let dir = Dir::new();
        let store = open(&dir, &FakeClock::at(NOON));
        assert!(!store.unreadable());
        assert_eq!(store.adds_left(), 5);
        assert_eq!(store.uncached_left(), 2);
        assert!(!store.caps_reached());
        assert!(!dir.file().exists());
    }

    #[test]
    fn adds_are_persisted_and_capped() {
        let dir = Dir::new();
        let clock = FakeClock::at(NOON);
        let store = open(&dir, &clock);
        for _ in 0..5 {
            store.record_add().unwrap();
        }
        assert_eq!(store.adds_left(), 0);
        assert!(store.caps_reached());
        assert!(matches!(store.record_add(), Err(StateError::CapReached)));
        let on_disk: State = serde_json::from_slice(&fs::read(dir.file()).unwrap()).unwrap();
        assert_eq!(on_disk.adds, 5);
    }

    #[test]
    fn uncached_cap_stops_adds_too() {
        let dir = Dir::new();
        let store = open(&dir, &FakeClock::at(NOON));
        store.record_uncached().unwrap();
        assert!(!store.caps_reached());
        store.record_uncached().unwrap();
        assert_eq!(store.uncached_left(), 0);
        assert!(store.caps_reached());
    }

    #[test]
    fn restart_keeps_counts_and_an_open_breaker() {
        let dir = Dir::new();
        let clock = FakeClock::at(NOON);
        let store = open(&dir, &clock);
        store.record_add().unwrap();
        store.record_uncached().unwrap();
        store.trip(Trip::RateLimit, NOON).unwrap();
        drop(store);

        let again = open(&dir, &clock);
        assert_eq!(again.adds_left(), 4);
        assert_eq!(again.uncached_left(), 1);
        assert_eq!(again.breaker_open(NOON), Some(Duration::from_secs(900)));
    }

    #[test]
    fn day_rollover_resets_counters_only() {
        let dir = Dir::new();
        let clock = FakeClock::at(NOON);
        let store = open(&dir, &clock);
        store.record_add().unwrap();
        store.record_uncached().unwrap();
        store.own("ABCDEFGHIJKLM", HASH).unwrap();
        // 23:00, so the breaker is still open after midnight.
        store.trip(Trip::Forbidden, NOON + 11 * 3600).unwrap();

        let after_midnight = NOON + 12 * 3600 + 1800;
        clock.set(after_midnight);
        assert_eq!(store.adds_left(), 5);
        assert_eq!(store.uncached_left(), 2);
        assert!(store.owns("ABCDEFGHIJKLM"));
        assert!(store.breaker_open(after_midnight).is_some());

        store.record_add().unwrap();
        let on_disk: State = serde_json::from_slice(&fs::read(dir.file()).unwrap()).unwrap();
        assert_eq!(on_disk.date, "2026-10-21");
        assert_eq!(on_disk.adds, 1);
    }

    fn assert_fails_closed(dir: &Dir, clock: &Arc<FakeClock>) {
        let store = open(dir, clock);
        assert!(store.unreadable());
        assert!(store.caps_reached());
        assert_eq!(store.adds_left(), 0);
        assert!(matches!(store.record_add(), Err(StateError::Unreadable)));
        assert!(matches!(
            store.own("ABC", HASH),
            Err(StateError::Unreadable)
        ));
    }

    #[test]
    fn corrupt_file_fails_closed_and_is_left_alone() {
        let dir = Dir::new();
        fs::write(dir.file(), b"{not json").unwrap();
        assert_fails_closed(&dir, &FakeClock::at(NOON));
        assert_eq!(fs::read(dir.file()).unwrap(), b"{not json");
    }

    #[test]
    fn unknown_version_and_unknown_fields_fail_closed() {
        let dir = Dir::new();
        let mut state = State::fresh("2026-10-20".into());
        state.version = 2;
        fs::write(dir.file(), serde_json::to_vec(&state).unwrap()).unwrap();
        assert_fails_closed(&dir, &FakeClock::at(NOON));

        let mut value = serde_json::to_value(State::fresh("2026-10-20".into())).unwrap();
        value["token"] = "x".into();
        fs::write(dir.file(), serde_json::to_vec(&value).unwrap()).unwrap();
        assert_fails_closed(&dir, &FakeClock::at(NOON));
    }

    #[test]
    fn future_date_fails_closed_at_open_and_later() {
        let dir = Dir::new();
        let state = State::fresh("2026-10-21".into());
        fs::write(dir.file(), serde_json::to_vec(&state).unwrap()).unwrap();
        assert_fails_closed(&dir, &FakeClock::at(NOON));

        let dir = Dir::new();
        let clock = FakeClock::at(NOON + DAY);
        let store = open(&dir, &clock);
        store.record_add().unwrap();
        clock.set(NOON);
        assert!(store.unreadable());
        assert!(store.caps_reached());
    }

    #[test]
    fn malformed_date_fails_closed() {
        let dir = Dir::new();
        let state = State::fresh("tomorrow".into());
        fs::write(dir.file(), serde_json::to_vec(&state).unwrap()).unwrap();
        assert_fails_closed(&dir, &FakeClock::at(NOON));
    }

    #[test]
    fn missing_directory_and_unreadable_file_fail_closed() {
        let clock = FakeClock::at(NOON);
        let missing = std::env::temp_dir().join("mediaqueue-state-test-no-such-dir");
        let store = StateStore::open(&missing, clock.clone());
        assert!(store.unreadable() && store.caps_reached());

        // A directory where the file should be cannot be read as a file.
        let dir = Dir::new();
        fs::create_dir(dir.file()).unwrap();
        assert_fails_closed(&dir, &clock);
    }

    #[test]
    fn failed_write_leaves_the_count_unchanged() {
        let dir = Dir::new();
        let store = open(&dir, &FakeClock::at(NOON));
        store.record_add().unwrap();
        fs::remove_dir_all(&dir.0).unwrap();
        assert!(matches!(store.record_add(), Err(StateError::Write(_))));
        assert_eq!(store.adds_left(), 4);
    }

    #[test]
    fn file_is_private_and_holds_only_the_known_fields() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new();
        let store = open(&dir, &FakeClock::at(NOON));
        store.record_add().unwrap();
        store.own("ABCDEFGHIJKLM", HASH).unwrap();
        store.pending_delete_add("NOPQRSTUVWXYZ").unwrap();

        let mode = fs::metadata(dir.file()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!dir.0.join("counters.json.tmp").exists());

        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.file()).unwrap()).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "adds",
                "breaker_trips",
                "breaker_until",
                "date",
                "owned",
                "pending_delete",
                "uncached",
                "version"
            ]
        );
        let owned = value["owned"][0].as_object().unwrap();
        let mut keys: Vec<_> = owned.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["at", "hash", "id"]);
    }

    #[test]
    fn rate_limit_breaker_doubles_and_caps_at_six_hours() {
        let dir = Dir::new();
        let clock = FakeClock::at(NOON);
        let store = open(&dir, &clock);
        let mut now = NOON;
        let mut waits = Vec::new();
        for _ in 0..7 {
            store.trip(Trip::RateLimit, now).unwrap();
            let left = store.breaker_open(now).unwrap();
            waits.push(left.as_secs());
            now += left.as_secs();
            assert_eq!(store.breaker_open(now), None);
        }
        assert_eq!(waits, [900, 1800, 3600, 7200, 14400, 21600, 21600]);

        store.record_success();
        store.trip(Trip::RateLimit, now).unwrap();
        assert_eq!(store.breaker_open(now), Some(Duration::from_secs(900)));
    }

    #[test]
    fn a_trip_while_open_changes_nothing() {
        let dir = Dir::new();
        let store = open(&dir, &FakeClock::at(NOON));
        store.trip(Trip::RateLimit, NOON).unwrap();
        store.trip(Trip::RateLimit, NOON + 10).unwrap();
        store.trip(Trip::Forbidden, NOON + 20).unwrap();
        assert_eq!(
            store.breaker_open(NOON + 10),
            Some(Duration::from_secs(890))
        );
    }

    #[test]
    fn breaker_trip_names_what_opened_it() {
        let dir = Dir::new();
        let store = open(&dir, &FakeClock::at(NOON));
        assert_eq!(store.breaker_trip(NOON), None);
        store.trip(Trip::Forbidden, NOON).unwrap();
        assert_eq!(store.breaker_trip(NOON), Some(Trip::Forbidden));
        assert_eq!(store.breaker_trip(NOON + 6 * 3600), None);
        store.trip(Trip::Auth, NOON).unwrap();
        assert_eq!(store.breaker_trip(NOON + 6 * 3600), Some(Trip::Auth));
    }

    #[test]
    fn forbidden_opens_six_hours() {
        let dir = Dir::new();
        let store = open(&dir, &FakeClock::at(NOON));
        store.trip(Trip::Forbidden, NOON).unwrap();
        assert_eq!(
            store.breaker_open(NOON),
            Some(Duration::from_secs(6 * 3600))
        );
        assert_eq!(store.breaker_open(NOON + 6 * 3600), None);
    }

    #[test]
    fn auth_trip_lasts_until_restart_and_is_not_persisted() {
        let dir = Dir::new();
        let clock = FakeClock::at(NOON);
        let store = open(&dir, &clock);
        store.trip(Trip::Auth, NOON).unwrap();
        assert!(store.breaker_open(NOON + 30 * DAY).is_some());
        assert!(!dir.file().exists());

        let restarted = open(&dir, &clock);
        assert_eq!(restarted.breaker_open(NOON), None);
    }

    #[test]
    fn trip_takes_effect_even_when_the_write_fails() {
        let dir = Dir::new();
        let store = open(&dir, &FakeClock::at(NOON));
        fs::remove_dir_all(&dir.0).unwrap();
        assert!(matches!(
            store.trip(Trip::Forbidden, NOON),
            Err(StateError::Write(_))
        ));
        assert!(store.breaker_open(NOON).is_some());
    }

    #[test]
    fn owned_ids_are_pruned_by_age_and_count() {
        let dir = Dir::new();
        let clock = FakeClock::at(NOON);
        let store = open(&dir, &clock);
        store.own("OLD", HASH).unwrap();
        clock.set(NOON + 8 * DAY);
        store.own("NEW", HASH).unwrap();
        assert!(!store.owns("OLD"));
        assert!(store.owns("NEW"));

        for i in 0..205 {
            store.own(&format!("ID{i}"), HASH).unwrap();
        }
        assert!(!store.owns("NEW"));
        assert!(!store.owns("ID0"));
        assert!(store.owns("ID5"));
        assert!(store.owns("ID204"));
        let on_disk: State = serde_json::from_slice(&fs::read(dir.file()).unwrap()).unwrap();
        assert_eq!(on_disk.owned.len(), 200);
    }

    #[test]
    fn has_pending_delete_follows_the_list() {
        let dir = Dir::new();
        let store = open(&dir, &FakeClock::at(NOON));
        assert!(!store.has_pending_delete());
        store.pending_delete_add("AAA").unwrap();
        assert!(store.has_pending_delete());
        store.pending_delete_done("AAA").unwrap();
        assert!(!store.has_pending_delete());
    }

    #[test]
    fn own_forget_and_pending_delete_round_trip() {
        let dir = Dir::new();
        let clock = FakeClock::at(NOON);
        let store = open(&dir, &clock);
        store.own("ABCDEFGHIJKLM", HASH).unwrap();
        assert!(store.owns("ABCDEFGHIJKLM"));
        assert!(!store.owns("OTHER"));
        store.forget("ABCDEFGHIJKLM").unwrap();
        assert!(!store.owns("ABCDEFGHIJKLM"));

        store.pending_delete_add("AAA").unwrap();
        store.pending_delete_add("AAA").unwrap();
        store.pending_delete_add("BBB").unwrap();
        drop(store);

        let again = open(&dir, &clock);
        // Reading the list does not empty it.
        assert_eq!(again.pending_delete_ids(), ["AAA", "BBB"]);
        assert_eq!(again.pending_delete_ids(), ["AAA", "BBB"]);
        again.pending_delete_done("AAA").unwrap();
        assert_eq!(again.pending_delete_ids(), ["BBB"]);
    }
}
