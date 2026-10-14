//! `AppState` is the one piece of shared state cloned into every handler.
//! Later phases add the TMDB caches here.

use std::{sync::Arc, time::Duration};

use sqlx::SqlitePool;
use tokio::sync::Semaphore;

use crate::{clock::Clock, config::Config, notify::Notifier, ratelimit::RateLimits};

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
        Ok(Self {
            pool,
            config: Arc::new(config),
            clock,
            http,
            notifier,
            limits: Arc::new(limits),
            admin_login_semaphore: Arc::new(Semaphore::new(1)),
        })
    }
}
