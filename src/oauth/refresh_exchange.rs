//! One refresh-token dispatch boundary and redacted transport observation.
use super::{
    OAuthRefreshFailure, OAuthRefreshFailureKind, OAuthRefreshObservation, OAuthRefreshPhase,
    OAuthRefreshRequestGuard, OAuthRefreshTimeoutSource,
};
use crate::error::AppError;

pub(super) async fn execute(
    client: &reqwest::Client,
    request: reqwest::Request,
    guard: &dyn OAuthRefreshRequestGuard,
) -> Result<(u16, Vec<u8>), AppError> {
    guard.mark_request_started().await?;
    execute_started(client, request, Some(guard)).await
}

// A provider-owned retry operation keeps its single durable dispatch fence.
// It must not clear that fence between attempts, including connector failures.
pub(super) async fn execute_started(
    client: &reqwest::Client,
    request: reqwest::Request,
    connector_guard: Option<&dyn OAuthRefreshRequestGuard>,
) -> Result<(u16, Vec<u8>), AppError> {
    let started = tokio::time::Instant::now();
    let observed = |kind, status, phase, connector_failure, timeout_source| {
        AppError::OAuthRefresh(OAuthRefreshFailure {
            kind,
            http_status: status,
            observation: Some(OAuthRefreshObservation {
                phase,
                elapsed_ms: started.elapsed().as_millis() as u64,
                budget_ms: 0,
                connector_failure,
                timeout_source,
            }),
        })
    };
    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(error) => {
            // Only connector evidence proves that no request reached the
            // authorization server. Every ambiguous dispatch retains its fence.
            if error.is_connect() {
                if let Some(guard) = connector_guard {
                    guard.mark_request_not_sent(&error).await?;
                }
            }
            return Err(observed(
                if error.is_timeout() {
                    OAuthRefreshFailureKind::Timeout
                } else {
                    OAuthRefreshFailureKind::Network
                },
                None,
                OAuthRefreshPhase::BeforeHeaders,
                error.is_connect(),
                error
                    .is_timeout()
                    .then_some(OAuthRefreshTimeoutSource::HttpClient),
            ));
        }
    };
    let status = response.status().as_u16();
    let bytes = super::bounded_body(response).await.map_err(|_| {
        observed(
            OAuthRefreshFailureKind::ResponseBody,
            Some(status),
            OAuthRefreshPhase::ResponseBody,
            false,
            None,
        )
    })?;
    Ok((status, bytes))
}

pub(super) fn rejected(status: u16, body: &[u8]) -> AppError {
    let value = serde_json::from_slice::<serde_json::Value>(body).ok();
    let code = value.as_ref().and_then(|value| {
        value["error"]
            .as_str()
            .or_else(|| value["error"]["code"].as_str())
    });
    let kind = match code {
        Some("invalid_grant") => OAuthRefreshFailureKind::InvalidGrant,
        Some("refresh_revoked") => OAuthRefreshFailureKind::RefreshRevoked,
        Some("invalid_refresh_token") => OAuthRefreshFailureKind::InvalidRefreshToken,
        _ => OAuthRefreshFailureKind::HttpRejected,
    };
    AppError::OAuthRefresh(OAuthRefreshFailure::new(kind, Some(status)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejected_token_codes_are_typed_without_retaining_untrusted_messages() {
        for error in [
            serde_json::json!("invalid_grant"),
            serde_json::json!({"code":"invalid_grant","message":"private-token-fixture"}),
        ] {
            let error = rejected(
                400,
                &serde_json::to_vec(&serde_json::json!({"error":error})).unwrap(),
            );
            assert!(!format!("{error:?}").contains("private-token-fixture"));
            let AppError::OAuthRefresh(failure) = error else {
                panic!("expected typed refresh failure")
            };
            assert_eq!(failure.kind, OAuthRefreshFailureKind::InvalidGrant);
            assert_eq!(failure.http_status, Some(400));
        }
    }
}
