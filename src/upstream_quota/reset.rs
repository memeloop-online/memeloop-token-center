//! Explicit, at-most-one locally dispatched supplier reset per durable operation.
//! A redeem_request_id is correlation only: supplier idempotency is unproven.
use super::*;
use crate::{
    crypto,
    db::{PrepareQuotaReset, QuotaResetOperation},
    error::AppError,
};

const CONSUME_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";

#[derive(Serialize)]
pub(crate) struct PreparedReset {
    operation: QuotaResetOperation,
    confirmation_token: String,
}

fn blocked() -> AppError {
    AppError::Conflict(
        "fresh supplier quota and applicable reset credit evidence are required".into(),
    )
}

async fn fresh(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    tenant: &str,
) -> Result<QuotaSnapshot, AppError> {
    if account.driver != "openai-codex" {
        return Err(blocked());
    }
    let _permit = state
        .upstream_quota
        .permits
        .try_acquire()
        .map_err(|_| blocked())?;
    let snapshot = tokio::time::timeout(
        Duration::from_secs(8),
        read_codex(
            state,
            credential,
            QuotaSnapshot::empty(account, tenant, None),
        ),
    )
    .await
    .map_err(|_| blocked())?
    .map_err(|_| blocked())?;
    if snapshot.reset_capability.credit_error_code.is_some()
        || snapshot.reset_capability.available_credits.is_none()
        || snapshot.reset_capability.applicable_credits.is_none()
    {
        return Err(blocked());
    }
    Ok(snapshot)
}

pub(crate) async fn prepare(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    tenant: &str,
    actor: &str,
) -> Result<PreparedReset, AppError> {
    let snapshot = fresh(state, account, credential, tenant).await?;
    let available = snapshot
        .reset_capability
        .available_credits
        .ok_or_else(blocked)?;
    let applicable = snapshot
        .reset_capability
        .applicable_credits
        .ok_or_else(blocked)?;
    if available < 1 || applicable < 1 {
        return Err(blocked());
    }
    let token =
        crypto::issue_service_credential(account.id, state.config.key_pepper.as_bytes()).secret;
    let hash = token_hash(state, &token);
    let operation = state
        .db
        .prepare_quota_reset(PrepareQuotaReset {
            account: account.clone(),
            actor: actor.to_owned(),
            confirmation_hash: hash,
            available,
            applicable,
            observed_at: snapshot.observed_at.ok_or_else(blocked)?,
        })
        .await?;
    Ok(PreparedReset {
        operation,
        confirmation_token: token,
    })
}

fn token_hash(state: &AppState, token: &str) -> String {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(state.config.key_pepper.as_bytes())
        .expect("HMAC supports any key length");
    mac.update(token.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn confirm(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    tenant: &str,
    actor: &str,
    operation: &str,
    token: &str,
) -> Result<QuotaResetOperation, AppError> {
    if !(32..=256).contains(&token.len()) {
        return Err(blocked());
    }
    let original = state
        .db
        .quota_reset_operation(
            &account.tenant_id.to_string(),
            &account.id.to_string(),
            operation,
        )
        .await?;
    if original.state != "prepared" {
        return Err(AppError::Conflict(
            "operation cannot be dispatched again".into(),
        ));
    }
    let snapshot = fresh(state, account, credential, tenant).await?;
    if snapshot.reset_capability.available_credits != Some(original.available_credits)
        || snapshot.reset_capability.applicable_credits != Some(original.applicable_credits)
        || original.applicable_credits < 1
    {
        return Err(blocked());
    }
    // Prepare the fixed destination before the durable dispatch claim.
    let account_header =
        crate::oauth::managed::codex::account_header_value(credential).map_err(|_| blocked())?;
    let _permit = state
        .upstream_quota
        .permits
        .try_acquire()
        .map_err(|_| blocked())?;
    let http = tokio::time::timeout(
        Duration::from_secs(5),
        crate::network::client_for_config_url_without_retries(
            &state.http,
            CONSUME_URL,
            &json!({"network_scope":"public"}),
            credential.proxy(),
            false,
        ),
    )
    .await
    .map_err(|_| blocked())?
    .map_err(|_| blocked())?;
    let redeem = state
        .db
        .claim_quota_reset(account, operation, actor, &token_hash(state, token))
        .await?;
    // No automatic retry and no new redeem ID on any ambiguous outcome.
    let result = dispatch_once(&http, credential, account_header, CONSUME_URL, &redeem).await;
    state
        .db
        .finish_quota_reset(operation, result.is_ok(), result.err())
        .await?;
    state
        .upstream_quota
        .entries
        .lock()
        .await
        .retain(|(id, _, _), _| *id != account.id);
    // Accepted means only supplier HTTP 2xx. It does not assert window state.
    state
        .db
        .quota_reset_operation(
            &account.tenant_id.to_string(),
            &account.id.to_string(),
            operation,
        )
        .await
}

pub(crate) async fn reconcile(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    tenant: &str,
    operation: &str,
) -> Result<QuotaResetOperation, AppError> {
    let tenant_id = account.tenant_id.to_string();
    let account_id = account.id.to_string();
    state
        .db
        .quota_reset_operation(&tenant_id, &account_id, operation)
        .await?;
    let snapshot = fresh(state, account, credential, tenant).await?;
    state
        .db
        .reconcile_quota_reset(
            &tenant_id,
            &account_id,
            operation,
            snapshot
                .reset_capability
                .available_credits
                .ok_or_else(blocked)?,
            snapshot
                .reset_capability
                .applicable_credits
                .ok_or_else(blocked)?,
            snapshot.observed_at.ok_or_else(blocked)?,
        )
        .await?;
    state
        .db
        .quota_reset_operation(&tenant_id, &account_id, operation)
        .await
}

async fn dispatch_once(
    http: &reqwest::Client,
    credential: &UpstreamCredential,
    account: reqwest::header::HeaderValue,
    url: &str,
    redeem: &str,
) -> Result<(), &'static str> {
    let request = http
        .post(url)
        .header("chatgpt-account-id", account)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::USER_AGENT,
            crate::oauth::managed::codex::USER_AGENT,
        )
        .header("originator", crate::oauth::managed::codex::ORIGINATOR)
        .timeout(Duration::from_secs(8))
        .json(&json!({"redeem_request_id":redeem}));
    let request = credential
        .apply(request, unix_millis())
        .map_err(|_| "reset_dispatch_unknown")?;
    let response = request.send().await.map_err(|_| "reset_dispatch_unknown")?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err("reset_supplier_response_unknown")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path},
    };

    #[tokio::test]
    async fn consumption_is_one_post_and_redirects_are_unknown_without_following() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/consume"))
            .and(body_json(
                json!({"redeem_request_id":"stable-operation-id"}),
            ))
            .respond_with(ResponseTemplate::new(307).insert_header("Location", "/retry"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/retry"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let credential = UpstreamCredential::OAuth {
            access_token: "fixture-token".into(),
            refresh_token: None,
            expires_at: None,
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: None,
            proxy_network_scope: None,
        };
        let http = crate::build_no_retry_http_client(None, &[]).unwrap();
        assert_eq!(
            dispatch_once(
                &http,
                &credential,
                reqwest::header::HeaderValue::from_static("fixture-account"),
                &format!("{}/consume", server.uri()),
                "stable-operation-id"
            )
            .await
            .unwrap_err(),
            "reset_supplier_response_unknown"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
