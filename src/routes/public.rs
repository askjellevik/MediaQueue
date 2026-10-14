//! User-facing `/api` routes: access-code login, logout, `/api/me` and
//! search. Request creation arrives in Phase 4.

use axum::{
    extract::State,
    http::header,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::{
    auth::{self, RequireUser},
    clientip::ClientIp,
    db,
    error::{AppError, AppJson, AppQuery},
    state::AppState,
    tmdb::{self, MediaType, SearchItem, SearchType},
};

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
    let member = match member {
        Some(member) if member.revoked_at.is_none() => member,
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
        member.max_sessions,
        &token_hash,
        now,
        expires_at,
    )
    .await?;

    if let Some(evicted_session_id) = result.evicted_session_id {
        tracing::warn!(
            member_id = member.id,
            evicted_session_id,
            "member reached max_sessions; evicted oldest session"
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
    remaining_today: i64,
}

/// `GET /api/me`: identifies the caller, or 401s so the frontend shows the
/// access-code gate.
pub async fn get_me(
    State(state): State<AppState>,
    user: RequireUser,
) -> Result<impl IntoResponse, AppError> {
    let limit = user.daily_limit.unwrap_or(state.config.default_daily_limit);
    let now = db::now_unix();
    let start_of_today = now - (now % 86_400);
    let used = db::submitted_count_since(&state.pool, user.member_id, start_of_today).await?;
    let remaining_today = (limit - used).max(0);

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
    state: &'static str,
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
    let state_of = |item: &SearchItem| {
        rows.iter()
            .find(|r| {
                r.tmdb_id == i64::from(item.tmdb_id) && r.media_type == item.media_type.as_str()
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
            state: state_of(item),
        })
        .collect();
    Ok(axum::Json(SearchBody { results }))
}
