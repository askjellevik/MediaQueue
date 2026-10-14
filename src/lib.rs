#![forbid(unsafe_code)]
// unwrap/expect are fine in test code; request-handling code must always
// return a proper `AppError` instead of panicking.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod auth;
pub mod clientip;
pub mod clock;
pub mod config;
pub mod db;
pub mod error;
pub mod middleware;
pub mod notify;
pub mod ratelimit;
pub mod routes;
pub mod state;
pub mod tasks;
pub mod tmdb;

use axum::{
    Json, Router,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::json;

use crate::state::AppState;

async fn healthz(state: axum::extract::State<AppState>) -> impl IntoResponse {
    match db::healthcheck(&state.pool).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "healthz database check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "ok": false })),
            )
                .into_response()
        }
    }
}

/// All `/api` routes, with the per-IP rate limit, the CSRF/Origin guard and
/// `Cache-Control: no-store` applied to this subtree only — static assets
/// keep their own caching policy. Layers run outermost-last: no-store
/// wraps everything (429s included), then the rate limit, then CSRF.
fn api_router(state: AppState) -> Router<()> {
    Router::new()
        .route("/auth/code", post(routes::public::post_auth_code))
        .route("/auth/logout", post(routes::public::post_auth_logout))
        .route("/me", get(routes::public::get_me))
        .route("/search", get(routes::public::get_search))
        .route("/requests", post(routes::public::post_request))
        .route("/requests/mine", get(routes::public::get_requests_mine))
        .route("/admin/login", post(routes::admin::post_admin_login))
        .route("/admin/logout", post(routes::admin::post_admin_logout))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::csrf_guard,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::api_rate_limit,
        ))
        .layer(middleware::no_store_layer())
        .with_state(state)
}

/// Assembles the full application router. Shared by `main.rs` and the
/// integration tests so both exercise the exact same routing and
/// middleware stack.
pub fn build_router(state: AppState) -> Router {
    let healthz_router = Router::new()
        .route("/healthz", get(healthz))
        .layer(middleware::no_store_layer())
        .with_state(state.clone());

    let app = Router::new()
        .merge(healthz_router)
        .nest("/api", api_router(state))
        .fallback(routes::static_files::serve)
        .layer(middleware::catch_panic_layer())
        .layer(middleware::body_limit_layer())
        .layer(middleware::timeout_layer())
        .layer(middleware::concurrency_limit_layer());

    middleware::apply_security_headers(app)
}
