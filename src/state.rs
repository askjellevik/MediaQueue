//! `AppState` is the one piece of shared state cloned into every handler.
//! The TMDB client and its caches live here too.

use std::{sync::Arc, time::Duration};

use sqlx::SqlitePool;
use tokio::sync::Semaphore;

use crate::{
    clock::Clock, config::Config, events::Hub, notify::Notifier, ratelimit::RateLimits, tmdb::Tmdb,
};

/// Upstream calls (TMDB, ntfy) must never hold a request open for long.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    pub clock: Clock,
    /// The one shared outbound HTTP client (connection pooling, rustls).
    pub http: reqwest::Client,
    pub notifier: Notifier,
    pub limits: Arc<RateLimits>,
    pub tmdb: Arc<Tmdb>,
    /// Live-update ticks for open event streams.
    pub events: Hub,
    /// The same pool as `pool`, leaked once per state so the streamed CSV
    /// export can borrow it for `'static` (its row stream outlives the
    /// handler). Costs one pointer for the life of the process.
    pub export_pool: &'static SqlitePool,
    /// Cloudflare Access verifier, when switched on.
    #[cfg(feature = "cf-access")]
    pub cf_access: Option<Arc<crate::cf_access::CfAccess>>,
    /// Bounds concurrent Argon2id hashing to 1 permit, so a flood of admin
    /// logins queues instead of exhausting RAM on 19 MiB hashes at once.
    pub admin_login_semaphore: Arc<Semaphore>,
}

impl AppState {
    pub fn new(pool: SqlitePool, config: Config) -> anyhow::Result<Self> {
        Self::with_clock(pool, config, Clock::system())
    }

    /// Like [`AppState::new`] but with a given clock, so tests can drive
    /// time-based limits without sleeping.
    pub fn with_clock(pool: SqlitePool, config: Config, clock: Clock) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(UPSTREAM_TIMEOUT)
            .timeout(UPSTREAM_TIMEOUT)
            .build()?;
        let notifier = Notifier::new(http.clone(), config.ntfy.clone());
        let limits = RateLimits::new(&config.rate_limits, clock.clone(), notifier.clone());
        let tmdb = Tmdb::new(http.clone(), &config, clock.clone());
        let export_pool: &'static SqlitePool = Box::leak(Box::new(pool.clone()));
        #[cfg(feature = "cf-access")]
        let cf_access = config.cf_access.as_ref().map(|cf| {
            Arc::new(crate::cf_access::CfAccess::new(
                http.clone(),
                cf,
                clock.clone(),
            ))
        });
        Ok(Self {
            pool,
            config: Arc::new(config),
            clock,
            http,
            notifier,
            limits: Arc::new(limits),
            tmdb: Arc::new(tmdb),
            events: Hub::new(),
            export_pool,
            #[cfg(feature = "cf-access")]
            cf_access,
            admin_login_semaphore: Arc::new(Semaphore::new(1)),
        })
    }
}
