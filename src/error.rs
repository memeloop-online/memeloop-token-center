use axum::{Json, http::StatusCode, response::IntoResponse};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("authentication required")]
    Unauthorized,
    #[error("permission denied")]
    Forbidden,
    #[error("model has no price in the key currency")]
    UnpricedModel,
    #[error("available balance or configured budget is insufficient")]
    QuotaExceeded,
    #[error("rate limit exceeded")]
    RateLimited,
    #[error("resource not found")]
    NotFound,
    #[error("invalid request: {0}")]
    BadRequest(String),
    #[error("configured upstream is unavailable: {0}")]
    Upstream(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("internal error")]
    Internal,
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, code) = match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            Self::UnpricedModel => (StatusCode::FAILED_DEPENDENCY, "unpriced_model"),
            Self::QuotaExceeded => (StatusCode::TOO_MANY_REQUESTS, "insufficient_quota"),
            Self::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limit_exceeded"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
            Self::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream_error"),
            Self::Storage(_) | Self::Internal => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
        };
        let message = self.to_string();
        (
            status,
            Json(json!({"error": {"code": code, "message": message}})),
        )
            .into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(%error, "database operation failed");
        Self::Internal
    }
}

impl From<object_store::Error> for AppError {
    fn from(error: object_store::Error) -> Self {
        tracing::error!(%error, "object storage operation failed");
        Self::Storage(error.to_string())
    }
}

impl From<reqwest::Error> for AppError {
    fn from(error: reqwest::Error) -> Self {
        Self::Upstream(error.to_string())
    }
}
