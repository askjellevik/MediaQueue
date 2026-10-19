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

use super::{next_page, page_limit, parse_cursor, public::clean_text};
use crate::{
    auth::{self, RequireAdmin},
    clientip::ClientIp,
    db,
    error::{AppError, AppJson, AppPath, AppQuery},
    events::Tick,
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

pub(super) fn now(state: &AppState) -> i64 {
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
    infuse_pro: bool,
    status: String,
    reason: Option<String>,
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
            infuse_pro: r.infuse_pro,
            status: r.status,
            reason: r.reject_reason,
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
    /// Why it was refused; only kept with `refused`.
    reason: Option<String>,
}

/// Longest reason an admin can give for refusing a request.
const MAX_REASON_CHARS: usize = 300;

/// `PATCH /api/admin/requests/{id}`: any status to any other, logged as a
/// `status` event in the same transaction. Setting the current status
/// again is a no-op, except that `refused` with a different reason
/// replaces the reason.
pub async fn patch_request(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppPath(id): AppPath<i64>,
    AppJson(body): AppJson<StatusBody>,
) -> Result<impl IntoResponse, AppError> {
    let to = body.status.as_str();
    let reason = clean_text(body.reason.as_deref(), MAX_REASON_CHARS)?;
    let now = now(&state);
    let changed = match db::set_request_status(&state.pool, id, to, reason.as_deref(), now).await? {
        db::StatusChange::NotFound => return Err(AppError::NotFound),
        db::StatusChange::Unchanged => false,
        db::StatusChange::Changed { from } => {
            tracing::info!(request_id = id, from = %from, to, "request status changed");
            true
        }
    };
    if changed {
        state.events.publish(Tick::Members);
        state.events.publish(Tick::Admin);
    }
    Ok(Json(json!({ "id": id, "status": to, "changed": changed })))
}

/// `DELETE /api/admin/requests/{id}`: removes the request and its history,
/// whatever its status. It disappears from the requester's list too.
pub async fn delete_request(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppPath(id): AppPath<i64>,
) -> Result<impl IntoResponse, AppError> {
    if !db::delete_request(&state.pool, id).await? {
        return Err(AppError::NotFound);
    }
    tracing::info!(request_id = id, "request deleted by admin");
    state.events.publish(Tick::Members);
    state.events.publish(Tick::Admin);
    Ok(Json(json!({ "ok": true })))
}

/// `GET /api/admin/events`: a server-sent tick whenever requests change.
pub async fn get_events(
    State(state): State<AppState>,
    admin: RequireAdmin,
) -> Result<impl IntoResponse, AppError> {
    let pool = state.pool.clone();
    let session_id = admin.session_id;
    state.events.open(Tick::Admin, move || {
        let pool = pool.clone();
        async move {
            // A database hiccup is not a reason to drop everyone.
            db::session_alive(
                &pool,
                session_id,
                db::now_unix(),
                Some(auth::ADMIN_SESSION_IDLE_SECS),
            )
            .await
            .unwrap_or(true)
        }
    })
}

/// `GET /api/admin/counts`: requests per status, for the tab badges.
pub async fn get_counts(
    State(state): State<AppState>,
    _admin: RequireAdmin,
) -> Result<impl IntoResponse, AppError> {
    let mut counts = serde_json::Map::new();
    for status in ["pending", "added", "already_had", "refused"] {
        counts.insert(status.to_string(), json!(0));
    }
    for (status, n) in db::status_counts(&state.pool).await? {
        counts.insert(status, json!(n));
    }
    Ok(Json(counts))
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
        HeaderValue::from_static("attachment; filename=\"mediaqueue.csv\""),
    );
    response
}

// ---------------------------------------------------------------------
// Members and devices
// ---------------------------------------------------------------------

const MAX_NAME_CHARS: usize = 40;
const MAX_DAILY_LIMIT: i64 = 1000;
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

/// A fresh code and its HMAC; only the HMAC is ever stored.
fn new_code(state: &AppState) -> Result<(String, Vec<u8>), AppError> {
    let code = auth::generate_code(auth::GENERATED_CODE_LENGTH)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("OS random source failed: {e}")))?;
    let hash = auth::hmac_code(&state.config.code_pepper, &auth::normalise_code(&code));
    Ok((code, hash))
}

/// The HMAC of an admin-chosen code, or `None` when the field is absent or
/// blank (meaning "generate one"). A code that breaks the rules is a 400.
fn chosen_code_hash(state: &AppState, code: Option<&str>) -> Result<Option<Vec<u8>>, AppError> {
    let Some(code) = code.filter(|c| !c.trim().is_empty()) else {
        return Ok(None);
    };
    let normalised = auth::check_custom_code(code).ok_or(AppError::BadRequest)?;
    Ok(Some(auth::hmac_code(
        &state.config.code_pepper,
        &normalised,
    )))
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
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMemberBody {
    name: String,
    #[serde(default)]
    daily_limit: Option<i64>,
    /// An admin-chosen code; blank or absent means "generate one".
    #[serde(default)]
    code: Option<String>,
}

/// `POST /api/admin/members`: returns the new code when one was generated.
/// This response and the rotate response are the only places a code ever
/// leaves the server; a code the admin chose is never echoed back.
pub async fn post_member(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppJson(body): AppJson<CreateMemberBody>,
) -> Result<Response, AppError> {
    let name = clean_name(&body.name)?;
    let daily_limit = check_daily_limit(body.daily_limit)?;
    let now = now(&state);

    if let Some(hash) = chosen_code_hash(&state, body.code.as_deref())? {
        return match db::create_member(&state.pool, &name, &hash, daily_limit, now).await {
            Ok(db::MemberWrite::Done(id)) => {
                tracing::info!(member_id = id, member = %name, "member created with chosen code");
                let body = json!({ "id": id, "name": name, "code": null });
                Ok((StatusCode::CREATED, Json(body)).into_response())
            }
            Ok(_) => Err(AppError::Conflict(
                "en bruker med det navnet finnes allerede",
            )),
            Err(e) if is_unique_violation(&e) => {
                Err(AppError::Conflict("den koden er allerede i bruk"))
            }
            Err(e) => Err(e.into()),
        };
    }

    for _ in 0..CODE_ATTEMPTS {
        let (code, hash) = new_code(&state)?;
        match db::create_member(&state.pool, &name, &hash, daily_limit, now).await {
            Ok(db::MemberWrite::Done(id)) => {
                tracing::info!(member_id = id, member = %name, "member created");
                let body = json!({
                    "id": id,
                    "name": name,
                    "code": auth::format_code_for_display(&code),
                });
                return Ok((StatusCode::CREATED, Json(body)).into_response());
            }
            Ok(_) => {
                return Err(AppError::Conflict(
                    "en bruker med det navnet finnes allerede",
                ));
            }
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
    /// `null` removes the member's daily limit.
    #[serde(default, deserialize_with = "present")]
    daily_limit: Option<Option<i64>>,
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
    let update = db::MemberUpdate {
        name: name.as_deref(),
        daily_limit,
    };
    match db::update_member(&state.pool, id, &update).await? {
        db::MemberWrite::Done(_) => {
            tracing::info!(member_id = id, "member updated");
            Ok(Json(json!({ "ok": true })))
        }
        db::MemberWrite::NotFound => Err(AppError::NotFound),
        _ => Err(AppError::Conflict(
            "en bruker med det navnet finnes allerede",
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RotateBody {
    /// An admin-chosen code; blank or absent means "generate one".
    #[serde(default)]
    code: Option<String>,
}

/// `POST /api/admin/members/{id}/rotate`: a new code (shown once if it was
/// generated); the old code and every device using it stop working
/// immediately.
pub async fn post_rotate(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppPath(id): AppPath<i64>,
    AppJson(body): AppJson<RotateBody>,
) -> Result<impl IntoResponse, AppError> {
    let now = now(&state);
    if let Some(hash) = chosen_code_hash(&state, body.code.as_deref())? {
        return match db::rotate_member_code(&state.pool, id, &hash, now).await {
            Ok(db::MemberWrite::Done(_)) => {
                tracing::info!(
                    member_id = id,
                    "member code rotated to chosen code; sessions deleted"
                );
                Ok(Json(json!({ "code": null })))
            }
            Ok(db::MemberWrite::NotFound) => Err(AppError::NotFound),
            Ok(_) => Err(AppError::Conflict("brukeren er deaktivert")),
            Err(e) if is_unique_violation(&e) => {
                Err(AppError::Conflict("den koden er allerede i bruk"))
            }
            Err(e) => Err(e.into()),
        };
    }
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
            Ok(_) => return Err(AppError::Conflict("brukeren er deaktivert")),
            Err(e) if is_unique_violation(&e) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(AppError::Internal(anyhow::anyhow!(
        "could not generate a unique access code"
    )))
}

/// `POST /api/admin/members/{id}/revoke`: blocks the member until restored.
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

/// `POST /api/admin/members/{id}/restore`: lifts a revoke; the old code
/// works again.
pub async fn post_restore(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppPath(id): AppPath<i64>,
) -> Result<impl IntoResponse, AppError> {
    match db::restore_member(&state.pool, id).await? {
        db::MemberWrite::Done(_) => {
            tracing::info!(member_id = id, "member restored");
            Ok(Json(json!({ "ok": true })))
        }
        _ => Err(AppError::NotFound),
    }
}

/// `DELETE /api/admin/members/{id}`: removes a revoked member. Their
/// requests stay in the queue under their name.
pub async fn delete_member(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppPath(id): AppPath<i64>,
) -> Result<impl IntoResponse, AppError> {
    match db::delete_member(&state.pool, id).await? {
        db::MemberDelete::Deleted => {
            tracing::info!(member_id = id, "member deleted; requests kept");
            Ok(Json(json!({ "ok": true })))
        }
        db::MemberDelete::NotFound => Err(AppError::NotFound),
        db::MemberDelete::NotRevoked => Err(AppError::Conflict("deaktiver brukeren først")),
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
    }
}

// ---------------------------------------------------------------------
// Fetch jobs and the worker switch
// ---------------------------------------------------------------------

/// `POST /api/admin/requests/{id}/fetch`: hands a pending request to the
/// worker.
pub async fn post_fetch(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppPath(id): AppPath<i64>,
) -> Result<impl IntoResponse, AppError> {
    match db::create_fetch_job(&state.pool, id, now(&state)).await? {
        db::CreateJob::NotFound => Err(AppError::NotFound),
        db::CreateJob::Exists => Err(AppError::Conflict("forespørselen har allerede en jobb")),
        db::CreateJob::NotPending => Err(AppError::Conflict(
            "bare ventende forespørsler kan hentes automatisk",
        )),
        db::CreateJob::Created {
            id: job_id,
            state: job_state,
            reason,
        } => {
            tracing::info!(request_id = id, job_id, state = %job_state, "fetch job created");
            state.events.publish(Tick::Admin);
            Ok((
                StatusCode::CREATED,
                Json(json!({ "job": { "id": job_id, "state": job_state, "reason": reason } })),
            ))
        }
    }
}

/// `POST /api/admin/jobs/{id}/retry`: requeues a job, as long as its
/// request is still pending and has an IMDb ID.
pub async fn post_job_retry(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppPath(id): AppPath<i64>,
) -> Result<impl IntoResponse, AppError> {
    match db::retry_fetch_job(&state.pool, id, now(&state)).await? {
        db::RetryOutcome::NotFound => Err(AppError::NotFound),
        db::RetryOutcome::WrongState => Err(AppError::Conflict(
            "jobben kan ikke kjøres på nytt nå (forespørselen må være ventende og ha IMDb-ID)",
        )),
        db::RetryOutcome::Retried => {
            tracing::info!(job_id = id, state = "queued", "fetch job retried");
            state.events.publish(Tick::Admin);
            Ok(Json(json!({ "id": id, "state": "queued" })))
        }
    }
}

/// `POST /api/admin/jobs/{id}/cancel`: ends a job as failed / cancelled.
pub async fn post_job_cancel(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppPath(id): AppPath<i64>,
) -> Result<impl IntoResponse, AppError> {
    match db::cancel_fetch_job(&state.pool, id, now(&state)).await? {
        db::CancelJobOutcome::NotFound => Err(AppError::NotFound),
        db::CancelJobOutcome::WrongState => Err(AppError::Conflict(
            "jobben kan ikke avbrytes i denne tilstanden",
        )),
        db::CancelJobOutcome::Cancelled => {
            tracing::info!(job_id = id, state = "failed", "fetch job cancelled");
            state.events.publish(Tick::Admin);
            Ok(Json(json!({ "id": id, "state": "failed" })))
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStateFilter {
    #[default]
    All,
    Queued,
    Fetching,
    NeedsPick,
    Failed,
    Done,
}

impl JobStateFilter {
    fn state(self) -> Option<&'static str> {
        match self {
            JobStateFilter::All => None,
            JobStateFilter::Queued => Some("queued"),
            JobStateFilter::Fetching => Some("fetching"),
            JobStateFilter::NeedsPick => Some("needs_pick"),
            JobStateFilter::Failed => Some("failed"),
            JobStateFilter::Done => Some("done"),
        }
    }
}

#[derive(Deserialize)]
pub struct JobListParams {
    #[serde(default)]
    state: JobStateFilter,
    cursor: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct JobView {
    id: i64,
    request_id: i64,
    state: String,
    reason: Option<String>,
    infuse_pro: bool,
    attempts: i64,
    top_infohash: Option<String>,
    chosen_infohash: Option<String>,
    picked: bool,
    /// The worker's shortlist as stored; `null` if it does not parse.
    candidates: Option<serde_json::Value>,
    rd_torrent_id: Option<String>,
    last_error: Option<String>,
    created_at: i64,
    updated_at: i64,
    title: String,
    year: Option<i64>,
    #[serde(rename = "type")]
    media_type: String,
}

/// `GET /api/admin/jobs?state=&limit=&cursor=`. `candidates` and
/// `last_error` are worker-supplied text: render them with `textContent`.
pub async fn get_jobs(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppQuery(params): AppQuery<JobListParams>,
) -> Result<impl IntoResponse, AppError> {
    let before = parse_cursor(params.cursor.as_deref())?;
    let limit = page_limit(params.limit)?;
    let mut rows =
        db::list_fetch_jobs(&state.pool, params.state.state(), before, limit + 1).await?;
    let next_cursor = next_page(&mut rows, limit, |r| (r.created_at, r.id));
    let items: Vec<JobView> = rows
        .into_iter()
        .map(|r| JobView {
            candidates: r
                .candidates
                .as_deref()
                .and_then(|text| serde_json::from_str(text).ok()),
            id: r.id,
            request_id: r.request_id,
            state: r.state,
            reason: r.reason,
            infuse_pro: r.infuse_pro,
            attempts: r.attempts,
            top_infohash: r.top_infohash,
            chosen_infohash: r.chosen_infohash,
            picked: r.picked,
            rd_torrent_id: r.rd_torrent_id,
            last_error: r.last_error,
            created_at: r.created_at,
            updated_at: r.updated_at,
            title: r.title,
            year: r.year,
            media_type: r.media_type,
        })
        .collect();
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// The worker switches as they stand, and whether the environment pins
/// them (an env override always wins over the stored value).
async fn worker_view(state: &AppState) -> Result<Json<serde_json::Value>, AppError> {
    let settings = db::worker_settings(&state.pool, &state.config).await?;
    Ok(Json(json!({
        "enabled": settings.enabled,
        "mode": settings.mode.as_str(),
        "enabled_from_env": state.config.worker_enabled.is_some(),
        "mode_from_env": state.config.worker_mode.is_some(),
    })))
}

/// `GET /api/admin/worker`.
pub async fn get_worker(
    State(state): State<AppState>,
    _admin: RequireAdmin,
) -> Result<impl IntoResponse, AppError> {
    worker_view(&state).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerBody {
    enabled: Option<bool>,
    mode: Option<WorkerModeBody>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerModeBody {
    DryRun,
    Assisted,
    Auto,
}

impl WorkerModeBody {
    fn as_str(self) -> &'static str {
        match self {
            WorkerModeBody::DryRun => "dry_run",
            WorkerModeBody::Assisted => "assisted",
            WorkerModeBody::Auto => "auto",
        }
    }
}

/// `PUT /api/admin/worker`: stores the given switches and returns the
/// effective values.
pub async fn put_worker(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    AppJson(body): AppJson<WorkerBody>,
) -> Result<impl IntoResponse, AppError> {
    if let Some(enabled) = body.enabled {
        db::set_setting(
            &state.pool,
            db::SETTING_WORKER_ENABLED,
            if enabled { "1" } else { "0" },
        )
        .await?;
    }
    if let Some(mode) = body.mode {
        db::set_setting(&state.pool, db::SETTING_WORKER_MODE, mode.as_str()).await?;
    }
    if body.enabled.is_some() || body.mode.is_some() {
        tracing::info!(
            enabled = ?body.enabled,
            mode = ?body.mode,
            "worker settings changed"
        );
        state.events.publish(Tick::Admin);
    }
    worker_view(&state).await
}
