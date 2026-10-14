//! Admin-facing `/api/admin` routes. Login/logout land here in Phase 1;
//! the request and member management endpoints arrive in Phase 6.

use axum::{
    extract::State,
    http::header,
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use crate::{
    auth::{self, RequireAdmin},
    clientip::ClientIp,
    db,
    error::{AppError, AppJson},
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
