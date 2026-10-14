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

use crate::{error::AppError, state::AppState};

pub const BODY_LIMIT_BYTES: usize = 16 * 1024;
pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub const CONCURRENCY_LIMIT: usize = 64;

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
        let origin_ok = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(|v| v == state.config.public_origin)
            .unwrap_or(false);
        let xrw_ok = req
            .headers()
            .get("x-requested-with")
            .and_then(|v| v.to_str().ok())
            .map(|v| v == "mp")
            .unwrap_or(false);
        let content_type_ok = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.starts_with("application/json"))
            .unwrap_or(false);

        if !origin_ok || !xrw_ok || !content_type_ok {
            return AppError::Forbidden.into_response();
        }
    }
    next.run(req).await
}

pub fn body_limit_layer() -> RequestBodyLimitLayer {
    RequestBodyLimitLayer::new(BODY_LIMIT_BYTES)
}

pub fn timeout_layer() -> TimeoutLayer {
    TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, REQUEST_TIMEOUT)
}

pub fn concurrency_limit_layer() -> ConcurrencyLimitLayer {
    ConcurrencyLimitLayer::new(CONCURRENCY_LIMIT)
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
