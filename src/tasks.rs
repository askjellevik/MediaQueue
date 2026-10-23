//! Background maintenance spawned once at startup: hourly session cleanup,
//! per-minute rate-limit housekeeping and the daily backup.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use sqlx::SqlitePool;

use crate::{config::WorkerMode, db, events::Tick, ratelimit::RateLimits, state::AppState};

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

const AUTO_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// One pass of the auto sweep: nothing unless the worker is on and in
/// `auto` mode, otherwise the jobs [`db::sweep_auto_jobs`] queues, and a tick
/// for the admin page when there are any. Returns how many.
pub async fn auto_sweep_once(state: &AppState) -> Result<usize, sqlx::Error> {
    let settings = db::worker_settings(&state.pool, &state.config).await?;
    if !settings.enabled || settings.mode != WorkerMode::Auto {
        return Ok(0);
    }
    let now = i64::try_from(state.clock.unix_secs()).unwrap_or(i64::MAX);
    let created =
        db::sweep_auto_jobs(&state.pool, now, state.config.auto_fetch_per_member_day).await?;
    if !created.is_empty() {
        state.events.publish(Tick::Admin);
    }
    Ok(created.len())
}

/// Once a minute, queues automatic jobs for the members who opted in. The
/// log carries counts only.
pub fn spawn_auto_sweep(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(AUTO_SWEEP_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;
            match auto_sweep_once(&state).await {
                Ok(0) => {}
                Ok(count) => tracing::info!(count, "automatic jobs queued"),
                Err(error) => tracing::error!(%error, "automatic sweep failed"),
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

// ---------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------

/// Backups kept in the backup directory; older ones are deleted.
pub const BACKUPS_KEPT: usize = 14;
const BACKUP_PREFIX: &str = "db-";
const BACKUP_SUFFIX: &str = ".sqlite3";
const BACKUP_EVERY: Duration = Duration::from_secs(24 * 3600);
const BACKUP_CHECK_INTERVAL: Duration = Duration::from_secs(3600);

/// `/data/backups` for a database at `/data/db.sqlite3`.
pub fn backup_dir(database_path: &Path) -> PathBuf {
    database_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("backups")
}

fn is_backup(name: &str) -> bool {
    name.starts_with(BACKUP_PREFIX) && name.ends_with(BACKUP_SUFFIX)
}

/// Backup files in `dir`, oldest first (their names sort by time).
fn list_backups(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(is_backup))
        .map(|e| e.path())
        .collect();
    found.sort();
    Ok(found)
}

/// Writes `dir/db-<UTC time>.sqlite3` with `VACUUM INTO`, then deletes
/// all but the newest [`BACKUPS_KEPT`]. Returns the new file's path.
pub async fn backup_now(pool: &SqlitePool, dir: &Path, now: i64) -> anyhow::Result<PathBuf> {
    let stamp = db::compact_utc(pool, now).await?;
    let path = dir.join(format!("{BACKUP_PREFIX}{stamp}{BACKUP_SUFFIX}"));
    let path_text = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("backup path is not valid UTF-8"))?
        .to_string();

    let create_dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::create_dir_all(create_dir)).await??;
    db::vacuum_into(pool, &path_text).await?;

    let prune_dir = dir.to_path_buf();
    let removed = tokio::task::spawn_blocking(move || -> std::io::Result<usize> {
        let backups = list_backups(&prune_dir)?;
        let excess = backups.len().saturating_sub(BACKUPS_KEPT);
        for old in &backups[..excess] {
            std::fs::remove_file(old)?;
        }
        Ok(excess)
    })
    .await??;
    tracing::info!(path = %path.display(), removed, "database backup written");
    Ok(path)
}

/// Age of the newest backup, or `None` if there is none yet.
fn newest_backup_age(dir: &Path) -> Option<Duration> {
    let newest = list_backups(dir).ok()?.pop()?;
    let modified = std::fs::metadata(newest).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// Checks once an hour and writes a backup when the newest is a day old
/// (or missing), so restarts never skip or double a day's backup.
pub fn spawn_daily_backup(pool: SqlitePool, database_path: PathBuf) {
    let dir = backup_dir(&database_path);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(BACKUP_CHECK_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;
            let check_dir = dir.clone();
            let age = tokio::task::spawn_blocking(move || newest_backup_age(&check_dir))
                .await
                .ok()
                .flatten();
            if age.is_some_and(|age| age < BACKUP_EVERY) {
                continue;
            }
            if let Err(error) = backup_now(&pool, &dir, db::now_unix()).await {
                tracing::error!(%error, "database backup failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backups_live_next_to_the_database() {
        assert_eq!(
            backup_dir(Path::new("/data/db.sqlite3")),
            PathBuf::from("/data/backups")
        );
        assert!(is_backup("db-20231114T221320Z.sqlite3"));
        assert!(!is_backup("db.sqlite3"));
        assert!(!is_backup("db-1.sqlite3-wal"));
    }
}
