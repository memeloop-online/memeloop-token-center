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
    #[error("conflict: {0}")]
    Conflict(String),
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
        let (status, code, message) = match &self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized", self.to_string()),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden", self.to_string()),
            Self::UnpricedModel => (
                StatusCode::FAILED_DEPENDENCY,
                "unpriced_model",
                self.to_string(),
            ),
            Self::QuotaExceeded => (
                StatusCode::TOO_MANY_REQUESTS,
                "insufficient_quota",
                self.to_string(),
            ),
            Self::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_exceeded",
                self.to_string(),
            ),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", self.to_string()),
            Self::Conflict(_) => (StatusCode::CONFLICT, "conflict", self.to_string()),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "invalid_request", self.to_string()),
            Self::Upstream(_) => (
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "configured upstream is unavailable".to_owned(),
            ),
            Self::Storage(_) | Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal error".to_owned(),
            ),
        };
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
        tracing::warn!(
            is_timeout = error.is_timeout(),
            is_connect = error.is_connect(),
            "upstream HTTP operation failed"
        );
        Self::Upstream(error.to_string())
    }
}
