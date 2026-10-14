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
    pub code_hash: Vec<u8>,
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
        r#"SELECT id AS "id!", name AS "name!", code_hash AS "code_hash!",
                  max_sessions AS "max_sessions!", daily_limit, revoked_at
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
    pub status: String,
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
                  r.status AS "status!", m.name AS "requester!",
                  r.created_at AS "created_at!", r.status_changed_at AS "status_changed_at!",
                  (SELECT COUNT(*) FROM request_events e
                   WHERE e.request_id = r.id AND e.kind IN ('submitted', 'duplicate'))
                    AS "times_requested!: i64"
           FROM requests r
           JOIN members m ON m.id = r.requested_by
           WHERE (?1 IS NULL OR r.status = ?1)
             AND (?2 IS NULL OR r.title LIKE ?2 ESCAPE '\' OR m.name LIKE ?2 ESCAPE '\'
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
pub async fn set_request_status(
    pool: &SqlitePool,
    request_id: i64,
    to: &str,
    now: i64,
) -> Result<StatusChange, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    let current = sqlx::query_scalar!(
        r#"SELECT status AS "status!" FROM requests WHERE id = ?"#,
        request_id
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(from) = current else {
        tx.rollback().await?;
        return Ok(StatusChange::NotFound);
    };
    if from == to {
        tx.rollback().await?;
        return Ok(StatusChange::Unchanged);
    }
    sqlx::query!(
        "UPDATE requests SET status = ?, status_changed_at = ? WHERE id = ?",
        to,
        now,
        request_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO request_events (request_id, kind, actor, from_status, to_status, created_at)
         VALUES (?, 'status', 'admin', ?, ?, ?)",
        request_id,
        from,
        to,
        now
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
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
                  status_changed_at AS "status_changed_at!: String"
           FROM (
             SELECT 0 AS sort_key, 0 AS sort_id,
                    'id' AS id, 'tmdb_id' AS tmdb_id, 'type' AS media_type, 'title' AS title,
                    'year' AS year, 'imdb_id' AS imdb_id, 'status' AS status,
                    'requested_by' AS requested_by, 'note' AS note,
                    'times_requested' AS times_requested, 'created_at' AS created_at,
                    'status_changed_at' AS status_changed_at
             UNION ALL
             SELECT 1, r.id, CAST(r.id AS TEXT), CAST(r.tmdb_id AS TEXT), r.media_type, r.title,
                    CAST(r.year AS TEXT), r.imdb_id, r.status, m.name, r.note,
                    CAST((SELECT COUNT(*) FROM request_events e
                          WHERE e.request_id = r.id AND e.kind IN ('submitted', 'duplicate'))
                         AS TEXT),
                    strftime('%Y-%m-%dT%H:%M:%SZ', r.created_at, 'unixepoch'),
                    strftime('%Y-%m-%dT%H:%M:%SZ', r.status_changed_at, 'unixepoch')
             FROM requests r JOIN members m ON m.id = r.requested_by
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
    pub max_sessions: i64,
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
        r#"SELECT m.id AS "id!", m.name AS "name!", m.max_sessions AS "max_sessions!",
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

/// Whether another member already uses `name` (case-insensitively).
/// Names double as event actors, so they are kept unique.
async fn name_taken(
    executor: impl sqlx::SqliteExecutor<'_>,
    name: &str,
    except_id: i64,
) -> Result<bool, sqlx::Error> {
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
    max_sessions: i64,
    daily_limit: Option<i64>,
    now: i64,
) -> Result<MemberWrite, sqlx::Error> {
    let mut tx = begin_immediate(pool).await?;
    if name_taken(&mut *tx, name, 0).await? {
        tx.rollback().await?;
        return Ok(MemberWrite::NameTaken);
    }
    let id = sqlx::query_scalar!(
        r#"INSERT INTO members (name, code_hash, max_sessions, daily_limit, created_at)
           VALUES (?, ?, ?, ?, ?)
           RETURNING id AS "id!""#,
        name,
        code_hash,
        max_sessions,
        daily_limit,
        now
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(MemberWrite::Done(id))
}

/// Fields a member PATCH may change; `None` leaves a field alone, and
/// `daily_limit: Some(None)` resets it to the global default.
pub struct MemberUpdate<'a> {
    pub name: Option<&'a str>,
    pub daily_limit: Option<Option<i64>>,
    pub max_sessions: Option<i64>,
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
    let result = sqlx::query!(
        "UPDATE members
         SET name = COALESCE(?, name),
             max_sessions = COALESCE(?, max_sessions),
             daily_limit = CASE WHEN ? THEN ? ELSE daily_limit END
         WHERE id = ?",
        update.name,
        update.max_sessions,
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
