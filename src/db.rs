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

/// Current unix time in seconds. The one place request handlers get "now"
/// from in this phase; a later phase may swap this for an injectable clock
/// the way `tmdb.rs` does for cache freshness.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ---------------------------------------------------------------------
// Members
// ---------------------------------------------------------------------

pub struct Member {
    pub id: i64,
    pub name: String,
    pub max_sessions: i64,
    pub daily_limit: Option<i64>,
    pub revoked_at: Option<i64>,
}

/// Looks up a member by the HMAC of their access code. Does not filter on
/// `revoked_at` so the caller can give an identical response for "wrong",
/// "revoked" and "rotated" codes (a rotated code's hash simply won't match
/// any row, which already falls out of this query naturally).
pub async fn find_member_by_code_hash(
    pool: &SqlitePool,
    code_hash: &[u8],
) -> Result<Option<Member>, sqlx::Error> {
    sqlx::query_as!(
        Member,
        r#"SELECT id AS "id!", name AS "name!", max_sessions AS "max_sessions!",
                  daily_limit, revoked_at
           FROM members WHERE code_hash = ?"#,
        code_hash
    )
    .fetch_optional(pool)
    .await
}

/// Inserts a new member with the given (already-hashed) code. Used by the
/// `add-member` CLI command.
pub async fn insert_member(
    pool: &SqlitePool,
    name: &str,
    code_hash: &[u8],
    max_sessions: i64,
    daily_limit: Option<i64>,
    now: i64,
) -> Result<i64, sqlx::Error> {
    let rec = sqlx::query!(
        r#"INSERT INTO members (name, code_hash, max_sessions, daily_limit, created_at)
           VALUES (?, ?, ?, ?, ?)
           RETURNING id AS "id!""#,
        name,
        code_hash,
        max_sessions,
        daily_limit,
        now
    )
    .fetch_one(pool)
    .await?;
    Ok(rec.id)
}

// ---------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------

pub struct UserSession {
    pub session_id: i64,
    pub expires_at: i64,
    pub last_seen_at: i64,
    pub member_id: i64,
    pub member_name: String,
    pub daily_limit: Option<i64>,
}

/// Finds a live user session by its token hash, joined with the member it
/// belongs to. Expiry is checked by the caller, not here, so validation
/// stays in one place (`auth.rs`).
pub async fn find_user_session(
    pool: &SqlitePool,
    token_hash: &[u8],
) -> Result<Option<UserSession>, sqlx::Error> {
    sqlx::query_as!(
        UserSession,
        r#"SELECT s.id AS "session_id!", s.expires_at AS "expires_at!",
                  s.last_seen_at AS "last_seen_at!",
                  m.id AS "member_id!", m.name AS "member_name!", m.daily_limit
           FROM sessions s
           JOIN members m ON m.id = s.member_id
           WHERE s.token_hash = ? AND s.kind = 'user'"#,
        token_hash
    )
    .fetch_optional(pool)
    .await
}

pub struct AdminSession {
    pub session_id: i64,
    pub created_at: i64,
    pub expires_at: i64,
    pub last_seen_at: i64,
}

/// Finds a live admin session by its token hash.
pub async fn find_admin_session(
    pool: &SqlitePool,
    token_hash: &[u8],
) -> Result<Option<AdminSession>, sqlx::Error> {
    sqlx::query_as!(
        AdminSession,
        r#"SELECT id AS "session_id!", created_at AS "created_at!",
                  expires_at AS "expires_at!", last_seen_at AS "last_seen_at!"
           FROM sessions WHERE token_hash = ? AND kind = 'admin'"#,
        token_hash
    )
    .fetch_optional(pool)
    .await
}

/// Outcome of creating a user session: whether an existing session had to
/// be evicted to stay within `max_sessions`, and if so, which one (so the
/// caller can log it).
pub struct SessionCreated {
    pub evicted_session_id: Option<i64>,
}

/// Creates a new user session for `member_id`, evicting the oldest session
/// for that member first if they're already at `max_sessions`. Runs in one
/// `BEGIN IMMEDIATE` transaction so concurrent logins for the same member
/// can't both squeeze past the limit.
pub async fn create_user_session(
    pool: &SqlitePool,
    member_id: i64,
    max_sessions: i64,
    token_hash: &[u8],
    now: i64,
    expires_at: i64,
) -> Result<SessionCreated, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    let current: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM sessions WHERE member_id = ? AND kind = 'user'"#,
        member_id
    )
    .fetch_one(&mut *tx)
    .await?;

    let evicted_session_id = if current >= max_sessions {
        sqlx::query_scalar!(
            r#"DELETE FROM sessions
               WHERE id = (
                   SELECT id FROM sessions
                   WHERE member_id = ? AND kind = 'user'
                   ORDER BY created_at ASC, id ASC
                   LIMIT 1
               )
               RETURNING id AS "id!""#,
            member_id
        )
        .fetch_optional(&mut *tx)
        .await?
    } else {
        None
    };

    sqlx::query!(
        r#"INSERT INTO sessions (token_hash, kind, member_id, created_at, expires_at, last_seen_at)
           VALUES (?, 'user', ?, ?, ?, ?)"#,
        token_hash,
        member_id,
        now,
        expires_at,
        now
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(SessionCreated { evicted_session_id })
}

/// Creates a new admin session. Admin logins aren't capped by `max_sessions`
/// (that column only applies to members); each login is independent.
pub async fn create_admin_session(
    pool: &SqlitePool,
    token_hash: &[u8],
    now: i64,
    expires_at: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"INSERT INTO sessions (token_hash, kind, member_id, created_at, expires_at, last_seen_at)
           VALUES (?, 'admin', NULL, ?, ?, ?)"#,
        token_hash,
        now,
        expires_at,
        now
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Updates a session's (and, for user sessions, its member's) `last_seen_at`,
/// and optionally slides `expires_at` forward. Callers throttle this to at
/// most once per 10 minutes per session, per the spec's write budget.
pub async fn touch_session(
    pool: &SqlitePool,
    session_id: i64,
    member_id: Option<i64>,
    now: i64,
    new_expires_at: Option<i64>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    sqlx::query!(
        "UPDATE sessions SET last_seen_at = ?, expires_at = COALESCE(?, expires_at) WHERE id = ?",
        now,
        new_expires_at,
        session_id
    )
    .execute(&mut *tx)
    .await?;

    if let Some(member_id) = member_id {
        sqlx::query!(
            "UPDATE members SET last_seen_at = ? WHERE id = ?",
            now,
            member_id
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await
}

/// Deletes one session (logout, rotate or revoke).
pub async fn delete_session(pool: &SqlitePool, session_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query!("DELETE FROM sessions WHERE id = ?", session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Deletes every session past its `expires_at`. Returns how many were
/// removed, for the hourly cleanup task's log line.
pub async fn delete_expired_sessions(pool: &SqlitePool, now: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!("DELETE FROM sessions WHERE expires_at < ?", now)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------
// Search annotation
// ---------------------------------------------------------------------

pub struct RequestState {
    pub tmdb_id: i64,
    pub media_type: String,
    pub status: String,
}

/// Current status of every request matching any of `tmdb_ids`, in one
/// SELECT however many IDs there are. The IDs are bound as one JSON array
/// and expanded by `json_each`, so the statement text never changes.
/// Callers match `media_type` themselves.
pub async fn request_states(
    pool: &SqlitePool,
    tmdb_ids: &[u32],
) -> Result<Vec<RequestState>, sqlx::Error> {
    if tmdb_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids_json = serde_json::to_string(tmdb_ids).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    sqlx::query_as!(
        RequestState,
        r#"SELECT tmdb_id AS "tmdb_id!", media_type AS "media_type!", status AS "status!"
           FROM requests
           WHERE tmdb_id IN (SELECT value FROM json_each(?))"#,
        ids_json
    )
    .fetch_all(pool)
    .await
}

// ---------------------------------------------------------------------
// Requests and quotas
// ---------------------------------------------------------------------

/// How much of a member's request quota is used. Read before any TMDB
/// work for a cheap early 429, and again inside the write transaction so
/// concurrent submits cannot overshoot.
#[derive(Debug, Clone, Copy)]
pub struct QuotaUsage {
    /// `submitted` events since the start of the UTC day.
    pub today: i64,
    /// `submitted` and `duplicate` events in the last minute.
    pub last_minute: i64,
    /// Time of the oldest event in that minute, for `Retry-After`.
    pub oldest_in_minute: Option<i64>,
}

/// Counts quota use from `request_events`, so limits survive restarts.
/// The daily count follows `requests.requested_by` (exact); duplicate
/// events carry no member id, so the per-minute count matches on the
/// `actor` name the events were written with.
pub async fn quota_usage(
    executor: impl sqlx::SqliteExecutor<'_>,
    member_id: i64,
    member_name: &str,
    day_start: i64,
    minute_start: i64,
) -> Result<QuotaUsage, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT
             (SELECT COUNT(*) FROM request_events e JOIN requests r ON r.id = e.request_id
              WHERE e.kind = 'submitted' AND r.requested_by = ?1 AND e.created_at >= ?2)
               AS "today!: i64",
             (SELECT COUNT(*) FROM request_events
              WHERE kind IN ('submitted', 'duplicate') AND actor = ?3 AND created_at > ?4)
               AS "last_minute!: i64",
             (SELECT MIN(created_at) FROM request_events
              WHERE kind IN ('submitted', 'duplicate') AND actor = ?3 AND created_at > ?4)
               AS "oldest_in_minute: i64""#,
        member_id,
        day_start,
        member_name,
        minute_start
    )
    .fetch_one(executor)
    .await?;
    Ok(QuotaUsage {
        today: row.today,
        last_minute: row.last_minute,
        oldest_in_minute: row.oldest_in_minute,
    })
}

/// Everything needed to write a request row; title and friends come from
/// TMDB, never from the client.
pub struct NewRequest<'a> {
    pub tmdb_id: i64,
    pub media_type: &'a str,
    pub imdb_id: Option<&'a str>,
    pub title: &'a str,
    pub year: Option<i64>,
    pub poster_path: Option<&'a str>,
    pub note: Option<&'a str>,
    pub member_id: i64,
    pub member_name: &'a str,
    pub now: i64,
}

#[derive(Debug, Clone)]
pub struct RequestRow {
    pub id: i64,
    pub tmdb_id: i64,
    pub media_type: String,
    pub title: String,
    pub year: Option<i64>,
    pub poster_path: Option<String>,
    pub status: String,
    pub created_at: i64,
}

pub enum CreateOutcome {
    Created(RequestRow),
    Duplicate(RequestRow),
    /// The quota re-check inside the transaction failed; nothing written.
    OverQuota(QuotaUsage),
}

/// Inserts the request, or logs a `duplicate` event if the title is
/// already queued, in one `BEGIN IMMEDIATE` transaction. `within_quota`
/// re-checks the quota under the write lock; if it says no, the
/// transaction is rolled back and nothing is written.
pub async fn create_request(
    pool: &SqlitePool,
    new: &NewRequest<'_>,
    day_start: i64,
    minute_start: i64,
    within_quota: impl FnOnce(&QuotaUsage) -> bool,
) -> Result<CreateOutcome, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;

    let usage = quota_usage(
        &mut *tx,
        new.member_id,
        new.member_name,
        day_start,
        minute_start,
    )
    .await?;
    if !within_quota(&usage) {
        tx.rollback().await?;
        return Ok(CreateOutcome::OverQuota(usage));
    }

    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO requests (tmdb_id, media_type, imdb_id, title, year, poster_path, note,
                                 status, requested_by, created_at, status_changed_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?)
           ON CONFLICT (tmdb_id, media_type) DO NOTHING
           RETURNING id AS "id!""#,
        new.tmdb_id,
        new.media_type,
        new.imdb_id,
        new.title,
        new.year,
        new.poster_path,
        new.note,
        new.member_id,
        new.now,
        new.now
    )
    .fetch_optional(&mut *tx)
    .await?;
    let kind = if inserted.is_some() {
        "submitted"
    } else {
        "duplicate"
    };

    let row = sqlx::query_as!(
        RequestRow,
        r#"SELECT id AS "id!", tmdb_id AS "tmdb_id!", media_type AS "media_type!",
                  title AS "title!", year, poster_path, status AS "status!",
                  created_at AS "created_at!"
           FROM requests WHERE tmdb_id = ? AND media_type = ?"#,
        new.tmdb_id,
        new.media_type
    )
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query!(
        "INSERT INTO request_events (request_id, kind, actor, note, created_at)
         VALUES (?, ?, ?, ?, ?)",
        row.id,
        kind,
        new.member_name,
        new.note,
        new.now
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(if inserted.is_some() {
        CreateOutcome::Created(row)
    } else {
        CreateOutcome::Duplicate(row)
    })
}

/// A member's requests, newest first: titles they asked for first, plus
/// titles they asked for again after someone else (matched by the
/// `duplicate` event's actor name). Keyset pagination on
/// `(created_at, id)`: pass the last row's values to get the next page.
pub async fn requests_for_member(
    pool: &SqlitePool,
    member_id: i64,
    member_name: &str,
    before: (i64, i64),
    limit: i64,
) -> Result<Vec<RequestRow>, sqlx::Error> {
    let (before_created, before_id) = before;
    sqlx::query_as!(
        RequestRow,
        r#"SELECT r.id AS "id!", r.tmdb_id AS "tmdb_id!", r.media_type AS "media_type!",
                  r.title AS "title!", r.year, r.poster_path, r.status AS "status!",
                  r.created_at AS "created_at!"
           FROM requests r
           WHERE (r.requested_by = ?1
                  OR EXISTS (SELECT 1 FROM request_events e
                             WHERE e.request_id = r.id AND e.kind = 'duplicate'
                               AND e.actor = ?2))
             AND (r.created_at < ?3 OR (r.created_at = ?3 AND r.id < ?4))
           ORDER BY r.created_at DESC, r.id DESC
           LIMIT ?5"#,
        member_id,
        member_name,
        before_created,
        before_id,
        limit
    )
    .fetch_all(pool)
    .await
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

    #[tokio::test]
    async fn delete_expired_sessions_removes_only_expired_rows() {
        let pool = test_pool().await;
        let member_id = insert_member(&pool, "Alice", b"hash", 3, None, 0)
            .await
            .expect("insert member");

        sqlx::query!(
            "INSERT INTO sessions (token_hash, kind, member_id, created_at, expires_at, last_seen_at)
             VALUES (x'01', 'user', ?, 0, 100, 0)",
            member_id
        )
        .execute(&pool)
        .await
        .expect("insert expired session");
        sqlx::query!(
            "INSERT INTO sessions (token_hash, kind, member_id, created_at, expires_at, last_seen_at)
             VALUES (x'02', 'user', ?, 0, 999999, 0)",
            member_id
        )
        .execute(&pool)
        .await
        .expect("insert live session");

        let removed = delete_expired_sessions(&pool, 500).await.expect("cleanup");
        assert_eq!(removed, 1);

        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(remaining, 1);
    }
}
