#![forbid(unsafe_code)]
// unwrap/expect are fine in test code; request-handling code must always
// return a proper `AppError` instead of panicking.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod auth;
pub mod clientip;
pub mod config;
pub mod db;
pub mod error;
pub mod middleware;
pub mod routes;
pub mod state;

use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::get};
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

/// Assembles the full application router. Shared by `main.rs` and the
/// integration tests so both exercise the exact same routing.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .fallback(routes::static_files::serve)
        .with_state(state)
}
