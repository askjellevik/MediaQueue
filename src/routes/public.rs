//! User-facing `/api` routes: access-code login, logout, `/api/me`, search
//! and requests.

use axum::{
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::{
    auth::{self, RequireUser},
    clientip::ClientIp,
    config, db,
    error::{AppError, AppJson, AppPath, AppQuery},
    events::Tick,
    notify::{Message, NewRequestInfo},
    state::AppState,
    tmdb::{self, MediaType, SearchItem, SearchType},
};

use super::{next_page, page_limit, parse_cursor};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeBody {
    code: String,
}

#[derive(Serialize)]
struct NameBody {
    name: String,
}

/// `POST /api/auth/code`: exchanges an access code for a user session.
/// Wrong, rotated and revoked codes all take the same path to the same
/// `AppError::InvalidCode` response — a rotated code's hash simply matches
/// no row, and a revoked member is rejected only after an identical lookup.
pub async fn post_auth_code(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    AppJson(body): AppJson<CodeBody>,
) -> Result<Response, AppError> {
    // Locked-out IPs stop here; under the global throttle this waits first
    // and holds a delay permit until the code has been checked.
    let _throttle_permit = state.limits.code_login.before_attempt(ip).await?;

    let normalised = auth::normalise_code(&body.code);
    let code_hash = auth::hmac_code(&state.config.code_pepper, &normalised);

    let member = db::find_member_by_code_hash(&state.pool, &code_hash).await?;
    // The indexed lookup finds the row; the constant-time compare is what
    // decides, so the decision never depends on SQLite's comparison.
    let member = match member {
        Some(member)
            if auth::constant_time_eq(&member.code_hash, &code_hash)
                && member.revoked_at.is_none() =>
        {
            member
        }
        _ => {
            state.limits.code_login.record_failure(ip);
            tracing::info!(%ip, "access code rejected");
            return Err(AppError::InvalidCode);
        }
    };
    tracing::info!(%ip, member = %member.name, "access code accepted");

    let now = db::now_unix();
    let expires_at = now + auth::USER_SESSION_LIFETIME_SECS;
    let (token, token_hash) = auth::generate_session_token();

    let result = db::create_user_session(
        &state.pool,
        member.id,
        config::MAX_SESSIONS_PER_MEMBER,
        &token_hash,
        now,
        expires_at,
    )
    .await?;

    if let Some(evicted_session_id) = result.evicted_session_id {
        tracing::warn!(
            member_id = member.id,
            evicted_session_id,
            "member reached the device cap; evicted oldest session"
        );
    }

    let cookie = auth::build_user_cookie(&token, state.config.cookie_dev_insecure);
    let mut response = axum::Json(NameBody { name: member.name }).into_response();
    response.headers_mut().append(header::SET_COOKIE, cookie);
    Ok(response)
}

/// `POST /api/auth/logout`: deletes the current session and clears the cookie.
pub async fn post_auth_logout(
    State(state): State<AppState>,
    user: RequireUser,
) -> Result<Response, AppError> {
    db::delete_session(&state.pool, user.session_id).await?;
    let clear = auth::clear_user_cookie(state.config.cookie_dev_insecure);
    let mut response = axum::Json(serde_json::json!({"ok": true})).into_response();
    response.headers_mut().append(header::SET_COOKIE, clear);
    Ok(response)
}

#[derive(Serialize)]
struct MeBody {
    name: String,
    /// `None` when the member has no daily limit.
    remaining_today: Option<i64>,
}

/// `GET /api/events`: a server-sent tick whenever a request's status changes.
pub async fn get_events(
    State(state): State<AppState>,
    user: RequireUser,
) -> Result<impl IntoResponse, AppError> {
    let pool = state.pool.clone();
    let session_id = user.session_id;
    state.events.open(Tick::Members, move || {
        let pool = pool.clone();
        async move {
            // A database hiccup is not a reason to drop everyone.
            db::session_alive(&pool, session_id, db::now_unix(), None)
                .await
                .unwrap_or(true)
        }
    })
}

/// `GET /api/me`: identifies the caller, or 401s so the frontend shows the
/// access-code gate.
pub async fn get_me(
    State(state): State<AppState>,
    user: RequireUser,
) -> Result<impl IntoResponse, AppError> {
    let quota = Quota::for_user(&state, &user);
    let usage = db::quota_usage(
        &state.pool,
        user.member_id,
        &user.member_name,
        quota.day_start,
        quota.minute_start,
    )
    .await?;
    let remaining_today = quota.remaining_today(&usage);

    Ok(axum::Json(MeBody {
        name: user.member_name,
        remaining_today,
    }))
}

const MIN_QUERY_CHARS: usize = 2;
const MAX_QUERY_CHARS: usize = 80;

#[derive(Deserialize)]
pub struct SearchParams {
    q: String,
    #[serde(default, rename = "type")]
    search_type: SearchType,
}

/// One result as the browser sees it.
#[derive(Serialize)]
struct SearchResult {
    tmdb_id: u32,
    #[serde(rename = "type")]
    media_type: MediaType,
    title: String,
    year: Option<i32>,
    poster: Option<String>,
    overview: String,
    states: VariantStates,
}

/// How a title stands for each Infuse PRO answer; `"none"` if that variant
/// has not been requested.
#[derive(Serialize)]
struct VariantStates {
    plain: &'static str,
    pro: &'static str,
}

#[derive(Serialize)]
struct SearchBody {
    results: Vec<SearchResult>,
}

/// Maps a request's status to the chip the search dropdown shows.
fn state_for_status(status: &str) -> &'static str {
    match status {
        "pending" => "requested",
        "added" => "added",
        "already_had" => "have",
        "refused" => "unavailable",
        _ => "requested",
    }
}

/// `GET /api/search?q=&type=`: up to 8 movie/TV results from the TMDB
/// cache, each annotated with whether it has been requested already.
pub async fn get_search(
    State(state): State<AppState>,
    user: RequireUser,
    AppQuery(params): AppQuery<SearchParams>,
) -> Result<impl IntoResponse, AppError> {
    // Limited before validation, so malformed queries cost budget too.
    state.limits.check_search(user.member_id)?;

    let query = tmdb::normalise_query(&params.q);
    let chars = query.chars().count();
    if !(MIN_QUERY_CHARS..=MAX_QUERY_CHARS).contains(&chars) {
        return Err(AppError::BadRequest);
    }

    let items = state.tmdb.search(&query, params.search_type).await?;

    let ids: Vec<u32> = items.iter().map(|i| i.tmdb_id).collect();
    let rows = db::request_states(&state.pool, &ids).await?;
    let state_of = |item: &SearchItem, infuse_pro: bool| {
        rows.iter()
            .find(|r| {
                r.tmdb_id == i64::from(item.tmdb_id)
                    && r.media_type == item.media_type.as_str()
                    && r.infuse_pro == infuse_pro
            })
            .map(|r| state_for_status(&r.status))
            .unwrap_or("none")
    };

    let results = items
        .iter()
        .map(|item| SearchResult {
            tmdb_id: item.tmdb_id,
            media_type: item.media_type,
            title: item.title.clone(),
            year: item.year,
            poster: item.poster_path.clone(),
            overview: item.overview.clone(),
            states: VariantStates {
                plain: state_of(item, false),
                pro: state_of(item, true),
            },
        })
        .collect();
    Ok(axum::Json(SearchBody { results }))
}

// ---------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------

const MAX_NOTE_CHARS: usize = 200;
const SECS_PER_DAY: i64 = 86_400;
const MINUTE_SECS: i64 = 60;

/// A member's request quota at one instant: the daily cap (only if the admin
/// set one) and the per-minute cap.
struct Quota {
    now: i64,
    daily_limit: Option<i64>,
    per_minute: i64,
    /// Start of the current UTC day; "today" for the daily cap.
    day_start: i64,
    /// Events after this count toward the per-minute cap.
    minute_start: i64,
}

impl Quota {
    fn for_user(state: &AppState, user: &RequireUser) -> Self {
        let now = i64::try_from(state.clock.unix_secs()).unwrap_or(i64::MAX);
        Self {
            now,
            daily_limit: user.daily_limit,
            per_minute: i64::from(state.config.rate_limits.requests_per_minute),
            day_start: now - now.rem_euclid(SECS_PER_DAY),
            minute_start: now - MINUTE_SECS,
        }
    }

    fn remaining_today(&self, usage: &db::QuotaUsage) -> Option<i64> {
        self.daily_limit.map(|limit| (limit - usage.today).max(0))
    }

    /// `None` if another request is allowed now, else seconds to wait.
    fn retry_after(&self, usage: &db::QuotaUsage) -> Option<u64> {
        let over_day = self.daily_limit.is_some_and(|limit| usage.today >= limit);
        let over_minute = usage.last_minute >= self.per_minute;
        if !over_day && !over_minute {
            return None;
        }
        let mut wait = 1;
        if over_day {
            wait = wait.max(self.day_start + SECS_PER_DAY - self.now);
        }
        if over_minute {
            let oldest = usage.oldest_in_minute.unwrap_or(self.now);
            wait = wait.max(oldest + MINUTE_SECS - self.now);
        }
        Some(u64::try_from(wait).unwrap_or(1))
    }

    fn exceeded(&self, usage: &db::QuotaUsage) -> AppError {
        AppError::QuotaExceeded {
            retry_after_secs: self.retry_after(usage).unwrap_or(1),
            remaining_today: self.remaining_today(usage),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequestBody {
    /// `NonZeroU32` rejects 0, negatives and anything past `u32` while
    /// parsing, so a bad ID never reaches TMDB.
    tmdb_id: std::num::NonZeroU32,
    media_type: MediaType,
    #[serde(default)]
    note: Option<String>,
    /// Whether the requester has Infuse PRO. A title has one request per
    /// answer, so PRO and non-PRO members each get a fitting release.
    #[serde(default)]
    infuse_pro: bool,
}

/// Strips control characters and surrounding whitespace; an empty note is
/// no note. `Err` if what remains is longer than 200 characters.
fn clean_note(note: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(note) = note else {
        return Ok(None);
    };
    let cleaned: String = note.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.chars().count() > MAX_NOTE_CHARS {
        return Err(AppError::BadRequest);
    }
    Ok((!cleaned.is_empty()).then(|| cleaned.to_string()))
}

#[derive(Serialize)]
struct RequestView {
    id: i64,
    tmdb_id: i64,
    #[serde(rename = "type")]
    media_type: String,
    imdb_id: Option<String>,
    title: String,
    year: Option<i64>,
    poster: Option<String>,
    status: String,
    created_at: i64,
    infuse_pro: bool,
}

impl From<db::RequestRow> for RequestView {
    fn from(row: db::RequestRow) -> Self {
        Self {
            id: row.id,
            tmdb_id: row.tmdb_id,
            media_type: row.media_type,
            imdb_id: row.imdb_id,
            title: row.title,
            year: row.year,
            poster: row.poster_path,
            status: row.status,
            created_at: row.created_at,
            infuse_pro: row.infuse_pro,
        }
    }
}

#[derive(Serialize)]
struct CreatedBody {
    /// `false` when the title was already queued and only a duplicate
    /// event was logged.
    created: bool,
    request: RequestView,
    remaining_today: Option<i64>,
}

/// `POST /api/requests`: queues a title, or logs a duplicate if someone
/// already asked for it. 201 for new, 200 for duplicate, 429 over quota.
pub async fn post_request(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    user: RequireUser,
    AppJson(body): AppJson<CreateRequestBody>,
) -> Result<Response, AppError> {
    let note = clean_note(body.note.as_deref())?;
    let quota = Quota::for_user(&state, &user);

    // Cheap early answer before any TMDB work; re-checked under the write
    // lock below.
    let usage = db::quota_usage(
        &state.pool,
        user.member_id,
        &user.member_name,
        quota.day_start,
        quota.minute_start,
    )
    .await?;
    if quota.retry_after(&usage).is_some() {
        tracing::info!(%ip, member = %user.member_name, "request quota hit");
        return Err(quota.exceeded(&usage));
    }

    let tmdb_id = body.tmdb_id.get();
    let detail = state.tmdb.detail(tmdb_id, body.media_type).await?;

    let new = db::NewRequest {
        tmdb_id: i64::from(tmdb_id),
        media_type: body.media_type.as_str(),
        imdb_id: detail.imdb_id.as_deref(),
        title: &detail.title,
        year: detail.year.map(i64::from),
        poster_path: detail.poster_path.as_deref(),
        note: note.as_deref(),
        infuse_pro: body.infuse_pro,
        member_id: user.member_id,
        member_name: &user.member_name,
        now: quota.now,
    };
    let outcome = db::create_request(
        &state.pool,
        &new,
        quota.day_start,
        quota.minute_start,
        |usage| quota.retry_after(usage).is_none(),
    )
    .await
    // The member was deleted after this session was checked.
    .map_err(|e| match &e {
        sqlx::Error::Database(d) if d.is_foreign_key_violation() => AppError::Unauthorized,
        _ => AppError::from(e),
    })?;

    let (status, created, row) = match outcome {
        db::CreateOutcome::OverQuota(usage) => {
            tracing::info!(%ip, member = %user.member_name, "request quota hit");
            return Err(quota.exceeded(&usage));
        }
        db::CreateOutcome::Created(row) => (StatusCode::CREATED, true, row),
        db::CreateOutcome::Duplicate(row) => (StatusCode::OK, false, row),
    };
    tracing::info!(
        %ip,
        member = %user.member_name,
        request_id = row.id,
        tmdb_id,
        media_type = body.media_type.as_str(),
        duplicate = !created,
        "request submitted"
    );

    if created {
        let info = NewRequestInfo {
            title: &row.title,
            year: row.year,
            is_movie: body.media_type == MediaType::Movie,
            requester: &user.member_name,
            note: note.as_deref(),
            infuse_pro: row.infuse_pro,
        };
        state
            .notifier
            .send(Message::new_request(&info, &state.config.public_origin));
    }

    state.events.publish(Tick::Admin);

    // Counted after the write, so a new request shows one fewer.
    let today = usage.today + i64::from(created);
    let body = CreatedBody {
        created,
        request: row.into(),
        remaining_today: quota.daily_limit.map(|limit| (limit - today).max(0)),
    };
    Ok((status, axum::Json(body)).into_response())
}

/// `DELETE /api/requests/{id}`: the requester withdraws a request that is
/// still pending and that nobody else duplicated.
pub async fn delete_request(
    State(state): State<AppState>,
    user: RequireUser,
    AppPath(id): AppPath<i64>,
) -> Result<impl IntoResponse, AppError> {
    match db::cancel_request(&state.pool, id, user.member_id).await? {
        db::CancelOutcome::Cancelled => {
            tracing::info!(member = %user.member_name, request_id = id, "request cancelled");
            state.events.publish(Tick::Admin);
            state.events.publish(Tick::Members);
            Ok(axum::Json(serde_json::json!({ "ok": true })))
        }
        db::CancelOutcome::NotFound => Err(AppError::NotFound),
        db::CancelOutcome::Locked => Err(AppError::Conflict(
            "denne bestillingen kan ikke lenger trekkes tilbake",
        )),
    }
}

#[derive(Deserialize)]
pub struct PageParams {
    cursor: Option<String>,
    limit: Option<i64>,
    /// `pending` or `earlier` (everything else); absent means all.
    status: Option<String>,
}

#[derive(Serialize)]
struct MineView {
    #[serde(flatten)]
    request: RequestView,
    /// Theirs, still pending, and nobody else asked for it.
    can_cancel: bool,
}

#[derive(Serialize)]
struct MineBody {
    items: Vec<MineView>,
    next_cursor: Option<String>,
}

/// `GET /api/requests/mine?cursor=&limit=&status=`: the caller's requests
/// with their current status, newest first.
pub async fn get_requests_mine(
    State(state): State<AppState>,
    user: RequireUser,
    AppQuery(params): AppQuery<PageParams>,
) -> Result<impl IntoResponse, AppError> {
    let before = parse_cursor(params.cursor.as_deref())?;
    let limit = page_limit(params.limit)?;
    let filter = match params.status.as_deref() {
        None | Some("") => db::MineFilter::All,
        Some("pending") => db::MineFilter::Pending,
        Some("earlier") => db::MineFilter::Earlier,
        Some(_) => return Err(AppError::BadRequest),
    };
    // One extra row tells us whether another page exists.
    let mut rows = db::requests_for_member(
        &state.pool,
        user.member_id,
        &user.member_name,
        filter,
        before,
        limit + 1,
    )
    .await?;
    let next_cursor = next_page(&mut rows, limit, |r| (r.created_at, r.id));
    Ok(axum::Json(MineBody {
        items: rows
            .into_iter()
            .map(|r| MineView {
                can_cancel: r.can_cancel,
                request: RequestView::from(db::RequestRow {
                    id: r.id,
                    tmdb_id: r.tmdb_id,
                    media_type: r.media_type,
                    imdb_id: r.imdb_id,
                    title: r.title,
                    year: r.year,
                    poster_path: r.poster_path,
                    status: r.status,
                    created_at: r.created_at,
                    infuse_pro: r.infuse_pro,
                }),
            })
            .collect(),
        next_cursor,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quota(now: i64) -> Quota {
        Quota {
            now,
            daily_limit: Some(10),
            per_minute: 3,
            day_start: now - now.rem_euclid(SECS_PER_DAY),
            minute_start: now - MINUTE_SECS,
        }
    }

    fn usage(today: i64, last_minute: i64, oldest: Option<i64>) -> db::QuotaUsage {
        db::QuotaUsage {
            today,
            last_minute,
            oldest_in_minute: oldest,
        }
    }

    #[test]
    fn quota_allows_until_a_cap_is_reached() {
        let q = quota(1_000_000);
        assert_eq!(q.retry_after(&usage(0, 0, None)), None);
        assert_eq!(q.retry_after(&usage(9, 2, Some(999_990))), None);
    }

    #[test]
    fn minute_cap_waits_for_the_oldest_event_to_age_out() {
        let q = quota(1_000_000);
        assert_eq!(q.retry_after(&usage(3, 3, Some(999_950))), Some(10));
    }

    #[test]
    fn daily_cap_waits_until_utc_midnight() {
        let now = 86_400 * 100 + 86_000;
        let q = quota(now);
        assert_eq!(q.retry_after(&usage(10, 0, None)), Some(400));
        assert_eq!(q.remaining_today(&usage(10, 0, None)), Some(0));
        assert_eq!(q.remaining_today(&usage(12, 0, None)), Some(0));
    }

    #[test]
    fn no_daily_limit_never_caps_or_counts_down() {
        let mut q = quota(1_000_000);
        q.daily_limit = None;
        assert_eq!(q.retry_after(&usage(500, 0, None)), None);
        assert_eq!(q.remaining_today(&usage(500, 0, None)), None);
        // The per-minute cap still applies.
        assert_eq!(q.retry_after(&usage(500, 3, Some(999_950))), Some(10));
    }

    #[test]
    fn note_is_cleaned_and_capped() {
        assert_eq!(clean_note(None).unwrap(), None);
        assert_eq!(clean_note(Some("  \n ")).unwrap(), None);
        assert_eq!(
            clean_note(Some(" 4K\u{0007} please\n")).unwrap().as_deref(),
            Some("4K please")
        );
        assert!(clean_note(Some(&"x".repeat(200))).unwrap().is_some());
        assert!(clean_note(Some(&"x".repeat(201))).is_err());
        assert!(clean_note(Some(&"æ".repeat(200))).unwrap().is_some());
    }
}
