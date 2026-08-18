use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::IntoResponse,
};
use serde_json::json;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitReason {
    BalanceExhausted,
    DailyBudgetExhausted,
    WeeklyBudgetExhausted,
    LifetimeBudgetExhausted,
    RpmExhausted,
    TpmExhausted,
    ConcurrencyExhausted,
}

impl LimitReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BalanceExhausted => "balance_exhausted",
            Self::DailyBudgetExhausted => "daily_budget_exhausted",
            Self::WeeklyBudgetExhausted => "weekly_budget_exhausted",
            Self::LifetimeBudgetExhausted => "lifetime_budget_exhausted",
            Self::RpmExhausted => "rpm_exhausted",
            Self::TpmExhausted => "tpm_exhausted",
            Self::ConcurrencyExhausted => "concurrency_exhausted",
        }
    }

    const fn is_quota(self) -> bool {
        matches!(
            self,
            Self::BalanceExhausted
                | Self::DailyBudgetExhausted
                | Self::WeeklyBudgetExhausted
                | Self::LifetimeBudgetExhausted
        )
    }

    const fn retryable(self) -> bool {
        !matches!(self, Self::BalanceExhausted | Self::LifetimeBudgetExhausted)
    }
}

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
    #[error("usage limit exceeded: {reason:?}")]
    LimitExceeded {
        reason: LimitReason,
        retry_after_seconds: Option<u64>,
    },
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
            Self::LimitExceeded { reason, .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                if reason.is_quota() {
                    "insufficient_quota"
                } else {
                    "rate_limit_exceeded"
                },
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
        let mut response = match &self {
            Self::LimitExceeded { reason, .. } => (
                status,
                Json(json!({"error": {
                    "code": code,
                    "message": message,
                    "reason": reason.as_str(),
                    "retryable": reason.retryable(),
                }})),
            )
                .into_response(),
            _ => (
                status,
                Json(json!({"error": {"code": code, "message": message}})),
            )
                .into_response(),
        };
        if let Self::LimitExceeded {
            retry_after_seconds: Some(seconds),
            ..
        } = self
            && let Ok(value) = HeaderValue::from_str(&seconds.max(1).to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
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

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    #[tokio::test]
    async fn retryable_limit_response_has_fixed_reason_and_retry_after() {
        let response = AppError::LimitExceeded {
            reason: LimitReason::RpmExhausted,
            retry_after_seconds: Some(17),
        }
        .into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "17");
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "rate_limit_exceeded");
        assert_eq!(body["error"]["reason"], "rpm_exhausted");
        assert_eq!(body["error"]["retryable"], true);
    }

    #[tokio::test]
    async fn permanent_limit_response_has_no_retry_after() {
        let response = AppError::LimitExceeded {
            reason: LimitReason::LifetimeBudgetExhausted,
            retry_after_seconds: None,
        }
        .into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(!response.headers().contains_key(header::RETRY_AFTER));
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "insufficient_quota");
        assert_eq!(body["error"]["reason"], "lifetime_budget_exhausted");
        assert_eq!(body["error"]["retryable"], false);
    }
}
