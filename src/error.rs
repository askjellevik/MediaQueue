//! `AppError` is the one error type handlers return; it always renders as
//! the spec's JSON error shape and never leaks internal detail to clients.

use axum::{
    Json,
    extract::{FromRequest, Request},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Serialize, de::DeserializeOwned};

use crate::ratelimit::RetryAfter;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    /// No valid session. Used by `RequireUser`/`RequireAdmin`.
    #[error("unauthorized")]
    Unauthorized,
    /// Origin/header/content-type check failed on a state-changing request.
    #[error("forbidden")]
    Forbidden,
    /// Wrong, rotated or revoked access code. Deliberately one variant so
    /// the response is identical regardless of which it was.
    #[error("invalid code")]
    InvalidCode,
    #[error("invalid admin credentials")]
    InvalidAdminCredentials,
    /// A rate limit or lockout was hit; sent as 429 with `Retry-After`.
    #[error("rate limited")]
    RateLimited { retry_after_secs: u64 },
    #[error("malformed request body")]
    BadRequest,
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    fn status(&self) -> StatusCode {
        match self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::Forbidden => StatusCode::FORBIDDEN,
            AppError::InvalidCode | AppError::InvalidAdminCredentials => StatusCode::UNAUTHORIZED,
            AppError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            AppError::BadRequest => StatusCode::BAD_REQUEST,
            AppError::Database(_) | AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            AppError::NotFound => "not_found",
            AppError::Unauthorized => "unauthorized",
            AppError::Forbidden => "forbidden",
            AppError::InvalidCode => "invalid_code",
            AppError::InvalidAdminCredentials => "invalid_credentials",
            AppError::RateLimited { .. } => "rate_limited",
            AppError::BadRequest => "bad_request",
            AppError::Database(_) | AppError::Internal(_) => "internal_error",
        }
    }

    /// Message sent to the client. Internal failure detail (SQL errors,
    /// anyhow chains) stays in the server log via `tracing::error!`, never
    /// in the response body.
    fn public_message(&self) -> &'static str {
        match self {
            AppError::NotFound => "not found",
            AppError::Unauthorized => "authentication required",
            AppError::Forbidden => "forbidden",
            AppError::InvalidCode => "that code didn't work",
            AppError::InvalidAdminCredentials => "invalid username or password",
            AppError::RateLimited { .. } => "too many requests, try again later",
            AppError::BadRequest => "the request body was malformed",
            AppError::Database(_) | AppError::Internal(_) => "internal error",
        }
    }
}

impl From<RetryAfter> for AppError {
    fn from(RetryAfter(retry_after_secs): RetryAfter) -> Self {
        AppError::RateLimited { retry_after_secs }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    message: &'static str,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if matches!(self, AppError::Database(_) | AppError::Internal(_)) {
            tracing::error!(error = %self, "request failed");
        }
        let status = self.status();
        let body = ErrorBody {
            error: self.code(),
            message: self.public_message(),
        };
        let mut response = (status, Json(body)).into_response();
        if let AppError::RateLimited { retry_after_secs } = self {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_secs));
        }
        response
    }
}

/// A `Json<T>` extractor that renders rejections (malformed JSON, unknown
/// fields, wrong content-type) through [`AppError`] instead of axum's
/// default plain-text rejection body, so every `/api` response — success
/// or failure — is JSON in the one error shape.
pub struct AppJson<T>(pub T);

impl<T, S> FromRequest<S> for AppJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(AppJson(value)),
            Err(rejection) => {
                tracing::debug!(error = %rejection, "rejected request body");
                Err(AppError::BadRequest)
            }
        }
    }
}
