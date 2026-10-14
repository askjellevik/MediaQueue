//! Background maintenance spawned once at startup: hourly session cleanup
//! and per-minute rate-limit housekeeping. The daily backup task arrives
//! in Phase 7.

use std::{sync::Arc, time::Duration};

use sqlx::SqlitePool;

use crate::{db, ratelimit::RateLimits};

const CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);

/// Deletes expired sessions once an hour for the life of the process.
pub fn spawn_session_cleanup(pool: SqlitePool) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CLEANUP_INTERVAL);
        // The first tick fires immediately; skip it so we don't run right
        // after startup with nothing yet to clean up.
        interval.tick().await;
        loop {
            interval.tick().await;
            let now = db::now_unix();
            match db::delete_expired_sessions(&pool, now).await {
                Ok(0) => {}
                Ok(count) => tracing::info!(count, "cleaned up expired sessions"),
                Err(error) => tracing::error!(%error, "session cleanup failed"),
            }
        }
    });
}

const RATE_LIMIT_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);

/// Once a minute, evicts idle rate-limit keys so the tables stay small,
/// and lets the global throttle notice it has turned off even when no
/// further attempts arrive.
pub fn spawn_rate_limit_maintenance(limits: Arc<RateLimits>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RATE_LIMIT_MAINTENANCE_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;
            limits.maintain();
        }
    });
}
