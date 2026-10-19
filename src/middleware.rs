//! Cross-cutting HTTP middleware: security headers, the CSRF/Origin guard
//! on state-changing requests, body size and time limits, concurrency
//! limiting and the panic safety net.

use std::any::Any;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderName, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::{
    catch_panic::CatchPanicLayer, limit::RequestBodyLimitLayer, set_header::SetResponseHeaderLayer,
    timeout::TimeoutLayer,
};

use crate::{clientip::ClientIp, error::AppError, state::AppState};

pub const BODY_LIMIT_BYTES: usize = 16 * 1024;
pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub const CONCURRENCY_LIMIT: usize = 64;
/// The internal listener carries candidate lists, which the public 16 KiB
/// limit would cut off.
pub const INTERNAL_BODY_LIMIT_BYTES: usize = 256 * 1024;
/// One worker polls this listener; a handful of calls at once is plenty.
pub const INTERNAL_CONCURRENCY_LIMIT: usize = 8;

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
img-src 'self' https://image.tmdb.org; connect-src 'self'; base-uri 'none'; \
form-action 'self'; frame-ancestors 'none'";

/// Adds the fixed security response headers from the spec's "Headers and
/// input handling" section to every response, static or API.
pub fn apply_security_headers<S>(router: Router<S>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static(CSP),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ))
}

/// `Cache-Control: no-store` for every `/api` (and `/healthz`) response, per
/// the spec: "All endpoints return JSON... and send `Cache-Control: no-store`."
pub fn no_store_layer() -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::overriding(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))
}

fn is_state_changing(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// Rejects state-changing requests that don't carry `X-Requested-With: mp`,
/// a matching `Origin`, and a JSON content type. Runs before body parsing:
/// this is ordinary middleware wrapping the whole router, so it executes
/// before any handler's `Json`/`AppJson` extractor does.
pub async fn csrf_guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if is_state_changing(req.method()) {
        let origin = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok());
        let origin_ok = origin.is_some_and(|v| v == state.config.public_origin);
        let xrw_ok = req
            .headers()
            .get("x-requested-with")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == "mp");
        let content_type_ok = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json"));

        if !origin_ok || !xrw_ok || !content_type_ok {
            // The most common cause is a PUBLIC_ORIGIN that differs from what
            // the browser sends, so say which check failed and what arrived.
            // The Origin header is not a secret; cap it in case it is junk.
            let mut reasons = Vec::new();
            if !origin_ok {
                reasons.push("origin_mismatch");
            }
            if !xrw_ok {
                reasons.push("missing_x_requested_with");
            }
            if !content_type_ok {
                reasons.push("bad_content_type");
            }
            let shown: String = origin.unwrap_or("(none)").chars().take(200).collect();
            tracing::warn!(
                reasons = reasons.join(","),
                origin = shown,
                expected_origin = %state.config.public_origin,
                method = %req.method(),
                path = req.uri().path(),
                "request rejected by the csrf guard"
            );
            return AppError::Forbidden.into_response();
        }
    }
    next.run(req).await
}

/// The global per-IP limit on every `/api` call. Runs before the CSRF
/// guard and before any handler parses a body, so a flood is turned away
/// as cheaply as possible.
pub async fn api_rate_limit(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    req: Request,
    next: Next,
) -> Response {
    if let Err(retry) = state.limits.check_api(ip) {
        tracing::info!(%ip, retry_after_secs = retry.0, "api rate limit hit");
        return AppError::from(retry).into_response();
    }
    next.run(req).await
}

/// One structured log line per request: method, path, status, latency and
/// client IP. Never the query string (it carries search text), headers,
/// cookies or bodies. Health checks are skipped; they run every 30 s.
pub async fn request_log(ClientIp(ip): ClientIp, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let started = std::time::Instant::now();
    let response = next.run(req).await;
    if path != "/healthz" {
        log_request(false, ip, &method, &path, response.status(), started);
    }
    response
}

/// Whether an internal call is the worker's routine polling: a successful
/// progress or settings call, or a claim with nothing to hand out. Those
/// repeat every few seconds and would fill the log (and the SD card); they
/// log at `debug`. Everything else, failures and reports included, stays at
/// `info`.
fn is_routine_internal_call(path: &str, status: StatusCode) -> bool {
    if !status.is_success() {
        return false;
    }
    path == "/internal/settings"
        || (path.starts_with("/internal/jobs/") && path.ends_with("/progress"))
        || (path == "/internal/jobs/claim" && status == StatusCode::NO_CONTENT)
}

/// [`request_log`] for the internal listener, with the routine polling
/// calls demoted to `debug`.
pub async fn internal_request_log(ClientIp(ip): ClientIp, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let started = std::time::Instant::now();
    let response = next.run(req).await;
    let routine = is_routine_internal_call(&path, response.status());
    log_request(routine, ip, &method, &path, response.status(), started);
    response
}

fn log_request(
    debug: bool,
    ip: std::net::IpAddr,
    method: &axum::http::Method,
    path: &str,
    status: StatusCode,
    started: std::time::Instant,
) {
    let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let status = status.as_u16();
    if debug {
        tracing::debug!(target: "mediaqueue::request", %ip, %method, %path, status, ms, "request");
    } else {
        tracing::info!(target: "mediaqueue::request", %ip, %method, %path, status, ms, "request");
    }
}

pub fn body_limit_layer() -> RequestBodyLimitLayer {
    RequestBodyLimitLayer::new(BODY_LIMIT_BYTES)
}

pub fn internal_body_limit_layer() -> RequestBodyLimitLayer {
    RequestBodyLimitLayer::new(INTERNAL_BODY_LIMIT_BYTES)
}

pub fn timeout_layer() -> TimeoutLayer {
    TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, REQUEST_TIMEOUT)
}

pub fn concurrency_limit_layer() -> ConcurrencyLimitLayer {
    ConcurrencyLimitLayer::new(CONCURRENCY_LIMIT)
}

pub fn internal_concurrency_limit_layer() -> ConcurrencyLimitLayer {
    ConcurrencyLimitLayer::new(INTERNAL_CONCURRENCY_LIMIT)
}

fn panic_message<'a>(err: &'a (dyn Any + Send + 'static)) -> &'a str {
    if let Some(s) = err.downcast_ref::<&str>() {
        s
    } else if let Some(s) = err.downcast_ref::<String>() {
        s.as_str()
    } else {
        "unknown panic payload"
    }
}

fn panic_response(err: Box<dyn Any + Send + 'static>) -> Response {
    tracing::error!(panic = panic_message(&*err), "request handler panicked");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "internal_error", "message": "internal error"})),
    )
        .into_response()
}

/// A route that panics must still produce a response (our JSON error
/// shape, not axum's default) and must not take the server down.
pub fn catch_panic_layer() -> CatchPanicLayer<fn(Box<dyn Any + Send + 'static>) -> Response> {
    CatchPanicLayer::custom(panic_response as fn(Box<dyn Any + Send + 'static>) -> Response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_successful_polling_calls_are_routine() {
        let ok = StatusCode::OK;
        let none = StatusCode::NO_CONTENT;
        assert!(is_routine_internal_call("/internal/settings", ok));
        assert!(is_routine_internal_call("/internal/jobs/7/progress", ok));
        assert!(is_routine_internal_call("/internal/jobs/claim", none));
        // A claim that hands out a job, and every report, stay visible.
        assert!(!is_routine_internal_call("/internal/jobs/claim", ok));
        assert!(!is_routine_internal_call("/internal/jobs/7/report", ok));
        // So does anything that went wrong.
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::CONFLICT,
            StatusCode::BAD_REQUEST,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            assert!(!is_routine_internal_call("/internal/settings", status));
            assert!(!is_routine_internal_call(
                "/internal/jobs/7/progress",
                status
            ));
            assert!(!is_routine_internal_call("/internal/jobs/claim", status));
        }
    }

    #[test]
    fn get_and_head_are_not_state_changing() {
        assert!(!is_state_changing(&Method::GET));
        assert!(!is_state_changing(&Method::HEAD));
    }

    #[test]
    fn mutating_methods_are_state_changing() {
        assert!(is_state_changing(&Method::POST));
        assert!(is_state_changing(&Method::PUT));
        assert!(is_state_changing(&Method::PATCH));
        assert!(is_state_changing(&Method::DELETE));
    }

    #[test]
    fn panic_message_extracts_str_and_string_payloads() {
        let err: Box<dyn Any + Send> = Box::new("boom");
        assert_eq!(panic_message(&*err), "boom");
        let err: Box<dyn Any + Send> = Box::new(String::from("boom2"));
        assert_eq!(panic_message(&*err), "boom2");
        let err: Box<dyn Any + Send> = Box::new(42i32);
        assert_eq!(panic_message(&*err), "unknown panic payload");
    }
}
