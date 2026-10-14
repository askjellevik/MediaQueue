//! Background maintenance spawned once at startup. The daily backup task
//! arrives in Phase 7.

use std::time::Duration;

use sqlx::SqlitePool;

use crate::db;

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
