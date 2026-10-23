//! SQLite pool setup, pragmas and the single place migrations are run from.
//! All SQL in the app lives in this module; handlers call functions here,
//! never `sqlx::query!` directly.

use std::time::Duration;

use sqlx::{
    Sqlite, SqliteConnection, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::config::{Config, WorkerMode};

const MAX_CONNECTIONS: u32 = 4;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Opens the pool with the pragmas the spec requires and runs pending
/// migrations. Creates the database file's parent directory if needed,
/// since SQLite will create the file but not the directory.
pub async fn connect(cfg: &Config) -> Result<SqlitePool, sqlx::Error> {
    if let Some(parent) = cfg.database_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            sqlx::Error::Configuration(
                format!("creating database directory {parent:?}: {e}").into(),
            )
        })?;
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

/// Writes a consistent snapshot of the whole database to `path` with
/// `VACUUM INTO`, which works while the app keeps serving. Fails if `path`
/// already exists.
pub async fn vacuum_into(pool: &SqlitePool, path: &str) -> Result<(), sqlx::Error> {
    sqlx::query("VACUUM INTO ?")
        .bind(path)
        .execute(pool)
        .await?;
    Ok(())
}

/// `20231114T221320Z` for a unix time, for sortable backup file names.
pub async fn compact_utc(pool: &SqlitePool, unix: i64) -> Result<String, sqlx::Error> {
    let formatted = sqlx::query_scalar!(
        r#"SELECT strftime('%Y%m%dT%H%M%SZ', ?, 'unixepoch') AS "formatted!: String""#,
        unix
    )
    .fetch_one(pool)
    .await?;
    Ok(formatted)
}

/// Current unix time in seconds. The one place request handlers get "now"
/// from; `tmdb.rs` and the rate limiters use the injectable `Clock` instead.
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
    pub code_hash: Vec<u8>,
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
        r#"SELECT id AS "id!", name AS "name!", code_hash AS "code_hash!",
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
    daily_limit: Option<i64>,
    now: i64,
) -> Result<i64, sqlx::Error> {
    let rec = sqlx::query!(
        r#"INSERT INTO members (name, code_hash, daily_limit, created_at)
           VALUES (?, ?, ?, ?)
           RETURNING id AS "id!""#,
        name,
        code_hash,
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

/// Whether a session still exists and has not run out. `idle_secs` is the
/// idle timeout for sessions that have one (admin). Read-only, so a stream
/// holding a session open never extends it.
pub async fn session_alive(
    pool: &SqlitePool,
    session_id: i64,
    now: i64,
    idle_secs: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT expires_at AS "expires_at!", last_seen_at AS "last_seen_at!"
           FROM sessions WHERE id = ?"#,
        session_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some_and(|r| {
        now < r.expires_at && idle_secs.is_none_or(|idle| now - r.last_seen_at < idle)
    }))
}

/// Requests per status, for the admin tab badges. Statuses with no
/// requests are absent.
pub async fn status_counts(pool: &SqlitePool) -> Result<Vec<(String, i64)>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT status AS "status!", COUNT(*) AS "n!: i64" FROM requests GROUP BY status"#
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.status, r.n)).collect())
}

/// Jobs waiting for the admin's pick, for the Jobs tab badge. Such a job
/// always has a pending request: a request leaving `pending` closes its
/// open jobs.
pub async fn needs_pick_job_count(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM fetch_jobs WHERE state = 'needs_pick'"#
    )
    .fetch_one(pool)
    .await
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
    pub infuse_pro: bool,
}

/// Current status of every request matching any of `tmdb_ids`, in one
/// SELECT however many IDs there are. The IDs are bound as one JSON array
/// and expanded by `json_each`, so the statement text never changes.
/// Callers match `media_type` and `infuse_pro` themselves: a title can have
/// one request per variant.
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
        r#"SELECT tmdb_id AS "tmdb_id!", media_type AS "media_type!", status AS "status!",
                  infuse_pro AS "infuse_pro!: bool"
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
    pub infuse_pro: bool,
    pub member_id: i64,
    pub member_name: &'a str,
    pub now: i64,
}

#[derive(Debug, Clone)]
pub struct RequestRow {
    pub id: i64,
    pub tmdb_id: i64,
    pub media_type: String,
    pub imdb_id: Option<String>,
    pub title: String,
    pub year: Option<i64>,
    pub poster_path: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub infuse_pro: bool,
}

pub enum CreateOutcome {
    Created(RequestRow),
    Duplicate(RequestRow),
    /// The quota re-check inside the transaction failed; nothing written.
    OverQuota(QuotaUsage),
}

/// Inserts the request, or logs a `duplicate` event if the title is
/// already queued for the same Infuse PRO variant, in one `BEGIN IMMEDIATE` transaction. `within_quota`
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
                                 infuse_pro, status, requested_by, created_at, status_changed_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?)
           ON CONFLICT (tmdb_id, media_type, infuse_pro) DO NOTHING
           RETURNING id AS "id!""#,
        new.tmdb_id,
        new.media_type,
        new.imdb_id,
        new.title,
        new.year,
        new.poster_path,
        new.note,
        new.infuse_pro,
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
                  imdb_id, title AS "title!", year, poster_path, status AS "status!",
                  created_at AS "created_at!", infuse_pro AS "infuse_pro!: bool"
           FROM requests WHERE tmdb_id = ? AND media_type = ? AND infuse_pro = ?"#,
        new.tmdb_id,
        new.media_type,
        new.infuse_pro
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

/// A row of a member's own request list.
pub struct MineRow {
    pub id: i64,
    pub tmdb_id: i64,
    pub media_type: String,
    pub imdb_id: Option<String>,
    pub title: String,
    pub year: Option<i64>,
    pub poster_path: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub infuse_pro: bool,
    /// Why the admin refused it, if they said.
    pub reject_reason: Option<String>,
    pub can_cancel: bool,
}

pub enum CancelOutcome {
    Cancelled,
    /// Not theirs, or no such request.
    NotFound,
    /// Already handled by the admin, or someone else asked for it too.
    Locked,
}

/// Removes a request its original requester made, while it is still
/// pending and nobody else has asked for the same title.
pub async fn cancel_request(
    pool: &SqlitePool,
    request_id: i64,
    member_id: i64,
) -> Result<CancelOutcome, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    let Some(row) = sqlx::query!(
        "SELECT status FROM requests WHERE id = ? AND requested_by = ?",
        request_id,
        member_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(CancelOutcome::NotFound);
    };
    let duplicated: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM request_events
           WHERE request_id = ? AND kind = 'duplicate'"#,
        request_id
    )
    .fetch_one(&mut *tx)
    .await?;
    if row.status != "pending" || duplicated > 0 {
        tx.rollback().await?;
        return Ok(CancelOutcome::Locked);
    }
    sqlx::query!(
        "DELETE FROM request_events WHERE request_id = ?",
        request_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM requests WHERE id = ?", request_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(CancelOutcome::Cancelled)
}

/// Removes a request and its whole history, whatever its status. Returns
/// false if there was no such request. For the admin; members use
/// [`cancel_request`].
pub async fn delete_request(pool: &SqlitePool, request_id: i64) -> Result<bool, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    sqlx::query!(
        "DELETE FROM request_events WHERE request_id = ?",
        request_id
    )
    .execute(&mut *tx)
    .await?;
    let removed = sqlx::query!("DELETE FROM requests WHERE id = ?", request_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(removed > 0)
}

/// Which of a member's requests to list.
#[derive(Clone, Copy)]
pub enum MineFilter {
    All,
    Pending,
    /// Everything that is no longer pending.
    Earlier,
}

/// A member's requests, newest first: titles they asked for first, plus
/// titles they asked for again after someone else (matched by the
/// `duplicate` event's actor name). Keyset pagination on
/// `(created_at, id)`: pass the last row's values to get the next page.
pub async fn requests_for_member(
    pool: &SqlitePool,
    member_id: i64,
    member_name: &str,
    filter: MineFilter,
    before: (i64, i64),
    limit: i64,
) -> Result<Vec<MineRow>, sqlx::Error> {
    let (before_created, before_id) = before;
    let filter = match filter {
        MineFilter::All => 0_i64,
        MineFilter::Pending => 1,
        MineFilter::Earlier => 2,
    };
    sqlx::query_as!(
        MineRow,
        r#"SELECT r.id AS "id!", r.tmdb_id AS "tmdb_id!", r.media_type AS "media_type!",
                  r.imdb_id, r.title AS "title!", r.year, r.poster_path, r.status AS "status!",
                  r.created_at AS "created_at!", r.infuse_pro AS "infuse_pro!: bool",
                  r.reject_reason,
                  (r.requested_by = ?1 AND r.status = 'pending'
                   AND NOT EXISTS (SELECT 1 FROM request_events d
                                   WHERE d.request_id = r.id AND d.kind = 'duplicate'))
                    AS "can_cancel!: bool"
           FROM requests r
           WHERE (r.requested_by = ?1
                  OR EXISTS (SELECT 1 FROM request_events e
                             WHERE e.request_id = r.id AND e.kind = 'duplicate'
                               AND e.actor = ?2))
             AND (r.created_at < ?3 OR (r.created_at = ?3 AND r.id < ?4))
             AND (?6 = 0 OR (?6 = 1 AND r.status = 'pending')
                  OR (?6 = 2 AND r.status <> 'pending'))
           ORDER BY r.created_at DESC, r.id DESC
           LIMIT ?5"#,
        member_id,
        member_name,
        before_created,
        before_id,
        limit,
        filter
    )
    .fetch_all(pool)
    .await
}

// ---------------------------------------------------------------------
// Admin: requests
// ---------------------------------------------------------------------

pub struct AdminRequestRow {
    pub id: i64,
    pub tmdb_id: i64,
    pub media_type: String,
    pub imdb_id: Option<String>,
    pub title: String,
    pub year: Option<i64>,
    pub poster_path: Option<String>,
    pub note: Option<String>,
    pub infuse_pro: bool,
    pub status: String,
    pub reject_reason: Option<String>,
    pub requester: String,
    pub created_at: i64,
    pub status_changed_at: i64,
    /// `submitted` plus `duplicate` events: how many times people asked.
    pub times_requested: i64,
}

/// One page of the admin queue, newest first, keyset-paginated on
/// `(created_at, id)` so rows arriving between page loads never shift
/// later pages. `status: None` means every status. `title_like` is a
/// ready-escaped LIKE pattern (backslash escapes) matched against title
/// and requester; `exact` is matched against the IMDb ID.
pub async fn admin_requests(
    pool: &SqlitePool,
    status: Option<&str>,
    title_like: Option<&str>,
    exact: Option<&str>,
    before: (i64, i64),
    limit: i64,
) -> Result<Vec<AdminRequestRow>, sqlx::Error> {
    let (before_created, before_id) = before;
    sqlx::query_as!(
        AdminRequestRow,
        r#"SELECT r.id AS "id!", r.tmdb_id AS "tmdb_id!", r.media_type AS "media_type!",
                  r.imdb_id, r.title AS "title!", r.year, r.poster_path, r.note,
                  r.infuse_pro AS "infuse_pro!: bool",
                  r.status AS "status!", r.reject_reason,
                  COALESCE(m.name, r.requested_by_name, '(deleted)') AS "requester!",
                  r.created_at AS "created_at!", r.status_changed_at AS "status_changed_at!",
                  (SELECT COUNT(*) FROM request_events e
                   WHERE e.request_id = r.id AND e.kind IN ('submitted', 'duplicate'))
                    AS "times_requested!: i64"
           FROM requests r
           LEFT JOIN members m ON m.id = r.requested_by
           WHERE (?1 IS NULL OR r.status = ?1)
             AND (?2 IS NULL OR r.title LIKE ?2 ESCAPE '\'
                  OR COALESCE(m.name, r.requested_by_name) LIKE ?2 ESCAPE '\'
                  OR r.imdb_id = ?3)
             AND (r.created_at < ?4 OR (r.created_at = ?4 AND r.id < ?5))
           ORDER BY r.created_at DESC, r.id DESC
           LIMIT ?6"#,
        status,
        title_like,
        exact,
        before_created,
        before_id,
        limit
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug)]
pub enum StatusChange {
    NotFound,
    /// Already in that status; nothing written.
    Unchanged,
    Changed {
        from: String,
    },
}

/// Moves a request to `to` and logs a `status` event in the same
/// transaction, so the archive always explains the current status.
/// `reason` is why the admin refused it: kept (and logged as the event's
/// note) only when `to` is `refused`, and cleared when it leaves
/// `refused`. Setting `refused` again with a different reason (or none,
/// which clears it) replaces it and logs one more event; with the same
/// reason nothing is written.
pub async fn set_request_status(
    pool: &SqlitePool,
    request_id: i64,
    to: &str,
    reason: Option<&str>,
    now: i64,
) -> Result<StatusChange, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    let change = set_request_status_in(&mut tx, request_id, to, reason, "admin", now).await?;
    match change {
        StatusChange::Changed { .. } => tx.commit().await?,
        StatusChange::NotFound | StatusChange::Unchanged => tx.rollback().await?,
    }
    Ok(change)
}

/// The body of [`set_request_status`], for callers that already hold the
/// write transaction (the worker's `done` report). `actor` is who the
/// `status` event is logged as. A request that leaves `pending` also
/// closes its open fetch job, so a worker still running it finds out at
/// its next call and cleans up instead of leaving a torrent behind.
async fn set_request_status_in(
    conn: &mut SqliteConnection,
    request_id: i64,
    to: &str,
    reason: Option<&str>,
    actor: &str,
    now: i64,
) -> Result<StatusChange, sqlx::Error> {
    let current = sqlx::query!(
        r#"SELECT status AS "status!", reject_reason FROM requests WHERE id = ?"#,
        request_id
    )
    .fetch_optional(&mut *conn)
    .await?;
    let Some(current) = current else {
        return Ok(StatusChange::NotFound);
    };
    let from = current.status;
    let reason = reason.filter(|_| to == "refused");
    if from == to && (to != "refused" || reason == current.reject_reason.as_deref()) {
        return Ok(StatusChange::Unchanged);
    }
    sqlx::query!(
        "UPDATE requests SET status = ?, reject_reason = ?, status_changed_at = ? WHERE id = ?",
        to,
        reason,
        now,
        request_id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "INSERT INTO request_events (request_id, kind, actor, from_status, to_status, note, created_at)
         VALUES (?, 'status', ?, ?, ?, ?, ?)",
        request_id,
        actor,
        from,
        to,
        reason,
        now
    )
    .execute(&mut *conn)
    .await?;
    if to != "pending" {
        sqlx::query!(
            "UPDATE fetch_jobs
             SET state = 'done', reason = 'request_changed', claim_id = NULL, updated_at = ?
             WHERE request_id = ? AND state IN ('queued', 'needs_pick', 'fetching')",
            now,
            request_id
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(StatusChange::Changed { from })
}

pub struct EventRow {
    pub id: i64,
    pub kind: String,
    pub actor: String,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub note: Option<String>,
    pub created_at: i64,
}

/// Full history of one request, oldest first; `None` if no such request.
pub async fn request_history(
    pool: &SqlitePool,
    request_id: i64,
) -> Result<Option<Vec<EventRow>>, sqlx::Error> {
    let exists = sqlx::query_scalar!(
        r#"SELECT id AS "id!" FROM requests WHERE id = ?"#,
        request_id
    )
    .fetch_optional(pool)
    .await?;
    if exists.is_none() {
        return Ok(None);
    }
    let events = sqlx::query_as!(
        EventRow,
        r#"SELECT id AS "id!", kind AS "kind!", actor AS "actor!", from_status, to_status,
                  note, created_at AS "created_at!"
           FROM request_events WHERE request_id = ? ORDER BY id"#,
        request_id
    )
    .fetch_all(pool)
    .await?;
    Ok(Some(events))
}

/// One CSV line of the export. All cells are text; the first row the
/// query yields is the header.
pub struct ExportRow {
    pub id: String,
    pub tmdb_id: String,
    pub media_type: String,
    pub title: String,
    pub year: Option<String>,
    pub imdb_id: Option<String>,
    pub status: String,
    pub requested_by: String,
    pub note: Option<String>,
    pub times_requested: String,
    pub created_at: String,
    pub status_changed_at: String,
    pub infuse_pro: String,
    pub reject_reason: Option<String>,
}

/// Neutralises spreadsheet formulas: a cell starting with `=`, `+`, `-`
/// or `@` (or a tab or carriage return, which some spreadsheets strip
/// first) gets a leading `'` so it is shown as text, never evaluated.
pub fn csv_safe(cell: &str) -> std::borrow::Cow<'_, str> {
    match cell.chars().next() {
        Some('=' | '+' | '-' | '@' | '\t' | '\r') => format!("'{cell}").into(),
        _ => cell.into(),
    }
}

/// Lets the database row stream feed the response body directly, one
/// line at a time, so the export never holds the whole archive in memory.
impl From<ExportRow> for axum::body::Bytes {
    fn from(row: ExportRow) -> Self {
        let cells = [
            row.id.as_str(),
            &row.tmdb_id,
            &row.media_type,
            &row.title,
            row.year.as_deref().unwrap_or(""),
            row.imdb_id.as_deref().unwrap_or(""),
            &row.status,
            &row.requested_by,
            row.note.as_deref().unwrap_or(""),
            &row.times_requested,
            &row.created_at,
            &row.status_changed_at,
            &row.infuse_pro,
            row.reject_reason.as_deref().unwrap_or(""),
        ];
        let mut writer = csv::Writer::from_writer(Vec::with_capacity(256));
        // Writing to a Vec cannot fail; an error would only drop the line.
        let _ = writer.write_record(cells.iter().map(|c| csv_safe(c).into_owned()));
        writer.into_inner().unwrap_or_default().into()
    }
}

/// The whole archive as a streamed CSV body: a header line, then one line
/// per request, oldest first. Needs a `'static` pool handle because the
/// row stream outlives the handler that starts it.
pub fn export_csv(pool: &'static SqlitePool) -> axum::body::Body {
    let rows = sqlx::query_as!(
        ExportRow,
        r#"SELECT id AS "id!: String", tmdb_id AS "tmdb_id!: String",
                  media_type AS "media_type!: String", title AS "title!: String",
                  year AS "year?: String", imdb_id AS "imdb_id?: String",
                  status AS "status!: String", requested_by AS "requested_by!: String",
                  note AS "note?: String", times_requested AS "times_requested!: String",
                  created_at AS "created_at!: String",
                  status_changed_at AS "status_changed_at!: String",
                  infuse_pro AS "infuse_pro!: String",
                  reject_reason AS "reject_reason?: String"
           FROM (
             SELECT 0 AS sort_key, 0 AS sort_id,
                    'id' AS id, 'tmdb_id' AS tmdb_id, 'type' AS media_type, 'title' AS title,
                    'year' AS year, 'imdb_id' AS imdb_id, 'status' AS status,
                    'requested_by' AS requested_by, 'note' AS note,
                    'times_requested' AS times_requested, 'created_at' AS created_at,
                    'status_changed_at' AS status_changed_at, 'infuse_pro' AS infuse_pro,
                    'reject_reason' AS reject_reason
             UNION ALL
             SELECT 1, r.id, CAST(r.id AS TEXT), CAST(r.tmdb_id AS TEXT), r.media_type, r.title,
                    CAST(r.year AS TEXT), r.imdb_id, r.status,
                    COALESCE(m.name, r.requested_by_name, '(deleted)'), r.note,
                    CAST((SELECT COUNT(*) FROM request_events e
                          WHERE e.request_id = r.id AND e.kind IN ('submitted', 'duplicate'))
                         AS TEXT),
                    strftime('%Y-%m-%dT%H:%M:%SZ', r.created_at, 'unixepoch'),
                    strftime('%Y-%m-%dT%H:%M:%SZ', r.status_changed_at, 'unixepoch'),
                    CAST(r.infuse_pro AS TEXT), r.reject_reason
             FROM requests r LEFT JOIN members m ON m.id = r.requested_by
           )
           ORDER BY sort_key, sort_id"#
    )
    .fetch(pool);
    axum::body::Body::from_stream(rows)
}

// ---------------------------------------------------------------------
// Admin: members and devices
// ---------------------------------------------------------------------

pub struct MemberRow {
    pub id: i64,
    pub name: String,
    pub daily_limit: Option<i64>,
    pub created_at: i64,
    pub rotated_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub last_seen_at: Option<i64>,
    pub request_count: i64,
}

/// Every member, newest first. Never selects `code_hash`.
pub async fn list_members(pool: &SqlitePool) -> Result<Vec<MemberRow>, sqlx::Error> {
    sqlx::query_as!(
        MemberRow,
        r#"SELECT m.id AS "id!", m.name AS "name!",
                  m.daily_limit, m.created_at AS "created_at!", m.rotated_at, m.revoked_at,
                  m.last_seen_at,
                  (SELECT COUNT(*) FROM requests r WHERE r.requested_by = m.id)
                    AS "request_count!: i64"
           FROM members m ORDER BY m.id DESC"#
    )
    .fetch_all(pool)
    .await
}

pub struct DeviceRow {
    pub id: i64,
    pub member_id: i64,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
}

/// Live user sessions ("devices") of every member, oldest first.
pub async fn list_devices(pool: &SqlitePool, now: i64) -> Result<Vec<DeviceRow>, sqlx::Error> {
    sqlx::query_as!(
        DeviceRow,
        r#"SELECT id AS "id!", member_id AS "member_id!", created_at AS "created_at!",
                  last_seen_at AS "last_seen_at!", expires_at AS "expires_at!"
           FROM sessions
           WHERE kind = 'user' AND expires_at > ?
           ORDER BY created_at, id"#,
        now
    )
    .fetch_all(pool)
    .await
}

/// Suffix given to a deleted member's name on the events they authored.
/// `name_taken` refuses names with it, so a new member cannot collide
/// with a deleted one and inherit its history.
pub const DELETED_SUFFIX: &str = " (deleted)";

/// Whether another member already uses `name` (case-insensitively), or the
/// name is reserved for deleted members. Names double as event actors, so
/// they are kept unique.
async fn name_taken(
    executor: impl sqlx::SqliteExecutor<'_>,
    name: &str,
    except_id: i64,
) -> Result<bool, sqlx::Error> {
    if name.to_lowercase().ends_with(DELETED_SUFFIX) {
        return Ok(true);
    }
    let found = sqlx::query_scalar!(
        r#"SELECT id AS "id!" FROM members WHERE lower(name) = lower(?) AND id != ? LIMIT 1"#,
        name,
        except_id
    )
    .fetch_optional(executor)
    .await?;
    Ok(found.is_some())
}

pub enum MemberWrite {
    Done(i64),
    NotFound,
    NameTaken,
    Revoked,
}

/// Creates a member unless the name is taken. A `code_hash` collision
/// (two 40-bit codes alike) surfaces as a unique-constraint error for the
/// caller to retry with a new code.
pub async fn create_member(
    pool: &SqlitePool,
    name: &str,
    code_hash: &[u8],
    daily_limit: Option<i64>,
    now: i64,
) -> Result<MemberWrite, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    if name_taken(&mut *tx, name, 0).await? {
        tx.rollback().await?;
        return Ok(MemberWrite::NameTaken);
    }
    let id = sqlx::query_scalar!(
        r#"INSERT INTO members (name, code_hash, daily_limit, created_at)
           VALUES (?, ?, ?, ?)
           RETURNING id AS "id!""#,
        name,
        code_hash,
        daily_limit,
        now
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(MemberWrite::Done(id))
}

/// Fields a member PATCH may change; `None` leaves a field alone, and
/// `daily_limit: Some(None)` removes the member's daily limit.
pub struct MemberUpdate<'a> {
    pub name: Option<&'a str>,
    pub daily_limit: Option<Option<i64>>,
}

pub async fn update_member(
    pool: &SqlitePool,
    member_id: i64,
    update: &MemberUpdate<'_>,
) -> Result<MemberWrite, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    if let Some(name) = update.name
        && name_taken(&mut *tx, name, member_id).await?
    {
        tx.rollback().await?;
        return Ok(MemberWrite::NameTaken);
    }
    let set_limit = update.daily_limit.is_some();
    let new_limit = update.daily_limit.flatten();
    let old_name = sqlx::query_scalar!("SELECT name FROM members WHERE id = ?", member_id)
        .fetch_optional(&mut *tx)
        .await?;
    let result = sqlx::query!(
        "UPDATE members
         SET name = COALESCE(?, name),
             daily_limit = CASE WHEN ? THEN ? ELSE daily_limit END
         WHERE id = ?",
        update.name,
        set_limit,
        new_limit,
        member_id
    )
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(MemberWrite::NotFound);
    }
    // Events carry the name as text, so a rename must follow them or the
    // member's earlier duplicate requests would drop out of their list. Only
    // the events a member authors: `status` events are logged as `admin` or
    // `worker`, which a member may also be called.
    if let (Some(old), Some(new)) = (old_name, update.name)
        && old != new
    {
        sqlx::query!(
            "UPDATE request_events SET actor = ? WHERE actor = ? AND kind IN ('submitted', 'duplicate')",
            new,
            old
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(MemberWrite::Done(member_id))
}

/// Replaces a member's code and deletes all their sessions, so the old
/// code and every device logged in with it stop working at once.
pub async fn rotate_member_code(
    pool: &SqlitePool,
    member_id: i64,
    code_hash: &[u8],
    now: i64,
) -> Result<MemberWrite, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    let revoked = sqlx::query_scalar!("SELECT revoked_at FROM members WHERE id = ?", member_id)
        .fetch_optional(&mut *tx)
        .await?;
    match revoked {
        None => {
            tx.rollback().await?;
            return Ok(MemberWrite::NotFound);
        }
        Some(Some(_)) => {
            tx.rollback().await?;
            return Ok(MemberWrite::Revoked);
        }
        Some(None) => {}
    }
    sqlx::query!(
        "UPDATE members SET code_hash = ?, rotated_at = ? WHERE id = ?",
        code_hash,
        now,
        member_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM sessions WHERE member_id = ?", member_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(MemberWrite::Done(member_id))
}

/// Marks a member revoked (keeping the first revocation time) and deletes
/// all their sessions.
pub async fn revoke_member(
    pool: &SqlitePool,
    member_id: i64,
    now: i64,
) -> Result<MemberWrite, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    let result = sqlx::query!(
        "UPDATE members SET revoked_at = COALESCE(revoked_at, ?) WHERE id = ?",
        now,
        member_id
    )
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(MemberWrite::NotFound);
    }
    sqlx::query!("DELETE FROM sessions WHERE member_id = ?", member_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(MemberWrite::Done(member_id))
}

/// Lifts a revoke. The member's old code works again; their devices were
/// deleted by the revoke, so they log in afresh.
pub async fn restore_member(pool: &SqlitePool, member_id: i64) -> Result<MemberWrite, sqlx::Error> {
    let result = sqlx::query!(
        "UPDATE members SET revoked_at = NULL WHERE id = ?",
        member_id
    )
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Ok(MemberWrite::NotFound);
    }
    Ok(MemberWrite::Done(member_id))
}

pub enum MemberDelete {
    Deleted,
    NotFound,
    /// Only a revoked member can be deleted, so a stray click cannot
    /// remove someone who is still active.
    NotRevoked,
}

/// Deletes a revoked member. Their requests stay, carrying the name as
/// text; their events are re-labelled `<name> (deleted)`; their sessions
/// go with them. All in one transaction.
pub async fn delete_member(pool: &SqlitePool, member_id: i64) -> Result<MemberDelete, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    let Some(member) = sqlx::query!(
        "SELECT name, revoked_at FROM members WHERE id = ?",
        member_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(MemberDelete::NotFound);
    };
    if member.revoked_at.is_none() {
        tx.rollback().await?;
        return Ok(MemberDelete::NotRevoked);
    }
    sqlx::query!(
        "UPDATE requests SET requested_by_name = ?, requested_by = NULL WHERE requested_by = ?",
        member.name,
        member_id
    )
    .execute(&mut *tx)
    .await?;
    let tombstone = format!("{}{DELETED_SUFFIX}", member.name);
    sqlx::query!(
        "UPDATE request_events SET actor = ? WHERE actor = ? AND kind IN ('submitted', 'duplicate')",
        tombstone,
        member.name
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM sessions WHERE member_id = ?", member_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM members WHERE id = ?", member_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(MemberDelete::Deleted)
}

/// Kills one member device. Admin sessions are not touched here.
pub async fn delete_user_session(pool: &SqlitePool, session_id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        "DELETE FROM sessions WHERE id = ? AND kind = 'user'",
        session_id
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

// ---------------------------------------------------------------------
// Fetch jobs (the worker)
// ---------------------------------------------------------------------

/// A `fetching` job with no report or progress call for this long is taken
/// back by the next claim. Longer than the worker's own job deadline.
pub const RECLAIM_AFTER_SECS: i64 = 45 * 60;

/// Most `uncached_adds` one progress or report call may add.
const MAX_UNCACHED_INCREMENT: i64 = 5;

pub enum CreateJob {
    Created {
        id: i64,
        /// `queued`, or `needs_pick` when the request has no IMDb ID.
        state: String,
        /// `no_imdb_id` for a job that starts in `needs_pick`.
        reason: Option<String>,
    },
    /// The request already has a job.
    Exists,
    NotPending,
    /// Only movies can be fetched; series are not supported yet.
    Unsupported,
    NotFound,
}

/// Creates the job for a pending request. A request without an IMDb ID
/// goes straight to `needs_pick`, because the worker could never search
/// for it.
pub async fn create_fetch_job(
    pool: &SqlitePool,
    request_id: i64,
    now: i64,
) -> Result<CreateJob, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    let Some(request) = sqlx::query!(
        r#"SELECT status AS "status!", media_type AS "media_type!",
                  infuse_pro AS "infuse_pro!: bool", imdb_id
           FROM requests WHERE id = ?"#,
        request_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(CreateJob::NotFound);
    };
    if request.status != "pending" {
        tx.rollback().await?;
        return Ok(CreateJob::NotPending);
    }
    if request.media_type != "movie" {
        tx.rollback().await?;
        return Ok(CreateJob::Unsupported);
    }
    let existing = sqlx::query_scalar!(
        r#"SELECT id AS "id!" FROM fetch_jobs WHERE request_id = ?"#,
        request_id
    )
    .fetch_optional(&mut *tx)
    .await?;
    if existing.is_some() {
        tx.rollback().await?;
        return Ok(CreateJob::Exists);
    }
    let (state, reason) = if request.imdb_id.is_some() {
        ("queued", None)
    } else {
        ("needs_pick", Some("no_imdb_id"))
    };
    let id = sqlx::query!(
        "INSERT INTO fetch_jobs (request_id, state, infuse_pro, reason, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        request_id,
        state,
        request.infuse_pro,
        reason,
        now,
        now
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    tx.commit().await?;
    Ok(CreateJob::Created {
        id,
        state: state.to_string(),
        reason: reason.map(str::to_string),
    })
}

pub struct FetchJobRow {
    pub id: i64,
    pub request_id: i64,
    pub state: String,
    pub reason: Option<String>,
    pub infuse_pro: bool,
    pub attempts: i64,
    pub top_infohash: Option<String>,
    pub chosen_infohash: Option<String>,
    pub picked: bool,
    /// The stored JSON text; the worker supplied it, so the caller parses
    /// it defensively.
    pub candidates: Option<String>,
    pub rd_torrent_id: Option<String>,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub title: String,
    pub year: Option<i64>,
    pub media_type: String,
    pub request_status: String,
    /// The request's blocked infohashes, comma-joined; `None` when there are
    /// none.
    pub blocked_hashes: Option<String>,
}

/// One page of jobs with their request's title, newest first, keyset-
/// paginated on `(created_at, id)` like [`admin_requests`]. `state: None`
/// means every state.
pub async fn list_fetch_jobs(
    pool: &SqlitePool,
    state: Option<&str>,
    before: (i64, i64),
    limit: i64,
) -> Result<Vec<FetchJobRow>, sqlx::Error> {
    let (before_created, before_id) = before;
    sqlx::query_as!(
        FetchJobRow,
        r#"SELECT j.id AS "id!", j.request_id AS "request_id!", j.state AS "state!", j.reason,
                  j.infuse_pro AS "infuse_pro!: bool", j.attempts AS "attempts!",
                  j.top_infohash, j.chosen_infohash, j.picked AS "picked!: bool",
                  j.candidates, j.rd_torrent_id, j.last_error,
                  j.created_at AS "created_at!", j.updated_at AS "updated_at!",
                  r.title AS "title!", r.year, r.media_type AS "media_type!",
                  r.status AS "request_status!",
                  (SELECT group_concat(b.infohash) FROM blocked_hashes b
                    WHERE b.request_id = j.request_id) AS blocked_hashes
           FROM fetch_jobs j JOIN requests r ON r.id = j.request_id
           WHERE (?1 IS NULL OR j.state = ?1)
             AND (j.created_at < ?2 OR (j.created_at = ?2 AND j.id < ?3))
           ORDER BY j.created_at DESC, j.id DESC
           LIMIT ?4"#,
        state,
        before_created,
        before_id,
        limit
    )
    .fetch_all(pool)
    .await
}

/// What the worker is told about a job. Deliberately not the member's note,
/// name or id: the worker sees validated IDs and titles only.
#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub id: i64,
    pub claim_id: String,
    pub tmdb_id: i64,
    pub imdb_id: String,
    pub media_type: String,
    pub title: String,
    pub year: Option<i64>,
    pub infuse_pro: bool,
    /// Set when a human picked, or an earlier run got that far; the worker
    /// then skips the search.
    pub chosen_infohash: Option<String>,
    /// A human chose `chosen_infohash`: the worker uses exactly that hash and
    /// never falls back to another candidate.
    pub picked: bool,
    /// A torrent an earlier run already added, to resume from.
    pub rd_torrent_id: Option<String>,
    /// A torrent the admin wants gone (the release did not play). The worker
    /// deletes it only if its own run created it, then clears it with a
    /// progress call.
    pub remove_rd_torrent_id: Option<String>,
    pub blocked_hashes: Vec<String>,
}

/// The cheap check before a claim: is there a queued job, or a `fetching`
/// one whose claim has expired? Only a hint, the claim transaction
/// re-reads everything.
async fn has_claimable_fetch_job(pool: &SqlitePool, now: i64) -> Result<bool, sqlx::Error> {
    let stale_before = now - RECLAIM_AFTER_SECS;
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM fetch_jobs
             WHERE state = 'queued'
                OR (state = 'fetching' AND COALESCE(claimed_at, 0) < ?)
           ) AS "claimable!: bool""#,
        stale_before
    )
    .fetch_one(pool)
    .await
}

/// Hands the oldest queued job to the worker: sets it `fetching` with a
/// fresh `claim_id`. `enabled` is the kill switch ([`worker_settings`]);
/// off, nothing is touched. With nothing to do, no write transaction is
/// opened, so an idle worker polling every few seconds never takes the
/// write lock.
///
/// Before handing anything out it tidies up: expired `fetching` jobs go
/// back to `queued` (or `failed` with reason `max_attempts` once they have
/// used their attempts), queued jobs whose request is no longer pending are
/// closed, and queued jobs whose request has no IMDb ID wait in
/// `needs_pick`.
pub async fn claim_fetch_job(
    pool: &SqlitePool,
    enabled: bool,
    now: i64,
) -> Result<Option<ClaimedJob>, sqlx::Error> {
    Ok(claim_fetch_job_with_changes(pool, enabled, now).await?.job)
}

/// The result of [`claim_fetch_job_with_changes`].
pub struct ClaimResult {
    pub job: Option<ClaimedJob>,
    /// The tidy-up moved at least one job (reclaimed, failed, closed or
    /// parked), whether or not a job was handed out. The caller publishes a
    /// tick for it; an idle poll leaves this `false`.
    pub tidied: bool,
}

/// [`claim_fetch_job`], also saying whether the tidy-up changed any job.
pub async fn claim_fetch_job_with_changes(
    pool: &SqlitePool,
    enabled: bool,
    now: i64,
) -> Result<ClaimResult, sqlx::Error> {
    if !enabled || !has_claimable_fetch_job(pool, now).await? {
        return Ok(ClaimResult {
            job: None,
            tidied: false,
        });
    }
    let stale_before = now - RECLAIM_AFTER_SECS;
    let mut tx = begin_immediate(pool).await?;

    let mut tidied = 0;
    tidied += sqlx::query!(
        "UPDATE fetch_jobs
         SET state = 'failed', reason = 'max_attempts', claim_id = NULL, updated_at = ?
         WHERE state = 'fetching' AND COALESCE(claimed_at, 0) < ? AND attempts >= max_attempts",
        now,
        stale_before
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tidied += sqlx::query!(
        "UPDATE fetch_jobs
         SET state = 'queued', claim_id = NULL, updated_at = ?
         WHERE state = 'fetching' AND COALESCE(claimed_at, 0) < ? AND attempts < max_attempts",
        now,
        stale_before
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tidied += sqlx::query!(
        "UPDATE fetch_jobs
         SET state = 'done', reason = 'request_changed', claim_id = NULL, updated_at = ?
         WHERE state = 'queued'
           AND request_id IN (SELECT id FROM requests WHERE status <> 'pending')",
        now
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tidied += sqlx::query!(
        "UPDATE fetch_jobs
         SET state = 'needs_pick', reason = 'no_imdb_id', claim_id = NULL, updated_at = ?
         WHERE state = 'queued'
           AND request_id IN (SELECT id FROM requests WHERE imdb_id IS NULL)",
        now
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let tidied = tidied > 0;

    let Some(next) = sqlx::query!(
        r#"SELECT j.id AS "id!", j.request_id AS "request_id!",
                  r.tmdb_id AS "tmdb_id!", r.media_type AS "media_type!",
                  r.imdb_id AS "imdb_id!", r.title AS "title!", r.year,
                  j.infuse_pro AS "infuse_pro!: bool", j.chosen_infohash,
                  j.picked AS "picked!: bool", j.rd_torrent_id, j.remove_rd_torrent_id
           FROM fetch_jobs j JOIN requests r ON r.id = j.request_id
           WHERE j.state = 'queued' AND r.status = 'pending' AND r.imdb_id IS NOT NULL
           ORDER BY j.created_at, j.id
           LIMIT 1"#
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.commit().await?;
        return Ok(ClaimResult { job: None, tidied });
    };

    let claim_id = crate::auth::generate_claim_id();
    sqlx::query!(
        "UPDATE fetch_jobs
         SET state = 'fetching', claim_id = ?, claimed_at = ?, attempts = attempts + 1,
             updated_at = ?
         WHERE id = ?",
        claim_id,
        now,
        now,
        next.id
    )
    .execute(&mut *tx)
    .await?;
    let blocked_hashes = sqlx::query_scalar!(
        r#"SELECT infohash AS "infohash!" FROM blocked_hashes
           WHERE request_id = ? ORDER BY infohash"#,
        next.request_id
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let job = ClaimedJob {
        id: next.id,
        claim_id,
        tmdb_id: next.tmdb_id,
        imdb_id: next.imdb_id,
        media_type: next.media_type,
        title: next.title,
        year: next.year,
        infuse_pro: next.infuse_pro,
        chosen_infohash: next.chosen_infohash,
        picked: next.picked,
        rd_torrent_id: next.rd_torrent_id,
        remove_rd_torrent_id: next.remove_rd_torrent_id,
        blocked_hashes,
    };
    Ok(ClaimResult {
        job: Some(job),
        tidied,
    })
}

/// The states a worker may end a run in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportState {
    NeedsPick,
    Failed,
    Done,
}

impl ReportState {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportState::NeedsPick => "needs_pick",
            ReportState::Failed => "failed",
            ReportState::Done => "done",
        }
    }
}

/// A worker's final report. Fields are validated by the caller (infohash
/// shape, lengths, `candidates` being well-formed JSON). `None` leaves the
/// stored `candidates`, `top_infohash` and `chosen_infohash` alone;
/// `picked` is never touched here, only a human pick sets it.
#[derive(Debug, Default)]
pub struct Report<'a> {
    pub candidates: Option<&'a str>,
    pub top_infohash: Option<&'a str>,
    pub chosen_infohash: Option<&'a str>,
    pub rd_torrent_id: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub last_error: Option<&'a str>,
    pub uncached_adds: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReportOutcome {
    Applied {
        /// The request moved to `added` or `already_had`, so the caller
        /// should publish a members tick.
        request_status_changed: bool,
    },
    /// No such job in `fetching` with this claim: replayed, forged,
    /// reclaimed or overridden. Carries the id now stored on the job, so
    /// the worker keeps that torrent when it cleans up its own.
    Stale {
        rd_torrent_id: Option<String>,
    },
    /// The job is picked and the report names a different hash.
    PickedLocked,
    /// A `done` report, other than `already_had`, for a job with no RD
    /// torrent id (neither stored by a progress call nor in the report).
    /// Nothing was changed; the claim is still valid.
    MissingTorrent,
    NotFound,
}

/// Why a guarded job update matched no row, in this order: no such job,
/// then a wrong state or claim (so a stale worker gets its 409 before the
/// picked lock is looked at), otherwise the lock.
async fn classify_missed_update(
    conn: &mut SqliteConnection,
    job_id: i64,
    claim_id: &str,
) -> Result<ReportOutcome, sqlx::Error> {
    let Some(job) = sqlx::query!(
        "SELECT state, claim_id, rd_torrent_id FROM fetch_jobs WHERE id = ?",
        job_id
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(ReportOutcome::NotFound);
    };
    if job.state != "fetching" || job.claim_id.as_deref() != Some(claim_id) {
        return Ok(ReportOutcome::Stale {
            rd_torrent_id: job.rd_torrent_id,
        });
    }
    Ok(ReportOutcome::PickedLocked)
}

/// Ends a run. One guarded `UPDATE` makes it atomic: the job must be
/// `fetching` under exactly this `claim_id`, and a picked job's hash may
/// not change. On success `claim_id` is cleared, so replaying the report
/// is stale.
///
/// `needs_pick` and `failed` clear the stored `rd_torrent_id` unless the
/// report gives one (the worker has already deleted its torrent); `done`
/// keeps it, and needs one (stored or given) unless the reason is
/// `already_had`, so a worker that forgot the torrent cannot mark a request
/// added. A `done` report also moves the request to `added` (or
/// `already_had` when the reason says so) in the same transaction, but only
/// if it is still `pending`.
pub async fn report_fetch_job(
    pool: &SqlitePool,
    job_id: i64,
    claim_id: &str,
    state: ReportState,
    report: &Report<'_>,
    now: i64,
) -> Result<ReportOutcome, sqlx::Error> {
    let target = state.as_str();
    let keep_rd = state == ReportState::Done;
    let uncached = report.uncached_adds.clamp(0, MAX_UNCACHED_INCREMENT);
    let mut tx = begin_immediate(pool).await?;
    // `IS NOT`, not `<>`: with a stored NULL `<>` yields NULL, the WHERE
    // would fail and a valid claim would be classified as PickedLocked.
    let updated = sqlx::query!(
        "UPDATE fetch_jobs
         SET state = ?1,
             candidates = COALESCE(?2, candidates),
             top_infohash = COALESCE(?3, top_infohash),
             chosen_infohash = COALESCE(?4, chosen_infohash),
             rd_torrent_id = CASE WHEN ?5 THEN COALESCE(?6, rd_torrent_id) ELSE ?6 END,
             reason = ?7,
             last_error = ?8,
             uncached_adds = uncached_adds + ?9,
             claim_id = NULL,
             updated_at = ?10
         WHERE id = ?11 AND state = 'fetching' AND claim_id = ?12
           AND NOT (picked = 1 AND ?4 IS NOT NULL AND ?4 IS NOT chosen_infohash)",
        target,
        report.candidates,
        report.top_infohash,
        report.chosen_infohash,
        keep_rd,
        report.rd_torrent_id,
        report.reason,
        report.last_error,
        uncached,
        now,
        job_id,
        claim_id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        let outcome = classify_missed_update(&mut tx, job_id, claim_id).await?;
        tx.rollback().await?;
        return Ok(outcome);
    }

    let already_had = report.reason == Some("already_had");
    if state == ReportState::Done && !already_had {
        let stored =
            sqlx::query_scalar!("SELECT rd_torrent_id FROM fetch_jobs WHERE id = ?", job_id)
                .fetch_one(&mut *tx)
                .await?;
        if stored.is_none() {
            tx.rollback().await?;
            return Ok(ReportOutcome::MissingTorrent);
        }
    }

    let mut request_status_changed = false;
    if state == ReportState::Done {
        // The job is already `done`, so the shared status function's
        // close-open-jobs step leaves it (and its reason) alone.
        let request = sqlx::query!(
            r#"SELECT r.id AS "id!", r.status AS "status!"
               FROM fetch_jobs j JOIN requests r ON r.id = j.request_id
               WHERE j.id = ?"#,
            job_id
        )
        .fetch_one(&mut *tx)
        .await?;
        if request.status == "pending" {
            let to = if already_had { "already_had" } else { "added" };
            request_status_changed = matches!(
                set_request_status_in(&mut tx, request.id, to, None, "worker", now).await?,
                StatusChange::Changed { .. }
            );
        }
    }
    tx.commit().await?;
    Ok(ReportOutcome::Applied {
        request_status_changed,
    })
}

/// Title and year of the request behind an automatic job (`origin = 'auto'`);
/// `None` for a job the admin created, or no such job.
pub async fn auto_job_title(
    pool: &SqlitePool,
    job_id: i64,
) -> Result<Option<(String, Option<i64>)>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT r.title AS "title!", r.year
           FROM fetch_jobs j JOIN requests r ON r.id = j.request_id
           WHERE j.id = ? AND j.origin = 'auto'"#,
        job_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.title, r.year)))
}

#[derive(Debug, Default)]
pub struct Progress<'a> {
    pub rd_torrent_id: Option<&'a str>,
    pub chosen_infohash: Option<&'a str>,
    /// The worker deleted this torrent for a removal request; clears
    /// `remove_rd_torrent_id` if it still names it.
    pub removed_rd_torrent_id: Option<&'a str>,
    /// Add-and-delete cycles since the last call, 0 to 5.
    pub uncached_adds: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProgressOutcome {
    Applied,
    Stale { rd_torrent_id: Option<String> },
    PickedLocked,
    NotFound,
}

/// A sign of life from the worker: saves an RD torrent id the moment RD
/// returns it and refreshes `claimed_at`, so a long RD wait is not
/// reclaimed. `state` and `claim_id` stay. Same guard and classification
/// as [`report_fetch_job`], picked lock included.
pub async fn progress_fetch_job(
    pool: &SqlitePool,
    job_id: i64,
    claim_id: &str,
    progress: &Progress<'_>,
    now: i64,
) -> Result<ProgressOutcome, sqlx::Error> {
    let uncached = progress.uncached_adds.clamp(0, MAX_UNCACHED_INCREMENT);
    let mut tx = begin_immediate(pool).await?;
    // `IS NOT` for the reason given in `report_fetch_job`.
    let updated = sqlx::query!(
        "UPDATE fetch_jobs
         SET claimed_at = ?1,
             updated_at = ?1,
             rd_torrent_id = COALESCE(?2, rd_torrent_id),
             chosen_infohash = COALESCE(?3, chosen_infohash),
             uncached_adds = uncached_adds + ?4,
             remove_rd_torrent_id = CASE WHEN remove_rd_torrent_id = ?7 THEN NULL
                                         ELSE remove_rd_torrent_id END
         WHERE id = ?5 AND state = 'fetching' AND claim_id = ?6
           AND NOT (picked = 1 AND ?3 IS NOT NULL AND ?3 IS NOT chosen_infohash)",
        now,
        progress.rd_torrent_id,
        progress.chosen_infohash,
        uncached,
        job_id,
        claim_id,
        progress.removed_rd_torrent_id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let outcome = if updated > 0 {
        ProgressOutcome::Applied
    } else {
        match classify_missed_update(&mut tx, job_id, claim_id).await? {
            ReportOutcome::Stale { rd_torrent_id } => ProgressOutcome::Stale { rd_torrent_id },
            ReportOutcome::NotFound => ProgressOutcome::NotFound,
            _ => ProgressOutcome::PickedLocked,
        }
    };
    if updated > 0 {
        tx.commit().await?;
    } else {
        tx.rollback().await?;
    }
    Ok(outcome)
}

#[derive(Debug, PartialEq, Eq)]
pub enum RetryOutcome {
    Retried,
    NotFound,
    WrongState,
}

/// Requeues a job from `failed`, `needs_pick`, `fetching` or `done`, but
/// only while its request is still `pending`: once the request was marked
/// added or refused (by hand, say after a `failed` job) there is nothing
/// left to fetch. Attempts and messages reset; the
/// shortlist, chosen hash and RD torrent id are kept so the next run
/// resumes. Clearing `claim_id` in the same statement makes a running
/// worker's next call stale. Refused while the request has no IMDb ID
/// (checked now, not by the job's reason, which can be cleared): the worker
/// could never search for it and the job would loop.
pub async fn retry_fetch_job(
    pool: &SqlitePool,
    job_id: i64,
    now: i64,
) -> Result<RetryOutcome, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    let Some(job) = sqlx::query!(
        r#"SELECT j.state AS "state!", r.status AS "status!", r.imdb_id
           FROM fetch_jobs j JOIN requests r ON r.id = j.request_id
           WHERE j.id = ?"#,
        job_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(RetryOutcome::NotFound);
    };
    let allowed = matches!(
        job.state.as_str(),
        "failed" | "needs_pick" | "fetching" | "done"
    );
    if !allowed || job.status != "pending" || job.imdb_id.is_none() {
        tx.rollback().await?;
        return Ok(RetryOutcome::WrongState);
    }
    sqlx::query!(
        "UPDATE fetch_jobs
         SET state = 'queued', claim_id = NULL, claimed_at = NULL, attempts = 0,
             reason = NULL, last_error = NULL, updated_at = ?
         WHERE id = ?",
        now,
        job_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(RetryOutcome::Retried)
}

#[derive(Debug, PartialEq, Eq)]
pub enum PickOutcome {
    Picked { state: String },
    NotFound,
    WrongState,
    NotInShortlist,
    Blocked,
}

/// Records the admin's choice for a `needs_pick` job. The hash must be in
/// the stored shortlist and not blocked for the request. A live pick
/// requeues the job; in dry-run it closes the job as `done` / `dry_run`.
/// The request is untouched either way. This is not `report_fetch_job`: the
/// worker's picked lock does not apply, so an already picked job can be
/// picked again.
pub async fn pick_fetch_job(
    pool: &SqlitePool,
    job_id: i64,
    infohash: &str,
    dry_run: bool,
    now: i64,
) -> Result<PickOutcome, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    let Some(job) = sqlx::query!(
        r#"SELECT state AS "state!", request_id AS "request_id!", candidates
           FROM fetch_jobs WHERE id = ?"#,
        job_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(PickOutcome::NotFound);
    };
    if job.state != "needs_pick" {
        tx.rollback().await?;
        return Ok(PickOutcome::WrongState);
    }
    let listed = job
        .candidates
        .as_deref()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .is_some_and(|list| {
            list.as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("infohash").and_then(serde_json::Value::as_str) == Some(infohash)
                })
            })
        });
    if !listed {
        tx.rollback().await?;
        return Ok(PickOutcome::NotInShortlist);
    }
    let blocked = sqlx::query_scalar!(
        r#"SELECT 1 AS "one!: i64" FROM blocked_hashes WHERE request_id = ? AND infohash = ?"#,
        job.request_id,
        infohash
    )
    .fetch_optional(&mut *tx)
    .await?;
    if blocked.is_some() {
        tx.rollback().await?;
        return Ok(PickOutcome::Blocked);
    }
    let (state, reason) = if dry_run {
        ("done", Some("dry_run"))
    } else {
        ("queued", None)
    };
    let changed = sqlx::query!(
        "UPDATE fetch_jobs
         SET chosen_infohash = ?, picked = 1, attempts = 0, last_error = NULL,
             claim_id = NULL, claimed_at = NULL, state = ?, reason = ?, updated_at = ?
         WHERE id = ? AND state = 'needs_pick'",
        infohash,
        state,
        reason,
        now,
        job_id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed == 0 {
        tx.rollback().await?;
        return Ok(PickOutcome::WrongState);
    }
    tx.commit().await?;
    Ok(PickOutcome::Picked {
        state: state.to_string(),
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum NotPlayingOutcome {
    /// `reopened` is true when an `added` request went back to `pending`.
    Requeued {
        reopened: bool,
    },
    NotFound,
    NoChosenHash,
    /// The request is not `pending` or `added`, or has no IMDb ID.
    NotAllowed,
}

/// The admin says the job's chosen release did not play: blocks the hash
/// for the request, reopens the request if it was already `added`, and
/// requeues the job without a choice. Candidates stay; the next report
/// overwrites them. A stored Real-Debrid torrent id moves to
/// `remove_rd_torrent_id` for the worker to delete (an older pending
/// removal stays when there is no new id). All in one transaction. A
/// running worker's next call is stale, because `claim_id` is cleared.
pub async fn not_playing_fetch_job(
    pool: &SqlitePool,
    job_id: i64,
    now: i64,
) -> Result<NotPlayingOutcome, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    let Some(job) = sqlx::query!(
        r#"SELECT j.request_id AS "request_id!", j.chosen_infohash,
                  r.status AS "status!", r.imdb_id
           FROM fetch_jobs j JOIN requests r ON r.id = j.request_id
           WHERE j.id = ?"#,
        job_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(NotPlayingOutcome::NotFound);
    };
    let Some(chosen) = job.chosen_infohash else {
        tx.rollback().await?;
        return Ok(NotPlayingOutcome::NoChosenHash);
    };
    if !matches!(job.status.as_str(), "pending" | "added") || job.imdb_id.is_none() {
        tx.rollback().await?;
        return Ok(NotPlayingOutcome::NotAllowed);
    }
    sqlx::query!(
        "INSERT OR IGNORE INTO blocked_hashes (request_id, infohash, created_at) VALUES (?, ?, ?)",
        job.request_id,
        chosen,
        now
    )
    .execute(&mut *tx)
    .await?;
    let reopened = job.status == "added"
        && matches!(
            set_request_status_in(&mut tx, job.request_id, "pending", None, "admin", now).await?,
            StatusChange::Changed { .. }
        );
    sqlx::query!(
        "UPDATE fetch_jobs
         SET state = 'queued', chosen_infohash = NULL, picked = 0, claim_id = NULL,
             claimed_at = NULL, attempts = 0, reason = NULL, last_error = NULL,
             remove_rd_torrent_id = COALESCE(rd_torrent_id, remove_rd_torrent_id),
             rd_torrent_id = NULL, updated_at = ?
         WHERE id = ?",
        now,
        job_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(NotPlayingOutcome::Requeued { reopened })
}

#[derive(Debug, PartialEq, Eq)]
pub enum CancelJobOutcome {
    Cancelled,
    NotFound,
    WrongState,
    /// The job is not `fetching` but still names a Real-Debrid torrent: no
    /// worker is running that could remove it.
    HasTorrent,
}

/// Ends a `queued`, `fetching` or `needs_pick` job as `failed` with reason
/// `cancelled`. Cancel is terminal; `retry` is the way back. Like retry it
/// clears `claim_id`, so a running worker's next call is stale.
///
/// A `fetching` job's stored Real-Debrid torrent id moves to
/// `remove_rd_torrent_id`, as for "didn't play": the stale answer then names
/// no torrent, so the running worker deletes the one it created (and only
/// that). A job that is not `fetching` but holds a torrent id is refused,
/// because nobody would remove it.
pub async fn cancel_fetch_job(
    pool: &SqlitePool,
    job_id: i64,
    now: i64,
) -> Result<CancelJobOutcome, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    let Some(job) = sqlx::query!(
        r#"SELECT state AS "state!", rd_torrent_id FROM fetch_jobs WHERE id = ?"#,
        job_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(CancelJobOutcome::NotFound);
    };
    if !matches!(job.state.as_str(), "queued" | "fetching" | "needs_pick") {
        tx.rollback().await?;
        return Ok(CancelJobOutcome::WrongState);
    }
    if job.state != "fetching" && job.rd_torrent_id.is_some() {
        tx.rollback().await?;
        return Ok(CancelJobOutcome::HasTorrent);
    }
    sqlx::query!(
        "UPDATE fetch_jobs
         SET state = 'failed', reason = 'cancelled', claim_id = NULL,
             remove_rd_torrent_id = COALESCE(rd_torrent_id, remove_rd_torrent_id),
             rd_torrent_id = NULL, updated_at = ?
         WHERE id = ?",
        now,
        job_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(CancelJobOutcome::Cancelled)
}

pub async fn get_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT value AS "value!" FROM settings WHERE key = ?"#,
        key
    )
    .fetch_optional(pool)
    .await
}

pub async fn set_setting(pool: &SqlitePool, key: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO settings (key, value) VALUES (?, ?)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        key,
        value
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The worker's kill switch and mode as they currently stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerSettings {
    pub enabled: bool,
    pub mode: WorkerMode,
}

/// Environment override first, then the database, then off and dry-run.
/// A database value that does not parse counts as unset.
pub async fn worker_settings(
    pool: &SqlitePool,
    config: &Config,
) -> Result<WorkerSettings, sqlx::Error> {
    let enabled = match config.worker_enabled {
        Some(enabled) => enabled,
        None => get_setting(pool, SETTING_WORKER_ENABLED).await?.as_deref() == Some("1"),
    };
    let mode = match config.worker_mode {
        Some(mode) => mode,
        None => get_setting(pool, SETTING_WORKER_MODE)
            .await?
            .as_deref()
            .and_then(WorkerMode::parse)
            .unwrap_or(WorkerMode::DryRun),
    };
    Ok(WorkerSettings { enabled, mode })
}

pub const SETTING_WORKER_ENABLED: &str = "worker_enabled";
pub const SETTING_WORKER_MODE: &str = "worker_mode";

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
    async fn auto_job_title_answers_only_for_auto_jobs() {
        let pool = test_pool().await;
        let request_id = request(&pool, 1, Some("tt1"), false).await;
        let job_id = job_for(&pool, request_id, 10).await;
        assert_eq!(auto_job_title(&pool, job_id).await.unwrap(), None);
        assert_eq!(auto_job_title(&pool, 999).await.unwrap(), None);
        sql(&pool, "UPDATE fetch_jobs SET origin = 'auto'").await;
        assert_eq!(
            auto_job_title(&pool, job_id).await.unwrap(),
            Some(("Dune".to_string(), Some(2021)))
        );
    }

    #[tokio::test]
    async fn the_worker_columns_default_on_existing_rows() {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("connect to in-memory sqlite");
        let full = sqlx::migrate!();
        let before = sqlx::migrate::Migrator {
            migrations: full
                .migrations
                .iter()
                .filter(|m| m.version <= 5)
                .cloned()
                .collect::<Vec<_>>()
                .into(),
            ..sqlx::migrate::Migrator::DEFAULT
        };
        before.run(&pool).await.expect("migrations up to 5");
        sql(
            &pool,
            "INSERT INTO members (id, name, code_hash, created_at) VALUES (7, 'Ann', x'01', 0)",
        )
        .await;
        sql(
            &pool,
            "INSERT INTO requests (id, tmdb_id, media_type, title, requested_by, created_at, status_changed_at)
             VALUES (3, 10, 'movie', 'Dune', 7, 5, 5)",
        )
        .await;
        sql(
            &pool,
            "INSERT INTO fetch_jobs (request_id, infuse_pro, rd_torrent_id, created_at, updated_at)
             VALUES (3, 0, 'RD1', 5, 5)",
        )
        .await;

        full.run(&pool).await.expect("upgrade");

        let (removal, origin, rd): (Option<String>, String, Option<String>) = sqlx::query_as(
            "SELECT remove_rd_torrent_id, origin, rd_torrent_id FROM fetch_jobs WHERE request_id = 3",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (removal, origin.as_str(), rd.as_deref()),
            (None, "admin", Some("RD1"))
        );
        let (auto_fetch,): (i64,) = sqlx::query_as("SELECT auto_fetch FROM members WHERE id = 7")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(auto_fetch, 0);

        // The checks hold for new rows too.
        let bad_origin = sqlx::query("UPDATE fetch_jobs SET origin = 'robot'")
            .execute(&pool)
            .await;
        assert!(bad_origin.is_err());
        let bad_flag = sqlx::query("UPDATE members SET auto_fetch = 2")
            .execute(&pool)
            .await;
        assert!(bad_flag.is_err());
        sql(&pool, "UPDATE fetch_jobs SET origin = 'auto'").await;
        sql(&pool, "UPDATE members SET auto_fetch = 1").await;
    }

    #[tokio::test]
    async fn upgrading_from_the_first_schema_keeps_requests_and_events() {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("connect to in-memory sqlite");
        // Stop after the first migration, add data, then apply the rest.
        let full = sqlx::migrate!();
        let first = sqlx::migrate::Migrator {
            migrations: full
                .migrations
                .iter()
                .filter(|m| m.version <= 1)
                .cloned()
                .collect::<Vec<_>>()
                .into(),
            ..sqlx::migrate::Migrator::DEFAULT
        };
        first.run(&pool).await.expect("first migration");
        sqlx::query(
            "INSERT INTO members (id, name, code_hash, created_at) VALUES (7, 'Ann', x'01', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO requests (id, tmdb_id, media_type, title, requested_by, created_at, status_changed_at)
             VALUES (3, 10, 'movie', 'Dune', 7, 5, 5)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO request_events (request_id, kind, actor, created_at) VALUES (3, 'submitted', 'Ann', 5)",
        )
        .execute(&pool)
        .await
        .unwrap();

        full.run(&pool).await.expect("upgrade");

        let row: (i64, i64, Option<String>) =
            sqlx::query_as("SELECT id, requested_by, requested_by_name FROM requests")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, (3, 7, None));
        let events: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM request_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events.0, 1);
        let violations = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(violations.is_empty());
        // The unique key and the index survived the rebuild.
        let dup = sqlx::query(
            "INSERT INTO requests (tmdb_id, media_type, title, requested_by, created_at, status_changed_at)
             VALUES (10, 'movie', 'Dune again', 7, 6, 6)",
        )
        .execute(&pool)
        .await;
        assert!(dup.is_err());
        // Old rows are non-PRO, and the PRO variant of the same title is free.
        let (pro,): (i64,) = sqlx::query_as("SELECT infuse_pro FROM requests WHERE id = 3")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(pro, 0);
        // And have no refusal reason until the admin gives one.
        let (reason,): (Option<String>,) =
            sqlx::query_as("SELECT reject_reason FROM requests WHERE id = 3")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(reason, None);
        sqlx::query(
            "INSERT INTO requests (tmdb_id, media_type, title, infuse_pro, requested_by, created_at, status_changed_at)
             VALUES (10, 'movie', 'Dune PRO', 1, 7, 6, 6)",
        )
        .execute(&pool)
        .await
        .expect("PRO variant of an existing title");
        let index: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'requests_status_created'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(index.0, 1);
        // The fetch tables arrive with the same upgrade, empty.
        for table in ["fetch_jobs", "blocked_hashes", "settings"] {
            let (n,): (i64,) =
                sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                    .fetch_one(&pool)
                    .await
                    .unwrap_or_else(|e| panic!("{table}: {e}"));
            assert_eq!(n, 0, "{table}");
        }
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
        let member_id = insert_member(&pool, "Alice", b"hash", None, 0)
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

    // -----------------------------------------------------------------
    // Fetch jobs
    // -----------------------------------------------------------------

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn test_config() -> Config {
        Config {
            public_origin: "https://example.com".into(),
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            database_path: ":memory:".into(),
            code_pepper: Vec::new(),
            cookie_dev_insecure: false,
            admin_user: "admin".into(),
            admin_password_hash: String::new(),
            trusted_proxy_cidrs: Vec::new(),
            tmdb_token: String::new(),
            tmdb_base_url: String::new(),
            dmm_base_url: String::new(),
            cf_access: None,
            ntfy: Default::default(),
            rate_limits: Default::default(),
            internal_listen: None,
            internal_token: None,
            worker_enabled: None,
            worker_mode: None,
        }
    }

    async fn member(pool: &SqlitePool, name: &str) -> i64 {
        insert_member(pool, name, name.as_bytes(), None, 0)
            .await
            .expect("insert member")
    }

    /// A pending request by a member named Ann (created on first use).
    async fn request(pool: &SqlitePool, tmdb_id: i64, imdb: Option<&str>, pro: bool) -> i64 {
        let ann = match sqlx::query_scalar::<_, i64>("SELECT id FROM members WHERE name = 'Ann'")
            .fetch_optional(pool)
            .await
            .unwrap()
        {
            Some(id) => id,
            None => member(pool, "Ann").await,
        };
        sqlx::query_scalar(
            "INSERT INTO requests (tmdb_id, media_type, imdb_id, title, year, note, infuse_pro,
                                   requested_by, created_at, status_changed_at)
             VALUES (?, 'movie', ?, 'Dune', 2021, 'secret note', ?, ?, 1, 1)
             RETURNING id",
        )
        .bind(tmdb_id)
        .bind(imdb)
        .bind(pro)
        .bind(ann)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Creates the job for `request_id` and returns its id.
    async fn job_for(pool: &SqlitePool, request_id: i64, now: i64) -> i64 {
        assert!(matches!(
            create_fetch_job(pool, request_id, now).await.unwrap(),
            CreateJob::Created { .. }
        ));
        sqlx::query_scalar("SELECT id FROM fetch_jobs WHERE request_id = ?")
            .bind(request_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// A new pending request with a job, claimed at `now`.
    async fn claimed(pool: &SqlitePool, tmdb_id: i64, now: i64) -> ClaimedJob {
        let id = request(pool, tmdb_id, Some("tt0000001"), false).await;
        job_for(pool, id, now).await;
        claim_fetch_job(pool, true, now)
            .await
            .unwrap()
            .expect("a job to claim")
    }

    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct JobRow {
        state: String,
        claim_id: Option<String>,
        claimed_at: Option<i64>,
        attempts: i64,
        uncached_adds: i64,
        candidates: Option<String>,
        top_infohash: Option<String>,
        chosen_infohash: Option<String>,
        picked: bool,
        rd_torrent_id: Option<String>,
        reason: Option<String>,
        last_error: Option<String>,
    }

    async fn job(pool: &SqlitePool, id: i64) -> JobRow {
        sqlx::query_as(
            "SELECT state, claim_id, claimed_at, attempts, uncached_adds, candidates, top_infohash,
                    chosen_infohash, picked, rd_torrent_id, reason, last_error
             FROM fetch_jobs WHERE id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Test-only raw SQL; the statements are literals built in this module.
    async fn sql(pool: &SqlitePool, statement: &str) {
        sqlx::query(sqlx::AssertSqlSafe(statement.to_string()))
            .execute(pool)
            .await
            .unwrap();
    }

    async fn count(pool: &SqlitePool, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn request_status(pool: &SqlitePool, id: i64) -> String {
        sqlx::query_scalar("SELECT status FROM requests WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn request_of(pool: &SqlitePool, job_id: i64) -> i64 {
        sqlx::query_scalar("SELECT request_id FROM fetch_jobs WHERE id = ?")
            .bind(job_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn report(
        pool: &SqlitePool,
        claim: &ClaimedJob,
        state: ReportState,
        report: &Report<'_>,
        now: i64,
    ) -> ReportOutcome {
        report_fetch_job(pool, claim.id, &claim.claim_id, state, report, now)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_fetch_job_covers_every_outcome() {
        let pool = test_pool().await;
        let plain = request(&pool, 1, Some("tt1"), false).await;
        let pro = request(&pool, 1, Some("tt1"), true).await;
        let no_imdb = request(&pool, 2, None, false).await;
        let refused = request(&pool, 3, Some("tt3"), false).await;
        sql(
            &pool,
            &format!("UPDATE requests SET status = 'refused' WHERE id = {refused}"),
        )
        .await;
        let series = request(&pool, 4, Some("tt4"), false).await;
        sql(
            &pool,
            &format!("UPDATE requests SET media_type = 'tv' WHERE id = {series}"),
        )
        .await;

        assert!(matches!(
            create_fetch_job(&pool, plain, 10).await.unwrap(),
            CreateJob::Created { state, .. } if state == "queued"
        ));
        assert!(matches!(
            create_fetch_job(&pool, plain, 11).await.unwrap(),
            CreateJob::Exists
        ));
        assert!(matches!(
            create_fetch_job(&pool, pro, 12).await.unwrap(),
            CreateJob::Created { .. }
        ));
        assert!(matches!(
            create_fetch_job(&pool, no_imdb, 13).await.unwrap(),
            CreateJob::Created { state, .. } if state == "needs_pick"
        ));
        assert!(matches!(
            create_fetch_job(&pool, 999, 14).await.unwrap(),
            CreateJob::NotFound
        ));
        assert!(matches!(
            create_fetch_job(&pool, refused, 15).await.unwrap(),
            CreateJob::NotPending
        ));
        assert!(matches!(
            create_fetch_job(&pool, series, 16).await.unwrap(),
            CreateJob::Unsupported
        ));

        // infuse_pro is copied from the request, and a missing IMDb ID is explained.
        let rows: Vec<(i64, i64, String, Option<String>)> = sqlx::query_as(
            "SELECT request_id, infuse_pro, state, reason FROM fetch_jobs ORDER BY request_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                (plain, 0, "queued".into(), None),
                (pro, 1, "queued".into(), None),
                (no_imdb, 0, "needs_pick".into(), Some("no_imdb_id".into())),
            ]
        );
    }

    async fn insert_job(
        pool: &SqlitePool,
        request_id: i64,
        picked: i64,
        hash: Option<&str>,
    ) -> Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error> {
        sqlx::query(
            "INSERT INTO fetch_jobs (request_id, infuse_pro, picked, chosen_infohash, created_at, updated_at)
             VALUES (?, 0, ?, ?, 0, 0)",
        )
        .bind(request_id)
        .bind(picked)
        .bind(hash)
        .execute(pool)
        .await
    }

    #[tokio::test]
    async fn a_picked_job_must_have_a_stored_hash() {
        let pool = test_pool().await;
        let id = request(&pool, 1, Some("tt1"), false).await;
        assert!(insert_job(&pool, id, 1, None).await.is_err());
        insert_job(&pool, id, 1, Some(HASH_A))
            .await
            .expect("picked with a hash");
        sql(&pool, "DELETE FROM fetch_jobs").await;
        insert_job(&pool, id, 0, None)
            .await
            .expect("unpicked without one");
    }

    #[tokio::test]
    async fn claim_is_off_with_the_kill_switch_and_hands_out_a_job_once() {
        let pool = test_pool().await;
        let id = request(&pool, 1, Some("tt1"), true).await;
        let job_id = job_for(&pool, id, 10).await;

        assert!(claim_fetch_job(&pool, false, 20).await.unwrap().is_none());
        assert_eq!(job(&pool, job_id).await.state, "queued");

        let claimed = claim_fetch_job(&pool, true, 20).await.unwrap().unwrap();
        assert_eq!(claimed.id, job_id);
        assert_eq!(claimed.imdb_id, "tt1");
        assert_eq!((claimed.title.as_str(), claimed.year), ("Dune", Some(2021)));
        assert!(claimed.infuse_pro);
        assert_eq!(claimed.claim_id.len(), 32);
        let row = job(&pool, job_id).await;
        assert_eq!(row.state, "fetching");
        assert_eq!(row.claim_id.as_deref(), Some(claimed.claim_id.as_str()));
        assert_eq!((row.attempts, row.claimed_at), (1, Some(20)));

        assert!(claim_fetch_job(&pool, true, 21).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn claim_takes_the_oldest_job_and_lists_blocked_hashes() {
        let pool = test_pool().await;
        let newer = request(&pool, 1, Some("tt1"), false).await;
        let older = request(&pool, 2, Some("tt2"), false).await;
        job_for(&pool, newer, 20).await;
        let older_job = job_for(&pool, older, 10).await;
        sql(
            &pool,
            &format!(
                "INSERT INTO blocked_hashes (request_id, infohash, created_at) VALUES
                 ({older}, '{HASH_B}', 1), ({older}, '{HASH_A}', 2), ({newer}, '{HASH_B}', 3)"
            ),
        )
        .await;

        let claimed = claim_fetch_job(&pool, true, 30).await.unwrap().unwrap();
        assert_eq!(claimed.id, older_job);
        assert_eq!(claimed.blocked_hashes, vec![HASH_A, HASH_B]);
    }

    #[tokio::test]
    async fn an_idle_claim_opens_no_write_transaction() {
        let pool = test_pool().await;
        assert!(!has_claimable_fetch_job(&pool, 100).await.unwrap());
        assert!(claim_fetch_job(&pool, true, 100).await.unwrap().is_none());

        // Jobs in other states, or `fetching` and fresh, are no reason to write.
        let claim = claimed(&pool, 1, 100).await;
        let id = request(&pool, 2, Some("tt2"), false).await;
        let waiting = job_for(&pool, id, 100).await;
        sql(
            &pool,
            &format!("UPDATE fetch_jobs SET state = 'needs_pick' WHERE id = {waiting}"),
        )
        .await;
        assert!(
            !has_claimable_fetch_job(&pool, 100 + RECLAIM_AFTER_SECS)
                .await
                .unwrap()
        );
        assert!(
            has_claimable_fetch_job(&pool, 100 + RECLAIM_AFTER_SECS + 1)
                .await
                .unwrap()
        );
        let before = job(&pool, claim.id).await;
        assert!(claim_fetch_job(&pool, true, 200).await.unwrap().is_none());
        assert_eq!(job(&pool, claim.id).await, before);
    }

    #[tokio::test]
    async fn an_expired_job_is_requeued_with_its_ids_and_a_new_claim() {
        let pool = test_pool().await;
        let first = claimed(&pool, 1, 1000).await;
        sql(
            &pool,
            &format!(
                "UPDATE fetch_jobs SET chosen_infohash = '{HASH_A}', rd_torrent_id = 'RD1',
                        candidates = '[1]' WHERE id = {}",
                first.id
            ),
        )
        .await;

        // Exactly 45 minutes: still the first worker's.
        let edge = 1000 + RECLAIM_AFTER_SECS;
        assert!(claim_fetch_job(&pool, true, edge).await.unwrap().is_none());
        assert_eq!(job(&pool, first.id).await.state, "fetching");

        let again = claim_fetch_job(&pool, true, edge + 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.id, first.id);
        assert_ne!(again.claim_id, first.claim_id);
        assert_eq!(again.chosen_infohash.as_deref(), Some(HASH_A));
        assert_eq!(again.rd_torrent_id.as_deref(), Some("RD1"));
        let row = job(&pool, first.id).await;
        assert_eq!((row.attempts, row.candidates.as_deref()), (2, Some("[1]")));

        // The first worker's late report matches nothing.
        let late = report(
            &pool,
            &first,
            ReportState::Done,
            &Report::default(),
            edge + 2,
        )
        .await;
        assert_eq!(
            late,
            ReportOutcome::Stale {
                rd_torrent_id: Some("RD1".into())
            }
        );
    }

    #[tokio::test]
    async fn an_expired_job_out_of_attempts_fails_and_is_not_returned() {
        let pool = test_pool().await;
        let first = claimed(&pool, 1, 1000).await;
        sql(&pool, "UPDATE fetch_jobs SET attempts = max_attempts").await;
        let spare_request = request(&pool, 2, Some("tt2"), false).await;
        let spare = job_for(&pool, spare_request, 2000).await;

        let next = claim_fetch_job(&pool, true, 1000 + RECLAIM_AFTER_SECS + 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.id, spare);
        let row = job(&pool, first.id).await;
        assert_eq!(row.state, "failed");
        assert_eq!(row.reason.as_deref(), Some("max_attempts"));
        assert_eq!(row.claim_id, None);
    }

    #[tokio::test]
    async fn claim_closes_jobs_of_requests_that_are_no_longer_pending() {
        let pool = test_pool().await;
        let id = request(&pool, 1, Some("tt1"), false).await;
        let job_id = job_for(&pool, id, 10).await;
        sql(
            &pool,
            &format!("UPDATE requests SET status = 'refused' WHERE id = {id}"),
        )
        .await;

        assert!(claim_fetch_job(&pool, true, 20).await.unwrap().is_none());
        let row = job(&pool, job_id).await;
        assert_eq!(
            (row.state.as_str(), row.reason.as_deref()),
            ("done", Some("request_changed"))
        );
    }

    #[tokio::test]
    async fn claim_parks_a_queued_job_without_an_imdb_id_and_moves_on() {
        let pool = test_pool().await;
        let blind = request(&pool, 1, Some("tt1"), false).await;
        let blind_job = job_for(&pool, blind, 10).await;
        let seeing = request(&pool, 2, Some("tt2"), false).await;
        let seeing_job = job_for(&pool, seeing, 20).await;
        // The ID disappears after the job was queued (older data, a manual edit).
        sql(
            &pool,
            &format!("UPDATE requests SET imdb_id = NULL WHERE id = {blind}"),
        )
        .await;

        let claimed = claim_fetch_job(&pool, true, 30).await.unwrap().unwrap();
        assert_eq!(claimed.id, seeing_job);
        let row = job(&pool, blind_job).await;
        assert_eq!(
            (row.state.as_str(), row.reason.as_deref()),
            ("needs_pick", Some("no_imdb_id"))
        );

        // With nothing else eligible a claim returns nothing, and does not loop on it.
        sql(
            &pool,
            &format!("UPDATE fetch_jobs SET state = 'queued' WHERE id = {blind_job}"),
        )
        .await;
        sql(
            &pool,
            &format!("UPDATE fetch_jobs SET state = 'done' WHERE id = {seeing_job}"),
        )
        .await;
        assert!(claim_fetch_job(&pool, true, 40).await.unwrap().is_none());
        assert_eq!(job(&pool, blind_job).await.state, "needs_pick");
    }

    #[tokio::test]
    async fn progress_saves_ids_and_keeps_the_job_from_being_reclaimed() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;

        let progress = Progress {
            rd_torrent_id: Some("RD9"),
            chosen_infohash: Some(HASH_A),
            uncached_adds: 1,
            ..Default::default()
        };
        let at_44_minutes = 1000 + 44 * 60;
        assert_eq!(
            progress_fetch_job(&pool, claim.id, &claim.claim_id, &progress, at_44_minutes)
                .await
                .unwrap(),
            ProgressOutcome::Applied
        );
        let row = job(&pool, claim.id).await;
        assert_eq!(row.state, "fetching");
        assert_eq!(row.claim_id.as_deref(), Some(claim.claim_id.as_str()));
        assert_eq!(
            (
                row.rd_torrent_id.as_deref(),
                row.chosen_infohash.as_deref(),
                row.uncached_adds,
                row.claimed_at
            ),
            (Some("RD9"), Some(HASH_A), 1, Some(at_44_minutes))
        );

        // Six minutes later the original claim time would have expired; the refreshed one has not.
        assert!(
            claim_fetch_job(&pool, true, 1000 + 50 * 60)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(job(&pool, claim.id).await.state, "fetching");

        // Absent fields stay, and one call adds at most 5 uncached adds.
        let again = Progress {
            uncached_adds: 99,
            ..Progress::default()
        };
        progress_fetch_job(&pool, claim.id, &claim.claim_id, &again, at_44_minutes + 1)
            .await
            .unwrap();
        let row = job(&pool, claim.id).await;
        assert_eq!(
            (row.rd_torrent_id.as_deref(), row.uncached_adds),
            (Some("RD9"), 6)
        );
    }

    #[tokio::test]
    async fn progress_with_a_stale_claim_or_unknown_id_writes_nothing() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;
        sql(&pool, "UPDATE fetch_jobs SET rd_torrent_id = 'RD1'").await;
        let before = job(&pool, claim.id).await;

        let progress = Progress {
            rd_torrent_id: Some("RD2"),
            chosen_infohash: Some(HASH_B),
            uncached_adds: 2,
            ..Default::default()
        };
        assert_eq!(
            progress_fetch_job(&pool, claim.id, "not-the-claim", &progress, 2000)
                .await
                .unwrap(),
            ProgressOutcome::Stale {
                rd_torrent_id: Some("RD1".into())
            }
        );
        assert_eq!(job(&pool, claim.id).await, before);
        assert_eq!(
            progress_fetch_job(&pool, 999, &claim.claim_id, &progress, 2000)
                .await
                .unwrap(),
            ProgressOutcome::NotFound
        );
    }

    #[tokio::test]
    async fn progress_cannot_rewrite_a_human_pick() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;
        sql(
            &pool,
            &format!("UPDATE fetch_jobs SET picked = 1, chosen_infohash = '{HASH_A}'"),
        )
        .await;

        let other = Progress {
            chosen_infohash: Some(HASH_B),
            rd_torrent_id: Some("RD2"),
            ..Progress::default()
        };
        assert_eq!(
            progress_fetch_job(&pool, claim.id, &claim.claim_id, &other, 1500)
                .await
                .unwrap(),
            ProgressOutcome::PickedLocked
        );
        let row = job(&pool, claim.id).await;
        assert_eq!(
            (
                row.chosen_infohash.as_deref(),
                row.rd_torrent_id,
                row.claimed_at
            ),
            (Some(HASH_A), None, Some(1000))
        );

        let same = Progress {
            chosen_infohash: Some(HASH_A),
            ..Progress::default()
        };
        assert_eq!(
            progress_fetch_job(&pool, claim.id, &claim.claim_id, &same, 1500)
                .await
                .unwrap(),
            ProgressOutcome::Applied
        );
        // A stale claim on a picked job is stale, not locked.
        assert_eq!(
            progress_fetch_job(&pool, claim.id, "old", &other, 1500)
                .await
                .unwrap(),
            ProgressOutcome::Stale {
                rd_torrent_id: None
            }
        );
    }

    #[tokio::test]
    async fn report_applies_once_and_clears_the_claim() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;
        let body = Report {
            candidates: Some(r#"[{"h":1}]"#),
            top_infohash: Some(HASH_A),
            reason: Some("dry_run"),
            uncached_adds: 1,
            ..Report::default()
        };

        let wrong = report_fetch_job(
            &pool,
            claim.id,
            "wrong",
            ReportState::NeedsPick,
            &body,
            1100,
        )
        .await
        .unwrap();
        assert_eq!(
            wrong,
            ReportOutcome::Stale {
                rd_torrent_id: None
            }
        );
        assert_eq!(job(&pool, claim.id).await.state, "fetching");

        let applied = report(&pool, &claim, ReportState::NeedsPick, &body, 1100).await;
        assert_eq!(
            applied,
            ReportOutcome::Applied {
                request_status_changed: false
            }
        );
        let row = job(&pool, claim.id).await;
        assert_eq!(row.state, "needs_pick");
        assert_eq!(row.claim_id, None);
        assert_eq!(
            (
                row.candidates.as_deref(),
                row.top_infohash.as_deref(),
                row.reason.as_deref(),
                row.uncached_adds
            ),
            (Some(r#"[{"h":1}]"#), Some(HASH_A), Some("dry_run"), 1)
        );

        let replay = report(&pool, &claim, ReportState::NeedsPick, &body, 1200).await;
        assert!(matches!(replay, ReportOutcome::Stale { .. }));
        let unknown = report_fetch_job(
            &pool,
            999,
            &claim.claim_id,
            ReportState::Failed,
            &body,
            1200,
        )
        .await
        .unwrap();
        assert_eq!(unknown, ReportOutcome::NotFound);
    }

    #[tokio::test]
    async fn report_keeps_the_torrent_id_only_for_done_or_when_given() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;
        sql(&pool, "UPDATE fetch_jobs SET rd_torrent_id = 'RD1'").await;
        let failed = Report {
            reason: Some("picked_failed"),
            last_error: Some("timeout"),
            ..Report::default()
        };
        report(&pool, &claim, ReportState::NeedsPick, &failed, 1100).await;
        let row = job(&pool, claim.id).await;
        assert_eq!(
            (
                row.rd_torrent_id,
                row.reason.as_deref(),
                row.last_error.as_deref()
            ),
            (None, Some("picked_failed"), Some("timeout"))
        );

        // Given explicitly, it is stored even on a failure.
        assert_eq!(
            retry_fetch_job(&pool, claim.id, 1200).await.unwrap(),
            RetryOutcome::Retried
        );
        let second = claim_fetch_job(&pool, true, 1300).await.unwrap().unwrap();
        let keeps = Report {
            rd_torrent_id: Some("RD2"),
            ..Report::default()
        };
        report(&pool, &second, ReportState::Failed, &keeps, 1400).await;
        assert_eq!(
            job(&pool, claim.id).await.rd_torrent_id.as_deref(),
            Some("RD2")
        );

        // `done` keeps the stored id when the report gives none.
        retry_fetch_job(&pool, claim.id, 1500).await.unwrap();
        let third = claim_fetch_job(&pool, true, 1600).await.unwrap().unwrap();
        report(&pool, &third, ReportState::Done, &Report::default(), 1700).await;
        assert_eq!(
            job(&pool, claim.id).await.rd_torrent_id.as_deref(),
            Some("RD2")
        );
    }

    #[tokio::test]
    async fn report_cannot_rewrite_a_human_pick() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;
        sql(
            &pool,
            &format!(
                "UPDATE fetch_jobs SET picked = 1, chosen_infohash = '{HASH_A}', candidates = '[0]'"
            ),
        )
        .await;
        let before = job(&pool, claim.id).await;

        let other = Report {
            chosen_infohash: Some(HASH_B),
            candidates: Some("[1]"),
            reason: Some("x"),
            ..Report::default()
        };
        assert_eq!(
            report(&pool, &claim, ReportState::Failed, &other, 1100).await,
            ReportOutcome::PickedLocked
        );
        assert_eq!(
            job(&pool, claim.id).await,
            before,
            "nothing written, the claim is still usable"
        );

        // A stale claim is reported before the lock.
        let stale = report_fetch_job(&pool, claim.id, "old", ReportState::Failed, &other, 1100)
            .await
            .unwrap();
        assert_eq!(
            stale,
            ReportOutcome::Stale {
                rd_torrent_id: None
            }
        );

        // The same hash applies, and top_infohash is not locked.
        let same = Report {
            chosen_infohash: Some(HASH_A),
            top_infohash: Some(HASH_B),
            ..Report::default()
        };
        assert!(matches!(
            report(&pool, &claim, ReportState::NeedsPick, &same, 1200).await,
            ReportOutcome::Applied { .. }
        ));
        let row = job(&pool, claim.id).await;
        assert_eq!(
            (
                row.top_infohash.as_deref(),
                row.chosen_infohash.as_deref(),
                row.picked
            ),
            (Some(HASH_B), Some(HASH_A), true)
        );

        // A report that names no hash applies too.
        retry_fetch_job(&pool, claim.id, 1300).await.unwrap();
        let next = claim_fetch_job(&pool, true, 1400).await.unwrap().unwrap();
        assert!(matches!(
            report(
                &pool,
                &next,
                ReportState::NeedsPick,
                &Report::default(),
                1500
            )
            .await,
            ReportOutcome::Applied { .. }
        ));
    }

    #[tokio::test]
    async fn an_unpicked_job_may_change_its_hash_freely() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;
        sql(
            &pool,
            &format!("UPDATE fetch_jobs SET chosen_infohash = '{HASH_A}'"),
        )
        .await;
        let other = Report {
            chosen_infohash: Some(HASH_B),
            ..Report::default()
        };
        assert!(matches!(
            report(&pool, &claim, ReportState::NeedsPick, &other, 1100).await,
            ReportOutcome::Applied { .. }
        ));
        assert_eq!(
            job(&pool, claim.id).await.chosen_infohash.as_deref(),
            Some(HASH_B)
        );
    }

    #[tokio::test]
    async fn a_done_report_marks_the_request_added_once_as_the_worker() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;
        let request_id = request_of(&pool, claim.id).await;
        let body = Report {
            chosen_infohash: Some(HASH_A),
            rd_torrent_id: Some("RD1"),
            ..Report::default()
        };

        assert_eq!(
            report(&pool, &claim, ReportState::Done, &body, 1100).await,
            ReportOutcome::Applied {
                request_status_changed: true
            }
        );
        assert_eq!(request_status(&pool, request_id).await, "added");
        let row = job(&pool, claim.id).await;
        assert_eq!(
            (row.state.as_str(), row.reason),
            ("done", None),
            "not rewritten as request_changed"
        );
        let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT actor, from_status, to_status FROM request_events WHERE kind = 'status'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            events,
            vec![(
                "worker".into(),
                Some("pending".into()),
                Some("added".into())
            )]
        );

        assert!(matches!(
            report(&pool, &claim, ReportState::Done, &body, 1200).await,
            ReportOutcome::Stale { .. }
        ));
        assert_eq!(count(&pool, "request_events").await, 1);
    }

    #[tokio::test]
    async fn a_done_report_with_already_had_sets_already_had() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;
        let request_id = request_of(&pool, claim.id).await;
        let body = Report {
            reason: Some("already_had"),
            ..Report::default()
        };
        report(&pool, &claim, ReportState::Done, &body, 1100).await;
        assert_eq!(request_status(&pool, request_id).await, "already_had");
        assert_eq!(
            job(&pool, claim.id).await.reason.as_deref(),
            Some("already_had")
        );
    }

    #[tokio::test]
    async fn a_done_report_leaves_a_request_that_is_no_longer_pending_alone() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;
        let request_id = request_of(&pool, claim.id).await;
        // Bypasses the close rule, as only a direct write can.
        sql(
            &pool,
            &format!("UPDATE requests SET status = 'refused' WHERE id = {request_id}"),
        )
        .await;

        let body = Report {
            rd_torrent_id: Some("RD1"),
            ..Report::default()
        };
        assert_eq!(
            report(&pool, &claim, ReportState::Done, &body, 1100).await,
            ReportOutcome::Applied {
                request_status_changed: false
            }
        );
        assert_eq!(request_status(&pool, request_id).await, "refused");
        assert_eq!(job(&pool, claim.id).await.state, "done");
        assert_eq!(count(&pool, "request_events").await, 0);
    }

    #[tokio::test]
    async fn a_done_report_needs_a_torrent_unless_the_title_was_already_had() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;
        let request_id = request_of(&pool, claim.id).await;

        // Nothing stored and nothing given: refused, nothing changed, and the
        // claim is still good for a corrected report.
        assert_eq!(
            report(&pool, &claim, ReportState::Done, &Report::default(), 1100).await,
            ReportOutcome::MissingTorrent
        );
        let row = job(&pool, claim.id).await;
        assert_eq!(row.state, "fetching");
        assert_eq!(row.claim_id.as_deref(), Some(claim.claim_id.as_str()));
        assert_eq!(request_status(&pool, request_id).await, "pending");
        assert_eq!(count(&pool, "request_events").await, 0);

        // A stale claim is still stale, not missing a torrent.
        let stale = report_fetch_job(
            &pool,
            claim.id,
            "old",
            ReportState::Done,
            &Report::default(),
            1100,
        )
        .await
        .unwrap();
        assert!(matches!(stale, ReportOutcome::Stale { .. }));

        // A torrent stored by an earlier progress call is enough.
        let progress = Progress {
            rd_torrent_id: Some("RD1"),
            ..Progress::default()
        };
        progress_fetch_job(&pool, claim.id, &claim.claim_id, &progress, 1150)
            .await
            .unwrap();
        assert_eq!(
            report(&pool, &claim, ReportState::Done, &Report::default(), 1200).await,
            ReportOutcome::Applied {
                request_status_changed: true
            }
        );
        assert_eq!(request_status(&pool, request_id).await, "added");

        // needs_pick and failed never need one.
        let second = claimed(&pool, 2, 2000).await;
        assert!(matches!(
            report(
                &pool,
                &second,
                ReportState::NeedsPick,
                &Report::default(),
                2100
            )
            .await,
            ReportOutcome::Applied { .. }
        ));
    }

    #[tokio::test]
    async fn a_claim_says_whether_a_human_picked_the_hash() {
        let pool = test_pool().await;
        let id = request(&pool, 1, Some("tt0000001"), false).await;
        let job_id = job_for(&pool, id, 10).await;
        sql(
            &pool,
            &format!("UPDATE fetch_jobs SET chosen_infohash = '{HASH_A}', picked = 1"),
        )
        .await;
        let claim = claim_fetch_job(&pool, true, 20).await.unwrap().unwrap();
        assert_eq!(claim.id, job_id);
        assert!(claim.picked);
        assert_eq!(claim.chosen_infohash.as_deref(), Some(HASH_A));

        // A hash the worker saved itself is not a pick, and stays so after a
        // reclaim.
        sql(&pool, "UPDATE fetch_jobs SET picked = 0 WHERE picked = 1").await;
        let later = 20 + RECLAIM_AFTER_SECS + 1;
        let claim = claim_fetch_job(&pool, true, later).await.unwrap().unwrap();
        assert!(!claim.picked);
        assert_eq!(claim.chosen_infohash.as_deref(), Some(HASH_A));
    }

    #[tokio::test]
    async fn a_claim_reports_what_the_tidy_up_changed_and_an_idle_poll_changes_nothing() {
        let pool = test_pool().await;
        // Nothing to do: no tidy-up.
        let idle = claim_fetch_job_with_changes(&pool, true, 10).await.unwrap();
        assert!(idle.job.is_none() && !idle.tidied);

        // A job reclaimed past its attempts fails; nothing is handed out, but
        // the change is reported.
        let claim = claimed(&pool, 1, 100).await;
        sql(&pool, "UPDATE fetch_jobs SET max_attempts = 1").await;
        let later = 100 + RECLAIM_AFTER_SECS + 1;
        let result = claim_fetch_job_with_changes(&pool, true, later)
            .await
            .unwrap();
        assert!(result.job.is_none() && result.tidied);
        let row = job(&pool, claim.id).await;
        assert_eq!(
            (row.state.as_str(), row.reason.as_deref()),
            ("failed", Some("max_attempts"))
        );
        // And the next poll is idle again.
        let again = claim_fetch_job_with_changes(&pool, true, later + 1)
            .await
            .unwrap();
        assert!(again.job.is_none() && !again.tidied);

        // A queued job whose request changed behind the portal's back.
        let id = request(&pool, 2, Some("tt0000002"), false).await;
        let job_id = job_for(&pool, id, later).await;
        sql(
            &pool,
            &format!("UPDATE requests SET status = 'refused' WHERE id = {id}"),
        )
        .await;
        let result = claim_fetch_job_with_changes(&pool, true, later + 2)
            .await
            .unwrap();
        assert!(result.job.is_none() && result.tidied);
        let row = job(&pool, job_id).await;
        assert_eq!(
            (row.state.as_str(), row.reason.as_deref()),
            ("done", Some("request_changed"))
        );

        // A plain claim has nothing tidied.
        let id = request(&pool, 3, Some("tt0000003"), false).await;
        job_for(&pool, id, later + 3).await;
        let result = claim_fetch_job_with_changes(&pool, true, later + 4)
            .await
            .unwrap();
        assert!(result.job.is_some() && !result.tidied);
    }

    #[tokio::test]
    async fn retry_requeues_from_fetching_and_keeps_what_lets_the_next_run_resume() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;
        sql(
            &pool,
            &format!(
                "UPDATE fetch_jobs SET chosen_infohash = '{HASH_A}', rd_torrent_id = 'RD1',
                        candidates = '[1]', reason = 'rd_error', last_error = 'boom', attempts = 2"
            ),
        )
        .await;

        assert_eq!(
            retry_fetch_job(&pool, claim.id, 1100).await.unwrap(),
            RetryOutcome::Retried
        );
        let row = job(&pool, claim.id).await;
        assert_eq!(
            (
                row.state.as_str(),
                row.claim_id,
                row.claimed_at,
                row.attempts,
                row.reason,
                row.last_error
            ),
            ("queued", None, None, 0, None, None)
        );
        assert_eq!(
            (
                row.chosen_infohash.as_deref(),
                row.rd_torrent_id.as_deref(),
                row.candidates.as_deref()
            ),
            (Some(HASH_A), Some("RD1"), Some("[1]"))
        );

        // The running worker's next calls match no row.
        assert!(matches!(
            progress_fetch_job(&pool, claim.id, &claim.claim_id, &Progress::default(), 1200)
                .await
                .unwrap(),
            ProgressOutcome::Stale { rd_torrent_id: Some(id) } if id == "RD1"
        ));
        assert!(matches!(
            report(&pool, &claim, ReportState::Done, &Report::default(), 1200).await,
            ReportOutcome::Stale { .. }
        ));
    }

    /// A pending request whose job waits in `needs_pick` with a two-entry
    /// shortlist (`HASH_A`, `HASH_B`).
    async fn needs_pick_job(pool: &SqlitePool, tmdb_id: i64) -> (i64, i64) {
        let request_id = request(pool, tmdb_id, Some("tt1"), false).await;
        let job_id = job_for(pool, request_id, 10).await;
        sql(
            pool,
            &format!(
                "UPDATE fetch_jobs SET state = 'needs_pick', reason = 'dry_run', attempts = 2,
                        last_error = 'boom', claim_id = 'c1', claimed_at = 10,
                        candidates = '[{{\"infohash\":\"{HASH_A}\"}},{{\"infohash\":\"{HASH_B}\"}}]'
                 WHERE id = {job_id}"
            ),
        )
        .await;
        (request_id, job_id)
    }

    #[tokio::test]
    async fn pick_queues_the_job_and_resets_the_run() {
        let pool = test_pool().await;
        let (request_id, job_id) = needs_pick_job(&pool, 1).await;

        assert_eq!(
            pick_fetch_job(&pool, job_id, HASH_B, false, 50)
                .await
                .unwrap(),
            PickOutcome::Picked {
                state: "queued".into()
            }
        );
        let row = job(&pool, job_id).await;
        assert_eq!(
            (
                row.state.as_str(),
                row.chosen_infohash.as_deref(),
                row.picked,
                row.attempts,
                row.reason,
                row.last_error,
                row.claim_id,
                row.claimed_at
            ),
            ("queued", Some(HASH_B), true, 0, None, None, None, None)
        );
        assert!(row.candidates.is_some());
        let status: String = sqlx::query_scalar("SELECT status FROM requests WHERE id = ?")
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[tokio::test]
    async fn a_dry_run_pick_closes_the_job_and_leaves_the_request_pending() {
        let pool = test_pool().await;
        let (request_id, job_id) = needs_pick_job(&pool, 1).await;

        assert_eq!(
            pick_fetch_job(&pool, job_id, HASH_A, true, 50)
                .await
                .unwrap(),
            PickOutcome::Picked {
                state: "done".into()
            }
        );
        let row = job(&pool, job_id).await;
        assert_eq!(
            (
                row.state.as_str(),
                row.reason.as_deref(),
                row.chosen_infohash.as_deref(),
                row.picked
            ),
            ("done", Some("dry_run"), Some(HASH_A), true)
        );
        let status: String = sqlx::query_scalar("SELECT status FROM requests WHERE id = ?")
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "pending");
        assert_eq!(
            count(&pool, "request_events WHERE kind = 'status'").await,
            0
        );
    }

    #[tokio::test]
    async fn pick_only_works_on_a_needs_pick_job() {
        let pool = test_pool().await;
        let (_, job_id) = needs_pick_job(&pool, 1).await;
        assert_eq!(
            pick_fetch_job(&pool, 999, HASH_A, false, 50).await.unwrap(),
            PickOutcome::NotFound
        );
        for state in ["queued", "fetching", "failed", "done"] {
            sql(
                &pool,
                &format!("UPDATE fetch_jobs SET state = '{state}' WHERE id = {job_id}"),
            )
            .await;
            assert_eq!(
                pick_fetch_job(&pool, job_id, HASH_A, false, 50)
                    .await
                    .unwrap(),
                PickOutcome::WrongState,
                "{state}"
            );
        }
        let row = job(&pool, job_id).await;
        assert_eq!((row.chosen_infohash, row.picked), (None, false));
    }

    #[tokio::test]
    async fn pick_needs_the_hash_in_a_readable_shortlist() {
        let pool = test_pool().await;
        let (_, job_id) = needs_pick_job(&pool, 1).await;
        let other = "cccccccccccccccccccccccccccccccccccccccc";
        assert_eq!(
            pick_fetch_job(&pool, job_id, other, false, 50)
                .await
                .unwrap(),
            PickOutcome::NotInShortlist
        );
        for candidates in [
            "NULL",
            "'not json'",
            "'{\"infohash\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}'",
            "'[\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"]'",
            "'[{\"infohash\":7}]'",
        ] {
            sql(
                &pool,
                &format!("UPDATE fetch_jobs SET candidates = {candidates} WHERE id = {job_id}"),
            )
            .await;
            assert_eq!(
                pick_fetch_job(&pool, job_id, HASH_A, false, 50)
                    .await
                    .unwrap(),
                PickOutcome::NotInShortlist,
                "{candidates}"
            );
        }
        let row = job(&pool, job_id).await;
        assert_eq!((row.state.as_str(), row.picked), ("needs_pick", false));
    }

    #[tokio::test]
    async fn pick_refuses_a_blocked_hash_and_changes_nothing() {
        let pool = test_pool().await;
        let (request_id, job_id) = needs_pick_job(&pool, 1).await;
        sql(
            &pool,
            &format!("INSERT INTO blocked_hashes VALUES ({request_id}, '{HASH_A}', 1)"),
        )
        .await;
        assert_eq!(
            pick_fetch_job(&pool, job_id, HASH_A, false, 50)
                .await
                .unwrap(),
            PickOutcome::Blocked
        );
        let row = job(&pool, job_id).await;
        assert_eq!(
            (row.state.as_str(), row.chosen_infohash, row.attempts),
            ("needs_pick", None, 2)
        );
        // The other candidate is still fine.
        assert_eq!(
            pick_fetch_job(&pool, job_id, HASH_B, false, 50)
                .await
                .unwrap(),
            PickOutcome::Picked {
                state: "queued".into()
            }
        );
    }

    #[tokio::test]
    async fn an_already_picked_needs_pick_job_can_be_picked_again() {
        let pool = test_pool().await;
        let (_, job_id) = needs_pick_job(&pool, 1).await;
        sql(
            &pool,
            &format!(
                "UPDATE fetch_jobs SET chosen_infohash = '{HASH_A}', picked = 1 WHERE id = {job_id}"
            ),
        )
        .await;
        assert_eq!(
            pick_fetch_job(&pool, job_id, HASH_B, true, 50)
                .await
                .unwrap(),
            PickOutcome::Picked {
                state: "done".into()
            }
        );
        let row = job(&pool, job_id).await;
        assert_eq!(
            (row.chosen_infohash.as_deref(), row.picked),
            (Some(HASH_B), true)
        );
    }

    /// A pending request with a job in `state` that has `HASH_A` chosen,
    /// picked, a claim and some run history.
    async fn chosen_job(pool: &SqlitePool, tmdb_id: i64, state: &str) -> (i64, i64) {
        let request_id = request(pool, tmdb_id, Some("tt1"), false).await;
        let job_id = job_for(pool, request_id, 10).await;
        sql(
            pool,
            &format!(
                "UPDATE fetch_jobs SET state = '{state}', chosen_infohash = '{HASH_A}', picked = 1,
                        claim_id = 'c1', claimed_at = 10, attempts = 2, reason = 'dry_run',
                        last_error = 'boom', top_infohash = '{HASH_B}', candidates = '[1]'
                 WHERE id = {job_id}"
            ),
        )
        .await;
        (request_id, job_id)
    }

    #[tokio::test]
    async fn not_playing_blocks_the_hash_and_requeues_from_every_state() {
        let pool = test_pool().await;
        for (n, state) in ["queued", "fetching", "needs_pick", "failed", "done"]
            .into_iter()
            .enumerate()
        {
            let tmdb_id = i64::try_from(n).unwrap() + 1;
            let (request_id, job_id) = chosen_job(&pool, tmdb_id, state).await;
            assert_eq!(
                not_playing_fetch_job(&pool, job_id, 50).await.unwrap(),
                NotPlayingOutcome::Requeued { reopened: false },
                "{state}"
            );
            let row = job(&pool, job_id).await;
            assert_eq!(
                (
                    row.state.as_str(),
                    row.chosen_infohash.as_deref(),
                    row.picked,
                    row.claim_id.as_deref(),
                    row.claimed_at,
                    row.attempts,
                    row.reason.as_deref(),
                    row.last_error.as_deref()
                ),
                ("queued", None, false, None, None, 0, None, None),
                "{state}"
            );
            // The list and the top hash stay for the admin to look at.
            assert_eq!(
                (row.top_infohash.as_deref(), row.candidates.as_deref()),
                (Some(HASH_B), Some("[1]"))
            );
            assert_eq!(request_status(&pool, request_id).await, "pending");

            // The next claim carries the blocked hash.
            let claimed = claim_fetch_job(&pool, true, 60).await.unwrap().unwrap();
            assert_eq!(claimed.id, job_id);
            assert_eq!(claimed.blocked_hashes, vec![HASH_A], "{state}");
            sql(
                &pool,
                &format!("UPDATE fetch_jobs SET state = 'failed' WHERE id = {job_id}"),
            )
            .await;
        }
    }

    #[tokio::test]
    async fn not_playing_reopens_an_added_request_with_a_status_event() {
        let pool = test_pool().await;
        let (request_id, job_id) = chosen_job(&pool, 1, "done").await;
        set_request_status(&pool, request_id, "added", None, 20)
            .await
            .unwrap();
        // Closing the request by hand leaves the chosen hash in place.
        assert_eq!(request_status(&pool, request_id).await, "added");

        assert_eq!(
            not_playing_fetch_job(&pool, job_id, 50).await.unwrap(),
            NotPlayingOutcome::Requeued { reopened: true }
        );
        assert_eq!(request_status(&pool, request_id).await, "pending");
        let event: (String, Option<String>, Option<String>, i64) = sqlx::query_as(
            "SELECT actor, from_status, to_status, created_at FROM request_events
             WHERE request_id = ? AND kind = 'status' ORDER BY id DESC LIMIT 1",
        )
        .bind(request_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            event,
            (
                "admin".into(),
                Some("added".into()),
                Some("pending".into()),
                50
            )
        );
        let (state, hash): (String, Option<String>) =
            sqlx::query_as("SELECT state, chosen_infohash FROM fetch_jobs WHERE id = ?")
                .bind(job_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((state.as_str(), hash), ("queued", None));
        assert_eq!(count(&pool, "blocked_hashes").await, 1);
    }

    #[tokio::test]
    async fn not_playing_leaves_settled_and_unfit_requests_alone() {
        let pool = test_pool().await;
        for (n, status) in ["refused", "already_had"].into_iter().enumerate() {
            let (request_id, job_id) =
                chosen_job(&pool, i64::try_from(n).unwrap() + 1, "done").await;
            sql(
                &pool,
                &format!("UPDATE requests SET status = '{status}' WHERE id = {request_id}"),
            )
            .await;
            assert_eq!(
                not_playing_fetch_job(&pool, job_id, 50).await.unwrap(),
                NotPlayingOutcome::NotAllowed,
                "{status}"
            );
            assert_eq!(request_status(&pool, request_id).await, status);
            let row = job(&pool, job_id).await;
            assert_eq!(
                (
                    row.state.as_str(),
                    row.chosen_infohash.as_deref(),
                    row.picked
                ),
                ("done", Some(HASH_A), true)
            );
        }

        let (request_id, job_id) = chosen_job(&pool, 3, "needs_pick").await;
        sql(
            &pool,
            &format!("UPDATE requests SET imdb_id = NULL WHERE id = {request_id}"),
        )
        .await;
        assert_eq!(
            not_playing_fetch_job(&pool, job_id, 50).await.unwrap(),
            NotPlayingOutcome::NotAllowed
        );
        assert_eq!(count(&pool, "blocked_hashes").await, 0);
    }

    #[tokio::test]
    async fn not_playing_refuses_without_a_hash() {
        let pool = test_pool().await;
        assert_eq!(
            not_playing_fetch_job(&pool, 999, 50).await.unwrap(),
            NotPlayingOutcome::NotFound
        );

        let request_id = request(&pool, 1, Some("tt1"), false).await;
        let job_id = job_for(&pool, request_id, 10).await;
        assert_eq!(
            not_playing_fetch_job(&pool, job_id, 50).await.unwrap(),
            NotPlayingOutcome::NoChosenHash
        );
    }

    async fn removal_of(pool: &SqlitePool, job_id: i64) -> (Option<String>, Option<String>) {
        sqlx::query_as("SELECT rd_torrent_id, remove_rd_torrent_id FROM fetch_jobs WHERE id = ?")
            .bind(job_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn not_playing_moves_a_stored_torrent_to_the_removal_column() {
        let pool = test_pool().await;
        let (request_id, job_id) = chosen_job(&pool, 2, "failed").await;
        sql(
            &pool,
            &format!("UPDATE fetch_jobs SET rd_torrent_id = 'RD1' WHERE id = {job_id}"),
        )
        .await;
        set_request_status(&pool, request_id, "added", None, 20)
            .await
            .unwrap();
        assert_eq!(
            not_playing_fetch_job(&pool, job_id, 50).await.unwrap(),
            NotPlayingOutcome::Requeued { reopened: true }
        );
        assert_eq!(count(&pool, "blocked_hashes").await, 1);
        assert_eq!(request_status(&pool, request_id).await, "pending");
        assert_eq!(
            removal_of(&pool, job_id).await,
            (None, Some("RD1".to_string()))
        );
        let row = job(&pool, job_id).await;
        assert_eq!(
            (
                row.state.as_str(),
                row.chosen_infohash.as_deref(),
                row.picked
            ),
            ("queued", None, false)
        );

        // The requeued job hands the removal to the next claim.
        let claim = claim_fetch_job(&pool, true, 60)
            .await
            .unwrap()
            .expect("the requeued job");
        assert_eq!(claim.id, job_id);
        assert_eq!(claim.rd_torrent_id, None);
        assert_eq!(claim.remove_rd_torrent_id.as_deref(), Some("RD1"));
    }

    #[tokio::test]
    async fn not_playing_keeps_an_older_removal_unless_there_is_a_new_torrent() {
        let pool = test_pool().await;
        let (_, job_id) = chosen_job(&pool, 1, "done").await;
        sql(
            &pool,
            &format!("UPDATE fetch_jobs SET remove_rd_torrent_id = 'OLD' WHERE id = {job_id}"),
        )
        .await;
        not_playing_fetch_job(&pool, job_id, 50).await.unwrap();
        assert_eq!(removal_of(&pool, job_id).await, (None, Some("OLD".into())));

        // A new torrent is the one to remove next; nothing becomes NULL.
        sql(
            &pool,
            &format!(
                "UPDATE fetch_jobs SET chosen_infohash = '{HASH_A}', rd_torrent_id = 'NEW'
                 WHERE id = {job_id}"
            ),
        )
        .await;
        not_playing_fetch_job(&pool, job_id, 60).await.unwrap();
        assert_eq!(removal_of(&pool, job_id).await, (None, Some("NEW".into())));
    }

    #[tokio::test]
    async fn progress_clears_a_removal_only_when_it_names_the_same_id() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;
        sql(
            &pool,
            &format!(
                "UPDATE fetch_jobs SET remove_rd_torrent_id = 'RD1' WHERE id = {}",
                claim.id
            ),
        )
        .await;
        let call = |removed: Option<&'static str>, claim_id: String| {
            let pool = pool.clone();
            let id = claim.id;
            async move {
                let progress = Progress {
                    removed_rd_torrent_id: removed,
                    ..Default::default()
                };
                progress_fetch_job(&pool, id, &claim_id, &progress, 1100)
                    .await
                    .unwrap()
            }
        };

        // Nothing named, or another id: untouched.
        assert_eq!(
            call(None, claim.claim_id.clone()).await,
            ProgressOutcome::Applied
        );
        assert_eq!(
            call(Some("OTHER"), claim.claim_id.clone()).await,
            ProgressOutcome::Applied
        );
        assert_eq!(removal_of(&pool, claim.id).await.1.as_deref(), Some("RD1"));

        // A stale claim cannot clear it.
        assert!(matches!(
            call(Some("RD1"), "not-the-claim".into()).await,
            ProgressOutcome::Stale { .. }
        ));
        assert_eq!(removal_of(&pool, claim.id).await.1.as_deref(), Some("RD1"));

        assert_eq!(
            call(Some("RD1"), claim.claim_id.clone()).await,
            ProgressOutcome::Applied
        );
        assert_eq!(removal_of(&pool, claim.id).await.1, None);
    }

    #[tokio::test]
    async fn retry_is_refused_from_queued_unknown_and_for_requests_without_an_imdb_id() {
        let pool = test_pool().await;
        let id = request(&pool, 1, Some("tt1"), false).await;
        let job_id = job_for(&pool, id, 10).await;
        assert_eq!(
            retry_fetch_job(&pool, job_id, 20).await.unwrap(),
            RetryOutcome::WrongState
        );
        assert_eq!(
            retry_fetch_job(&pool, 999, 20).await.unwrap(),
            RetryOutcome::NotFound
        );

        // No IMDb ID: refused whatever the job's reason says, fine once an ID exists.
        let blind = request(&pool, 2, None, false).await;
        let blind_job = job_for(&pool, blind, 10).await;
        assert_eq!(
            retry_fetch_job(&pool, blind_job, 20).await.unwrap(),
            RetryOutcome::WrongState
        );
        sql(
            &pool,
            &format!("UPDATE fetch_jobs SET reason = NULL WHERE id = {blind_job}"),
        )
        .await;
        assert_eq!(
            retry_fetch_job(&pool, blind_job, 20).await.unwrap(),
            RetryOutcome::WrongState
        );
        sql(
            &pool,
            &format!("UPDATE requests SET imdb_id = 'tt2' WHERE id = {blind}"),
        )
        .await;
        assert_eq!(
            retry_fetch_job(&pool, blind_job, 30).await.unwrap(),
            RetryOutcome::Retried
        );
    }

    #[tokio::test]
    async fn retry_from_done_needs_a_pending_request() {
        let pool = test_pool().await;
        let id = request(&pool, 1, Some("tt1"), false).await;
        let job_id = job_for(&pool, id, 10).await;
        sql(
            &pool,
            &format!(
                "UPDATE fetch_jobs SET state = 'done', reason = 'dry_run', picked = 1,
                        chosen_infohash = '{HASH_A}'"
            ),
        )
        .await;
        assert_eq!(
            retry_fetch_job(&pool, job_id, 20).await.unwrap(),
            RetryOutcome::Retried
        );
        let row = job(&pool, job_id).await;
        assert_eq!(
            (
                row.state.as_str(),
                row.chosen_infohash.as_deref(),
                row.picked
            ),
            ("queued", Some(HASH_A), true)
        );

        sql(&pool, "UPDATE fetch_jobs SET state = 'done'").await;
        for status in ["added", "refused"] {
            sql(&pool, &format!("UPDATE requests SET status = '{status}'")).await;
            assert_eq!(
                retry_fetch_job(&pool, job_id, 30).await.unwrap(),
                RetryOutcome::WrongState,
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn retry_needs_a_pending_request_from_every_origin_state() {
        for origin in ["failed", "needs_pick", "fetching", "done"] {
            let pool = test_pool().await;
            let id = request(&pool, 1, Some("tt1"), false).await;
            let job_id = job_for(&pool, id, 10).await;
            let claim_part = if origin == "fetching" {
                ", claim_id = 'c1', claimed_at = 10"
            } else {
                ""
            };
            sql(
                &pool,
                &format!("UPDATE fetch_jobs SET state = '{origin}'{claim_part}"),
            )
            .await;
            for status in ["added", "refused", "already_had"] {
                sql(&pool, &format!("UPDATE requests SET status = '{status}'")).await;
                assert_eq!(
                    retry_fetch_job(&pool, job_id, 20).await.unwrap(),
                    RetryOutcome::WrongState,
                    "{origin} / {status}"
                );
                let row = job(&pool, job_id).await;
                assert_eq!(row.state, origin, "{origin} / {status}");
            }
            sql(&pool, "UPDATE requests SET status = 'pending'").await;
            assert_eq!(
                retry_fetch_job(&pool, job_id, 30).await.unwrap(),
                RetryOutcome::Retried,
                "{origin}"
            );
            assert_eq!(job(&pool, job_id).await.state, "queued", "{origin}");
        }
    }

    #[tokio::test]
    async fn cancel_ends_open_jobs_as_failed_and_cancelled() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;
        assert_eq!(
            cancel_fetch_job(&pool, claim.id, 1100).await.unwrap(),
            CancelJobOutcome::Cancelled
        );
        let row = job(&pool, claim.id).await;
        assert_eq!(
            (row.state.as_str(), row.reason.as_deref(), row.claim_id),
            ("failed", Some("cancelled"), None)
        );
        assert!(matches!(
            progress_fetch_job(&pool, claim.id, &claim.claim_id, &Progress::default(), 1200)
                .await
                .unwrap(),
            ProgressOutcome::Stale { .. }
        ));

        // Terminal: cancelling again is refused, and so is cancelling a finished job.
        assert_eq!(
            cancel_fetch_job(&pool, claim.id, 1300).await.unwrap(),
            CancelJobOutcome::WrongState
        );
        sql(&pool, "UPDATE fetch_jobs SET state = 'done'").await;
        assert_eq!(
            cancel_fetch_job(&pool, claim.id, 1300).await.unwrap(),
            CancelJobOutcome::WrongState
        );
        assert_eq!(
            cancel_fetch_job(&pool, 999, 1300).await.unwrap(),
            CancelJobOutcome::NotFound
        );

        // Retry is the way back.
        sql(&pool, "UPDATE fetch_jobs SET state = 'failed'").await;
        assert_eq!(
            retry_fetch_job(&pool, claim.id, 1400).await.unwrap(),
            RetryOutcome::Retried
        );
    }

    #[tokio::test]
    async fn cancelling_a_fetching_job_moves_its_torrent_to_the_removal_column() {
        let pool = test_pool().await;
        let claim = claimed(&pool, 1, 1000).await;
        progress_fetch_job(
            &pool,
            claim.id,
            &claim.claim_id,
            &Progress {
                rd_torrent_id: Some("RD1"),
                ..Progress::default()
            },
            1010,
        )
        .await
        .unwrap();
        assert_eq!(
            cancel_fetch_job(&pool, claim.id, 1100).await.unwrap(),
            CancelJobOutcome::Cancelled
        );
        assert_eq!(
            removal_of(&pool, claim.id).await,
            (None, Some("RD1".to_string()))
        );
        // The running worker's next call is stale and names no torrent to keep.
        assert_eq!(
            progress_fetch_job(&pool, claim.id, &claim.claim_id, &Progress::default(), 1200)
                .await
                .unwrap(),
            ProgressOutcome::Stale {
                rd_torrent_id: None
            }
        );
    }

    #[tokio::test]
    async fn cancelling_a_job_that_is_not_fetching_but_holds_a_torrent_is_refused() {
        for state in ["queued", "needs_pick"] {
            let pool = test_pool().await;
            let claim = claimed(&pool, 1, 1000).await;
            sql(
                &pool,
                &format!(
                    "UPDATE fetch_jobs SET state = '{state}', claim_id = NULL, rd_torrent_id = 'RD1'"
                ),
            )
            .await;
            assert_eq!(
                cancel_fetch_job(&pool, claim.id, 1100).await.unwrap(),
                CancelJobOutcome::HasTorrent,
                "{state}"
            );
            let row = job(&pool, claim.id).await;
            assert_eq!(
                (row.state.as_str(), row.rd_torrent_id.as_deref()),
                (state, Some("RD1"))
            );
        }
    }

    #[tokio::test]
    async fn a_request_leaving_pending_closes_its_open_job() {
        for open in ["queued", "needs_pick", "fetching"] {
            let pool = test_pool().await;
            let id = request(&pool, 1, Some("tt1"), false).await;
            let job_id = job_for(&pool, id, 10).await;
            let claim = claim_fetch_job(&pool, true, 20).await.unwrap().unwrap();
            match open {
                "queued" => {
                    sql(
                        &pool,
                        "UPDATE fetch_jobs SET state = 'queued', claim_id = NULL",
                    )
                    .await
                }
                "needs_pick" => {
                    sql(
                        &pool,
                        "UPDATE fetch_jobs SET state = 'needs_pick', claim_id = NULL",
                    )
                    .await
                }
                _ => {}
            }

            let change = set_request_status(&pool, id, "refused", Some("no"), 30)
                .await
                .unwrap();
            assert!(matches!(change, StatusChange::Changed { .. }));
            let row = job(&pool, job_id).await;
            assert_eq!(
                (row.state.as_str(), row.reason.as_deref(), row.claim_id),
                ("done", Some("request_changed"), None),
                "{open}"
            );

            // The worker that still held it is told at its next call.
            assert!(matches!(
                progress_fetch_job(&pool, job_id, &claim.claim_id, &Progress::default(), 40)
                    .await
                    .unwrap(),
                ProgressOutcome::Stale { .. }
            ));
            assert!(matches!(
                report(&pool, &claim, ReportState::Done, &Report::default(), 40).await,
                ReportOutcome::Stale { .. }
            ));
            assert_eq!(request_status(&pool, id).await, "refused");

            // Reopened: retry re-runs the job, and creating another one is refused.
            set_request_status(&pool, id, "pending", None, 50)
                .await
                .unwrap();
            assert!(matches!(
                create_fetch_job(&pool, id, 60).await.unwrap(),
                CreateJob::Exists
            ));
            assert_eq!(
                retry_fetch_job(&pool, job_id, 60).await.unwrap(),
                RetryOutcome::Retried
            );
        }
    }

    #[tokio::test]
    async fn the_admin_status_function_still_logs_admin_and_reopening_closes_nothing() {
        let pool = test_pool().await;
        let id = request(&pool, 1, Some("tt1"), false).await;
        let job_id = job_for(&pool, id, 10).await;
        assert!(matches!(
            set_request_status(&pool, id, "pending", None, 20)
                .await
                .unwrap(),
            StatusChange::Unchanged
        ));
        assert_eq!(job(&pool, job_id).await.state, "queued");
        assert!(matches!(
            set_request_status(&pool, 999, "added", None, 20)
                .await
                .unwrap(),
            StatusChange::NotFound
        ));
        set_request_status(&pool, id, "added", None, 30)
            .await
            .unwrap();
        let actor: String =
            sqlx::query_scalar("SELECT actor FROM request_events WHERE kind = 'status'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(actor, "admin");
    }

    #[tokio::test]
    async fn settings_default_off_then_database_then_environment() {
        let pool = test_pool().await;
        let mut cfg = test_config();
        assert_eq!(
            worker_settings(&pool, &cfg).await.unwrap(),
            WorkerSettings {
                enabled: false,
                mode: WorkerMode::DryRun
            }
        );
        assert_eq!(get_setting(&pool, "nope").await.unwrap(), None);

        set_setting(&pool, SETTING_WORKER_ENABLED, "1")
            .await
            .unwrap();
        set_setting(&pool, SETTING_WORKER_MODE, WorkerMode::Assisted.as_str())
            .await
            .unwrap();
        assert_eq!(
            worker_settings(&pool, &cfg).await.unwrap(),
            WorkerSettings {
                enabled: true,
                mode: WorkerMode::Assisted
            }
        );
        // An upsert replaces.
        set_setting(&pool, SETTING_WORKER_MODE, "auto")
            .await
            .unwrap();
        assert_eq!(
            worker_settings(&pool, &cfg).await.unwrap().mode,
            WorkerMode::Auto
        );

        cfg.worker_enabled = Some(false);
        cfg.worker_mode = Some(WorkerMode::DryRun);
        assert_eq!(
            worker_settings(&pool, &cfg).await.unwrap(),
            WorkerSettings {
                enabled: false,
                mode: WorkerMode::DryRun
            }
        );

        // Garbage in the database counts as unset.
        cfg.worker_mode = None;
        set_setting(&pool, SETTING_WORKER_MODE, "bogus")
            .await
            .unwrap();
        assert_eq!(
            worker_settings(&pool, &cfg).await.unwrap().mode,
            WorkerMode::DryRun
        );
    }

    #[tokio::test]
    async fn deleting_a_request_takes_its_job_and_blocked_hashes_along() {
        for admin_delete in [true, false] {
            let pool = test_pool().await;
            let id = request(&pool, 1, Some("tt1"), false).await;
            let ann: i64 = sqlx::query_scalar("SELECT id FROM members WHERE name = 'Ann'")
                .fetch_one(&pool)
                .await
                .unwrap();
            job_for(&pool, id, 10).await;
            sql(
                &pool,
                &format!("INSERT INTO blocked_hashes VALUES ({id}, '{HASH_A}', 1)"),
            )
            .await;
            if admin_delete {
                assert!(delete_request(&pool, id).await.unwrap());
            } else {
                assert!(matches!(
                    cancel_request(&pool, id, ann).await.unwrap(),
                    CancelOutcome::Cancelled
                ));
            }
            for table in ["fetch_jobs", "blocked_hashes"] {
                assert_eq!(
                    count(&pool, table).await,
                    0,
                    "{table} after admin_delete={admin_delete}"
                );
            }
        }
    }

    async fn event_actors(pool: &SqlitePool, kind: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT actor FROM request_events WHERE kind = ? ORDER BY id")
            .bind(kind)
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn renaming_or_deleting_a_member_leaves_status_events_alone() {
        let pool = test_pool().await;
        let id = request(&pool, 1, Some("tt1"), false).await;
        for kind in ["status", "duplicate"] {
            for name in ["admin", "worker"] {
                sql(
                    &pool,
                    &format!(
                        "INSERT INTO request_events (request_id, kind, actor, created_at)
                         VALUES ({id}, '{kind}', '{name}', 1)"
                    ),
                )
                .await;
            }
        }
        let admin = member(&pool, "admin").await;
        let worker = member(&pool, "worker").await;

        for (member_id, new_name) in [(admin, "Adam"), (worker, "Wendy")] {
            let update = MemberUpdate {
                name: Some(new_name),
                daily_limit: None,
            };
            update_member(&pool, member_id, &update).await.unwrap();
        }
        assert_eq!(event_actors(&pool, "status").await, ["admin", "worker"]);
        assert_eq!(event_actors(&pool, "duplicate").await, ["Adam", "Wendy"]);

        for member_id in [admin, worker] {
            revoke_member(&pool, member_id, 5).await.unwrap();
            assert!(matches!(
                delete_member(&pool, member_id).await.unwrap(),
                MemberDelete::Deleted
            ));
        }
        assert_eq!(event_actors(&pool, "status").await, ["admin", "worker"]);
        assert_eq!(
            event_actors(&pool, "duplicate").await,
            ["Adam (deleted)", "Wendy (deleted)"]
        );
    }
}
