#![forbid(unsafe_code)]
// unwrap/expect are fine in test code; request-handling code must always
// return a proper `AppError` instead of panicking.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod auth;
#[cfg(feature = "cf-access")]
pub mod cf_access;
pub mod clientip;
pub mod clock;
pub mod config;
pub mod db;
pub mod error;
pub mod events;
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
    routing::{delete, get, patch, post},
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
        .route("/events", get(routes::public::get_events))
        .route("/search", get(routes::public::get_search))
        .route("/requests", post(routes::public::post_request))
        .route("/requests/mine", get(routes::public::get_requests_mine))
        .route("/requests/{id}", delete(routes::public::delete_request))
        .route("/admin/login", post(routes::admin::post_admin_login))
        .route("/admin/logout", post(routes::admin::post_admin_logout))
        .route("/admin/events", get(routes::admin::get_events))
        .route("/admin/counts", get(routes::admin::get_counts))
        .route("/admin/requests", get(routes::admin::get_requests))
        .route(
            "/admin/requests/{id}",
            patch(routes::admin::patch_request).delete(routes::admin::delete_request),
        )
        .route(
            "/admin/requests/{id}/events",
            get(routes::admin::get_request_events),
        )
        .route(
            "/admin/requests/{id}/fetch",
            post(routes::admin::post_fetch),
        )
        .route("/admin/jobs", get(routes::admin::get_jobs))
        .route(
            "/admin/jobs/{id}/retry",
            post(routes::admin::post_job_retry),
        )
        .route(
            "/admin/jobs/{id}/cancel",
            post(routes::admin::post_job_cancel),
        )
        .route(
            "/admin/worker",
            get(routes::admin::get_worker).put(routes::admin::put_worker),
        )
        .route("/admin/export.csv", get(routes::admin::get_export))
        .route(
            "/admin/members",
            get(routes::admin::get_members).post(routes::admin::post_member),
        )
        .route(
            "/admin/members/{id}",
            patch(routes::admin::patch_member).delete(routes::admin::delete_member),
        )
        .route(
            "/admin/members/{id}/rotate",
            post(routes::admin::post_rotate),
        )
        .route(
            "/admin/members/{id}/revoke",
            post(routes::admin::post_revoke),
        )
        .route(
            "/admin/members/{id}/restore",
            post(routes::admin::post_restore),
        )
        .route(
            "/admin/sessions/{id}",
            delete(routes::admin::delete_session),
        )
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
        .nest("/api", api_router(state.clone()))
        .fallback(routes::static_files::serve);
    #[cfg(feature = "cf-access")]
    let app = app.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        cf_access::guard,
    ));
    let app = app
        .layer(middleware::catch_panic_layer())
        .layer(middleware::body_limit_layer())
        .layer(middleware::timeout_layer())
        // Outside the timeout and panic layers, so 408s and 500s are logged.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::request_log,
        ))
        .layer(middleware::concurrency_limit_layer());

    middleware::apply_security_headers(app)
}

/// The router for the worker's non-public listener. Deliberately not the
/// public stack: no CSRF guard (the caller is a service with a bearer
/// token, not a browser), no per-IP rate limit (every container would share
/// one Docker address), no Cloudflare Access and no browser security
/// headers. It has its own larger body limit for candidate lists and a
/// small concurrency limit. Authentication is the `RequireWorker`
/// extractor on every handler.
pub fn internal_router(state: AppState) -> Router {
    Router::new()
        .route("/internal/jobs/claim", post(routes::internal::post_claim))
        .route(
            "/internal/jobs/{id}/progress",
            post(routes::internal::post_progress),
        )
        .route(
            "/internal/jobs/{id}/report",
            post(routes::internal::post_report),
        )
        .route("/internal/settings", get(routes::internal::get_settings))
        .fallback(|| async { error::AppError::NotFound })
        .layer(middleware::no_store_layer())
        .layer(middleware::catch_panic_layer())
        .layer(middleware::internal_body_limit_layer())
        .layer(middleware::timeout_layer())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::internal_request_log,
        ))
        .layer(middleware::internal_concurrency_limit_layer())
        .with_state(state)
}
