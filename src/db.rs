//! SQLite pool setup, pragmas and the single place migrations are run from.
//! All SQL in the app lives in this module (or modules it grows into per
//! phase); handlers call functions here, never `sqlx::query!` directly.

use std::time::Duration;

use sqlx::{
    Sqlite, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::config::Config;

const MAX_CONNECTIONS: u32 = 4;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Opens the pool with the pragmas the spec requires and runs pending
/// migrations. Creates the database file's parent directory if needed,
/// since SQLite will create the file but not the directory.
pub async fn connect(cfg: &Config) -> Result<SqlitePool, sqlx::Error> {
    if let Some(parent) = cfg.database_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                sqlx::Error::Configuration(
                    format!("creating database directory {parent:?}: {e}").into(),
                )
            })?;
        }
    }

    let options = SqliteConnectOptions::new()
        .filename(&cfg.database_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(BUSY_TIMEOUT)
        .pragma("temp_store", "MEMORY");

    let pool = SqlitePoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .connect_with(options)
        .await?;

    sqlx::migrate!()
        .run(&pool)
        .await
        .map_err(|e| sqlx::Error::Configuration(format!("running migrations: {e}").into()))?;

    Ok(pool)
}

/// Starts a write transaction with `BEGIN IMMEDIATE`, so SQLite takes the
/// write lock up front instead of failing a later lock upgrade mid-transaction.
pub async fn begin_immediate(
    pool: &SqlitePool,
) -> Result<Transaction<'static, Sqlite>, sqlx::Error> {
    pool.begin_with("BEGIN IMMEDIATE").await
}

/// Cheap liveness probe used by `/healthz` and nothing else.
pub async fn healthcheck(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}

/// Flushes the WAL back into the main database file. Called on graceful
/// shutdown so an SD card power cut doesn't lose the WAL contents.
pub async fn checkpoint(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> SqlitePool {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("connect to in-memory sqlite");
        sqlx::migrate!().run(&pool).await.expect("run migrations");
        pool
    }

    #[tokio::test]
    async fn migrations_apply_to_empty_db() {
        let pool = test_pool().await;
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM members")
            .fetch_one(&pool)
            .await
            .expect("members table should exist and be empty");
        assert_eq!(row.0, 0);
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let pool = test_pool().await;
        // Running the migrator again against an already-migrated pool must
        // not error (it should see every migration as already applied).
        sqlx::migrate!()
            .run(&pool)
            .await
            .expect("re-running migrations must be a no-op");
    }

    #[tokio::test]
    async fn healthcheck_succeeds_on_fresh_db() {
        let pool = test_pool().await;
        healthcheck(&pool).await.expect("healthcheck should pass");
    }

    #[tokio::test]
    async fn begin_immediate_commits() {
        let pool = test_pool().await;
        let mut tx = begin_immediate(&pool).await.expect("begin immediate");
        sqlx::query("INSERT INTO members (name, code_hash, created_at) VALUES ('a', x'00', 0)")
            .execute(&mut *tx)
            .await
            .expect("insert");
        tx.commit().await.expect("commit");

        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM members")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(row.0, 1);
    }
}
