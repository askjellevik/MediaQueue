//! `AppError` is the one error type handlers return; it always renders as
//! the spec's JSON error shape and never leaks internal detail to clients.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    fn status(&self) -> StatusCode {
        match self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Database(_) | AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            AppError::NotFound => "not_found",
            AppError::Database(_) | AppError::Internal(_) => "internal_error",
        }
    }

    /// Message sent to the client. Internal failure detail (SQL errors,
    /// anyhow chains) stays in the server log via `tracing::error!`, never
    /// in the response body.
    fn public_message(&self) -> String {
        match self {
            AppError::NotFound => "not found".to_string(),
            AppError::Database(_) | AppError::Internal(_) => "internal error".to_string(),
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
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
        (status, Json(body)).into_response()
    }
}
