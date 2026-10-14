//! `AppError` is the one error type handlers return; it always renders as
//! the spec's JSON error shape and never leaks internal detail to clients.

use axum::{
    Json,
    extract::{FromRequest, FromRequestParts, Query, Request},
    http::{HeaderValue, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
};
use serde::{Serialize, de::DeserializeOwned};

use crate::{ratelimit::RetryAfter, tmdb::TmdbError};

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
    /// The member's per-minute or daily request quota is used up.
    #[error("request quota exceeded")]
    QuotaExceeded {
        retry_after_secs: u64,
        remaining_today: i64,
    },
    #[error("malformed request")]
    BadRequest,
    /// TMDB is unreachable and no usable cached answer exists.
    #[error("upstream unavailable")]
    Unavailable,
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
            AppError::RateLimited { .. } | AppError::QuotaExceeded { .. } => {
                StatusCode::TOO_MANY_REQUESTS
            }
            AppError::BadRequest => StatusCode::BAD_REQUEST,
            AppError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
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
            AppError::QuotaExceeded { .. } => "quota_exceeded",
            AppError::BadRequest => "bad_request",
            AppError::Unavailable => "upstream_unavailable",
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
            AppError::QuotaExceeded { .. } => "you have used up your requests for now",
            AppError::BadRequest => "the request was malformed",
            AppError::Unavailable => {
                "the movie database is not reachable right now, try again shortly"
            }
            AppError::Database(_) | AppError::Internal(_) => "internal error",
        }
    }
}

impl From<RetryAfter> for AppError {
    fn from(RetryAfter(retry_after_secs): RetryAfter) -> Self {
        AppError::RateLimited { retry_after_secs }
    }
}

impl From<TmdbError> for AppError {
    fn from(error: TmdbError) -> Self {
        match error {
            TmdbError::NotFound => AppError::NotFound,
            TmdbError::Unavailable | TmdbError::LocalLimit => AppError::Unavailable,
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    message: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    remaining_today: Option<i64>,
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
            remaining_today: match self {
                AppError::QuotaExceeded {
                    remaining_today, ..
                } => Some(remaining_today),
                _ => None,
            },
        };
        let mut response = (status, Json(body)).into_response();
        if let AppError::RateLimited { retry_after_secs }
        | AppError::QuotaExceeded {
            retry_after_secs, ..
        } = self
        {
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

/// A `Query<T>` extractor whose rejections render as [`AppError::BadRequest`]
/// in the one JSON error shape, like [`AppJson`].
pub struct AppQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for AppQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(AppQuery(value)),
            Err(rejection) => {
                tracing::debug!(error = %rejection, "rejected query string");
                Err(AppError::BadRequest)
            }
        }
    }
}
