//! Admin-facing `/api/admin` routes: login and logout, the request queue
//! and archive, the CSV export, and member and device management. Every
//! route but login takes `RequireAdmin`.

use axum::{
    Json,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;

use super::{next_page, page_limit, parse_cursor};
use crate::{
    auth::{self, RequireAdmin},
    clientip::ClientIp,
    db,
    error::{AppError, AppJson, AppPath, AppQuery},
    state::AppState,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminLoginBody {
    username: String,
    password: String,
}

/// `POST /api/admin/login`. Always runs a real Argon2id verification —
/// against the configured hash if the username matches, against a fixed
/// dummy hash otherwise — so login timing never reveals whether the
/// username exists.
pub async fn post_admin_login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    AppJson(body): AppJson<AdminLoginBody>,
) -> Result<Response, AppError> {
    // Checked before queueing for Argon2, so a locked-out or throttled
    // client never costs a hash.
    let _throttle_permit = state.limits.admin_login.before_attempt(ip).await?;

    let username_matches = body.username == state.config.admin_user;
    let hash_to_check =
        auth::admin_password_hash_for(username_matches, &state.config.admin_password_hash);

    let _permit = state
        .admin_login_semaphore
        .acquire()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
    let password_ok = auth::verify_admin_password(&hash_to_check, body.password).await;
    drop(_permit);

    if !username_matches || !password_ok {
        state.limits.admin_login.record_failure(ip);
        tracing::info!(%ip, "admin login rejected");
        return Err(AppError::InvalidAdminCredentials);
    }
    tracing::info!(%ip, "admin login accepted");

    let now = db::now_unix();
    let expires_at = now + auth::ADMIN_SESSION_ABSOLUTE_SECS;
    let (token, token_hash) = auth::generate_session_token();
    db::create_admin_session(&state.pool, &token_hash, now, expires_at).await?;

    let cookie = auth::build_admin_cookie(&token, state.config.cookie_dev_insecure);
    let mut response = axum::Json(serde_json::json!({"ok": true})).into_response();
    response.headers_mut().append(header::SET_COOKIE, cookie);
    Ok(response)
}

/// `POST /api/admin/logout`.
pub async fn post_admin_logout(
    State(state): State<AppState>,
    admin: RequireAdmin,
) -> Result<Response, AppError> {
    db::delete_session(&state.pool, admin.session_id).await?;
    let clear = auth::clear_admin_cookie(state.config.cookie_dev_insecure);
    let mut response = axum::Json(serde_json::json!({"ok": true})).into_response();
    response.headers_mut().append(header::SET_COOKIE, clear);
    Ok(response)
}

fn now(state: &AppState) -> i64 {
    i64::try_from(state.clock.unix_secs()).unwrap_or(i64::MAX)
}

// ---------------------------------------------------------------------
// Request queue and archive
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Added,
    Refused,
    AlreadyHad,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Added => "added",
            Status::Refused => "refused",
            Status::AlreadyHad => "already_had",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusFilter {
    #[default]
    Pending,
    Added,
    Refused,
    AlreadyHad,
    All,
}

impl StatusFilter {
    fn status(self) -> Option<Status> {
        match self {
            StatusFilter::Pending => Some(Status::Pending),
            StatusFilter::Added => Some(Status::Added),
            StatusFilter::Refused => Some(Status::Refused),
            StatusFilter::AlreadyHad => Some(Status::AlreadyHad),
            StatusFilter::All => None,
        }
    }
}

const MAX_FILTER_CHARS: usize = 100;

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default)]
    status: StatusFilter,
    q: Option<String>,
    cursor: Option<String>,
    limit: Option<i64>,
}

/// Escapes `%`, `_` and `\` so user text matches literally inside a
/// `LIKE ... ESCAPE '\'` pattern.
fn like_pattern(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('%');
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

#[derive(Serialize)]
struct AdminRequestView {
    id: i64,
    tmdb_id: i64,
    #[serde(rename = "type")]
    media_type: String,
    title: String,
    year: Option<i64>,
    poster: Option<String>,
    imdb_id: Option<String>,
    dmm_url: Option<String>,
    note: Option<String>,
    status: String,
    requester: String,
    times_requested: i64,
    created_at: i64,
    status_changed_at: i64,
}

/// Debrid Media Manager's title page: `/movie/<imdb_id>` or
/// `/show/<imdb_id>`.
fn dmm_url(base: &str, media_type: &str, imdb_id: Option<&str>) -> Option<String> {
    let kind = if media_type == "tv" { "show" } else { "movie" };
    imdb_id.map(|id| format!("{base}/{kind}/{id}"))
}

/// `GET /api/admin/requests?status=&q=&cursor=&limit=`
pub async fn get_requests(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppQuery(params): AppQuery<ListParams>,
) -> Result<impl IntoResponse, AppError> {
    let before = parse_cursor(params.cursor.as_deref())?;
    let limit = page_limit(params.limit)?;
    let filter = params.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    if filter.is_some_and(|q| q.chars().count() > MAX_FILTER_CHARS) {
        return Err(AppError::BadRequest);
    }
    let like = filter.map(like_pattern);
    let status = params.status.status().map(Status::as_str);

    let mut rows = db::admin_requests(
        &state.pool,
        status,
        like.as_deref(),
        filter,
        before,
        limit + 1,
    )
    .await?;
    let next_cursor = next_page(&mut rows, limit, |r| (r.created_at, r.id));
    let base = &state.config.dmm_base_url;
    let items: Vec<AdminRequestView> = rows
        .into_iter()
        .map(|r| AdminRequestView {
            dmm_url: dmm_url(base, &r.media_type, r.imdb_id.as_deref()),
            id: r.id,
            tmdb_id: r.tmdb_id,
            media_type: r.media_type,
            title: r.title,
            year: r.year,
            poster: r.poster_path,
            imdb_id: r.imdb_id,
            note: r.note,
            status: r.status,
            requester: r.requester,
            times_requested: r.times_requested,
            created_at: r.created_at,
            status_changed_at: r.status_changed_at,
        })
        .collect();
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusBody {
    status: Status,
}

/// `PATCH /api/admin/requests/{id}`: any status to any other, logged as a
/// `status` event in the same transaction. Setting the current status
/// again is a no-op.
pub async fn patch_request(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppPath(id): AppPath<i64>,
    AppJson(body): AppJson<StatusBody>,
) -> Result<impl IntoResponse, AppError> {
    let to = body.status.as_str();
    let now = now(&state);
    let changed = match db::set_request_status(&state.pool, id, to, now).await? {
        db::StatusChange::NotFound => return Err(AppError::NotFound),
        db::StatusChange::Unchanged => false,
        db::StatusChange::Changed { from } => {
            tracing::info!(request_id = id, from = %from, to, "request status changed");
            true
        }
    };
    Ok(Json(json!({ "id": id, "status": to, "changed": changed })))
}

#[derive(Serialize)]
struct EventView {
    id: i64,
    kind: String,
    actor: String,
    from_status: Option<String>,
    to_status: Option<String>,
    note: Option<String>,
    created_at: i64,
}

/// `GET /api/admin/requests/{id}/events`: the full history of one title.
pub async fn get_request_events(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppPath(id): AppPath<i64>,
) -> Result<impl IntoResponse, AppError> {
    let events = db::request_history(&state.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let events: Vec<EventView> = events
        .into_iter()
        .map(|e| EventView {
            id: e.id,
            kind: e.kind,
            actor: e.actor,
            from_status: e.from_status,
            to_status: e.to_status,
            note: e.note,
            created_at: e.created_at,
        })
        .collect();
    Ok(Json(json!({ "events": events })))
}

/// `GET /api/admin/export.csv`: the whole archive, streamed row by row.
pub async fn get_export(State(state): State<AppState>, _admin: RequireAdmin) -> Response {
    let mut response = db::export_csv(state.export_pool).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"media-requests.csv\""),
    );
    response
}

// ---------------------------------------------------------------------
// Members and devices
// ---------------------------------------------------------------------

const MAX_NAME_CHARS: usize = 40;
const MAX_DAILY_LIMIT: i64 = 1000;
const MAX_MAX_SESSIONS: i64 = 20;
/// Attempts at a fresh code if one collides with an existing code's hash.
const CODE_ATTEMPTS: usize = 3;

fn clean_name(name: &str) -> Result<String, AppError> {
    let name = name.trim();
    let len = name.chars().count();
    if len == 0 || len > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(AppError::BadRequest);
    }
    Ok(name.to_string())
}

fn check_daily_limit(limit: Option<i64>) -> Result<Option<i64>, AppError> {
    match limit {
        Some(n) if !(0..=MAX_DAILY_LIMIT).contains(&n) => Err(AppError::BadRequest),
        other => Ok(other),
    }
}

fn check_max_sessions(max: Option<i64>) -> Result<Option<i64>, AppError> {
    match max {
        Some(n) if !(1..=MAX_MAX_SESSIONS).contains(&n) => Err(AppError::BadRequest),
        other => Ok(other),
    }
}

/// A fresh code and its HMAC; only the HMAC is ever stored.
fn new_code(state: &AppState) -> Result<(String, Vec<u8>), AppError> {
    let code = auth::generate_code(state.config.code_length)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("OS random source failed: {e}")))?;
    let hash = auth::hmac_code(&state.config.code_pepper, &auth::normalise_code(&code));
    Ok((code, hash))
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(e) if e.is_unique_violation())
}

#[derive(Serialize)]
struct DeviceView {
    id: i64,
    created_at: i64,
    last_seen_at: i64,
    expires_at: i64,
}

#[derive(Serialize)]
struct MemberView {
    id: i64,
    name: String,
    daily_limit: Option<i64>,
    max_sessions: i64,
    created_at: i64,
    rotated_at: Option<i64>,
    revoked_at: Option<i64>,
    last_seen_at: Option<i64>,
    request_count: i64,
    devices: Vec<DeviceView>,
}

/// `GET /api/admin/members`: members with last seen, live devices and
/// request counts. Codes are never part of it.
pub async fn get_members(
    State(state): State<AppState>,
    _admin: RequireAdmin,
) -> Result<impl IntoResponse, AppError> {
    let members = db::list_members(&state.pool).await?;
    let devices = db::list_devices(&state.pool, now(&state)).await?;
    let members: Vec<MemberView> = members
        .into_iter()
        .map(|m| {
            let mine = devices
                .iter()
                .filter(|d| d.member_id == m.id)
                .map(|d| DeviceView {
                    id: d.id,
                    created_at: d.created_at,
                    last_seen_at: d.last_seen_at,
                    expires_at: d.expires_at,
                })
                .collect();
            MemberView {
                id: m.id,
                name: m.name,
                daily_limit: m.daily_limit,
                max_sessions: m.max_sessions,
                created_at: m.created_at,
                rotated_at: m.rotated_at,
                revoked_at: m.revoked_at,
                last_seen_at: m.last_seen_at,
                request_count: m.request_count,
                devices: mine,
            }
        })
        .collect();
    Ok(Json(json!({
        "members": members,
        "default_daily_limit": state.config.default_daily_limit,
        "default_max_sessions": state.config.default_max_sessions,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMemberBody {
    name: String,
    #[serde(default)]
    daily_limit: Option<i64>,
    #[serde(default)]
    max_sessions: Option<i64>,
}

/// `POST /api/admin/members`: returns the new code. This response and
/// the rotate response are the only places a code ever leaves the server.
pub async fn post_member(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppJson(body): AppJson<CreateMemberBody>,
) -> Result<Response, AppError> {
    let name = clean_name(&body.name)?;
    let daily_limit = check_daily_limit(body.daily_limit)?;
    let max_sessions =
        check_max_sessions(body.max_sessions)?.unwrap_or(state.config.default_max_sessions);
    let now = now(&state);

    for _ in 0..CODE_ATTEMPTS {
        let (code, hash) = new_code(&state)?;
        match db::create_member(&state.pool, &name, &hash, max_sessions, daily_limit, now).await {
            Ok(db::MemberWrite::Done(id)) => {
                tracing::info!(member_id = id, member = %name, "member created");
                let body = json!({
                    "id": id,
                    "name": name,
                    "code": auth::format_code_for_display(&code),
                });
                return Ok((StatusCode::CREATED, Json(body)).into_response());
            }
            Ok(_) => return Err(AppError::Conflict("a member with that name exists")),
            Err(e) if is_unique_violation(&e) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(AppError::Internal(anyhow::anyhow!(
        "could not generate a unique access code"
    )))
}

/// Distinguishes "field absent" (`None`) from "field set to null"
/// (`Some(None)`) in a PATCH body.
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchMemberBody {
    #[serde(default)]
    name: Option<String>,
    /// `null` resets to the global default.
    #[serde(default, deserialize_with = "present")]
    daily_limit: Option<Option<i64>>,
    #[serde(default)]
    max_sessions: Option<i64>,
}

/// `PATCH /api/admin/members/{id}`
pub async fn patch_member(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppPath(id): AppPath<i64>,
    AppJson(body): AppJson<PatchMemberBody>,
) -> Result<impl IntoResponse, AppError> {
    let name = body.name.as_deref().map(clean_name).transpose()?;
    let daily_limit = body.daily_limit.map(check_daily_limit).transpose()?;
    let max_sessions = check_max_sessions(body.max_sessions)?;
    let update = db::MemberUpdate {
        name: name.as_deref(),
        daily_limit,
        max_sessions,
    };
    match db::update_member(&state.pool, id, &update).await? {
        db::MemberWrite::Done(_) => {
            tracing::info!(member_id = id, "member updated");
            Ok(Json(json!({ "ok": true })))
        }
        db::MemberWrite::NotFound => Err(AppError::NotFound),
        _ => Err(AppError::Conflict("a member with that name exists")),
    }
}

/// `POST /api/admin/members/{id}/rotate`: a new code, shown once; the old
/// code and every device using it stop working immediately.
pub async fn post_rotate(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppPath(id): AppPath<i64>,
) -> Result<impl IntoResponse, AppError> {
    let now = now(&state);
    for _ in 0..CODE_ATTEMPTS {
        let (code, hash) = new_code(&state)?;
        match db::rotate_member_code(&state.pool, id, &hash, now).await {
            Ok(db::MemberWrite::Done(_)) => {
                tracing::info!(member_id = id, "member code rotated; sessions deleted");
                return Ok(Json(
                    json!({ "code": auth::format_code_for_display(&code) }),
                ));
            }
            Ok(db::MemberWrite::NotFound) => return Err(AppError::NotFound),
            Ok(_) => return Err(AppError::Conflict("member is revoked")),
            Err(e) if is_unique_violation(&e) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(AppError::Internal(anyhow::anyhow!(
        "could not generate a unique access code"
    )))
}

/// `POST /api/admin/members/{id}/revoke`
pub async fn post_revoke(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppPath(id): AppPath<i64>,
) -> Result<impl IntoResponse, AppError> {
    match db::revoke_member(&state.pool, id, now(&state)).await? {
        db::MemberWrite::Done(_) => {
            tracing::info!(member_id = id, "member revoked; sessions deleted");
            Ok(Json(json!({ "ok": true })))
        }
        _ => Err(AppError::NotFound),
    }
}

/// `DELETE /api/admin/sessions/{id}`: logs one member device out.
pub async fn delete_session(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppPath(id): AppPath<i64>,
) -> Result<impl IntoResponse, AppError> {
    if !db::delete_user_session(&state.pool, id).await? {
        return Err(AppError::NotFound);
    }
    tracing::info!(session_id = id, "member device logged out by admin");
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("dune"), "%dune%");
        assert_eq!(like_pattern("50%_off\\"), "%50\\%\\_off\\\\%");
    }

    #[test]
    fn dmm_urls_follow_the_movie_and_show_paths() {
        let base = "https://debridmediamanager.com";
        assert_eq!(
            dmm_url(base, "movie", Some("tt1160419")).as_deref(),
            Some("https://debridmediamanager.com/movie/tt1160419")
        );
        assert_eq!(
            dmm_url(base, "tv", Some("tt0944947")).as_deref(),
            Some("https://debridmediamanager.com/show/tt0944947")
        );
        assert_eq!(dmm_url(base, "movie", None), None);
    }

    #[test]
    fn member_fields_are_validated() {
        assert_eq!(clean_name("  Alice ").unwrap(), "Alice");
        assert!(clean_name("   ").is_err());
        assert!(clean_name(&"x".repeat(41)).is_err());
        assert!(clean_name("a\nb").is_err());
        assert!(check_daily_limit(Some(-1)).is_err());
        assert!(check_daily_limit(Some(0)).is_ok());
        assert!(check_max_sessions(Some(0)).is_err());
        assert!(check_max_sessions(None).unwrap().is_none());
    }
}
