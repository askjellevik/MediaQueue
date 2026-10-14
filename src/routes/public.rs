//! User-facing `/api` routes: access-code login, logout and `/api/me`.
//! Search and request creation arrive in later phases.

use axum::{
    extract::State,
    http::header,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::{
    auth::{self, RequireUser},
    db,
    error::{AppError, AppJson},
    state::AppState,
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
    AppJson(body): AppJson<CodeBody>,
) -> Result<Response, AppError> {
    // PHASE 2 HOOK: record this attempt (success or failure) against the
    // per-IP and global failure trackers before returning.
    let normalised = auth::normalise_code(&body.code);
    let code_hash = auth::hmac_code(&state.config.code_pepper, &normalised);

    let member = db::find_member_by_code_hash(&state.pool, &code_hash).await?;
    let Some(member) = member else {
        return Err(AppError::InvalidCode);
    };
    if member.revoked_at.is_some() {
        return Err(AppError::InvalidCode);
    }

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
