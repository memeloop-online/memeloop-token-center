//! Explicit, at-most-one locally dispatched supplier reset per durable operation.
//! A redeem_request_id is correlation only: supplier idempotency is unproven.
use super::*;
use crate::{
    db::{PrepareQuotaReset, QuotaResetClaim, QuotaResetOperation},
    error::AppError,
};

const CONSUME_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";

#[derive(Serialize)]
pub(crate) struct PreparedReset {
    operation: QuotaResetOperation,
    confirmation_token: String,
    idempotency_replayed: bool,
}

fn blocked() -> AppError {
    AppError::Conflict(
        "fresh supplier quota and applicable reset credit evidence are required".into(),
    )
}

fn temporarily_unavailable() -> AppError {
    AppError::Overloaded
}

async fn fresh_snapshot(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    tenant: &str,
) -> Result<QuotaSnapshot, AppError> {
    if account.driver != "openai-codex" {
        return Err(blocked());
    }
    let selection = state.transport_proxy_groups.select_config(
        account.id,
        account.credential_generation,
        credential,
        &account.config,
    )?;
    let credential = &selection.credential;
    let _permit = state
        .upstream_quota
        .permits
        .try_acquire()
        .map_err(|_| temporarily_unavailable())?;
    let mut budget = codex_quota_budget(&account.config).map_err(|_| temporarily_unavailable())?;
    // Even the reset workflow's credit precheck remains a single attempt.
    // Consumption below is a separate no-retry transport and never enters the reader.
    budget.attempt_limit = 1;
    let session = retry::ReadSession::new(
        Some(state),
        tokio::time::Instant::now() + budget.total,
        budget,
    );
    let observed_started_at = unix_millis();
    let account_header =
        crate::oauth::managed::codex::account_header_value(credential).map_err(|_| blocked())?;
    let http = state
        .codex_clients
        .account_transport_snapshot(account, credential, selection.generation)
        .map_err(|_| temporarily_unavailable())?;
    let (credential_header, credential_value) = credential
        .request_header(observed_started_at)
        .map_err(|_| blocked())?
        .ok_or_else(blocked)?;
    let auth = CodexQuotaAuth {
        credential_header,
        credential_value,
        account: account_header,
        proxy_url: credential.proxy().map(|(url, _)| url),
    };
    let refresh_started = tokio::time::Instant::now();
    let reset = match tokio::time::timeout(
        budget.total,
        get_codex_json(
            &http,
            auth,
            CREDITS_URL,
            QuotaRequestContext::for_account(account, "credits", QuotaReadTrigger::ResetWorkflow),
            budget,
            &session,
        ),
    )
    .await
    {
        Ok(Ok(reset)) => reset,
        Ok(Err(error)) => {
            log_quota_read_completed(
                account,
                QuotaReadTrigger::ResetWorkflow,
                "error",
                error,
                refresh_started,
            );
            return Err(temporarily_unavailable());
        }
        Err(_) => {
            log_quota_read_completed(
                account,
                QuotaReadTrigger::ResetWorkflow,
                "error",
                "quota_timeout",
                refresh_started,
            );
            return Err(temporarily_unavailable());
        }
    };
    let observed_at = unix_millis();
    let mut snapshot = QuotaSnapshot::empty(account, tenant, None);
    if let Err(error) = normalize::reset_credits(&mut snapshot, &reset, observed_at) {
        log_quota_read_completed(
            account,
            QuotaReadTrigger::ResetWorkflow,
            "error",
            error,
            refresh_started,
        );
        return Err(temporarily_unavailable());
    }
    snapshot.status = "ready";
    snapshot.freshness = "fresh";
    snapshot.observed_at = Some(observed_at);
    snapshot.stale_after = Some(observed_at + FRESH_MS);
    snapshot.finalize_reset_capability();
    log_quota_read_completed(
        account,
        QuotaReadTrigger::ResetWorkflow,
        "success",
        "none",
        refresh_started,
    );
    Ok(snapshot)
}

async fn fresh(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    tenant: &str,
) -> Result<QuotaSnapshot, AppError> {
    let snapshot = fresh_snapshot(state, account, credential, tenant).await?;
    let available = snapshot.reset_capability.available_credits;
    let applicable = snapshot.reset_capability.applicable_credits;
    if snapshot.reset_capability.credit_error_code.is_some()
        || available.is_none_or(|value| value < 0)
        || applicable.is_none_or(|value| value < 0)
    {
        return Err(temporarily_unavailable());
    }
    Ok(snapshot)
}

pub(crate) async fn prepare(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    tenant: &str,
    actor: &str,
    idempotency_key: &str,
) -> Result<PreparedReset, AppError> {
    let prepare_idempotency_hash = idempotency_hash(state, idempotency_key);
    if let Some(operation) = state
        .db
        .quota_reset_prepare_replay(account, actor, &prepare_idempotency_hash)
        .await?
    {
        let operation_id = Uuid::parse_str(&operation.id).map_err(|_| AppError::Internal)?;
        return Ok(PreparedReset {
            operation,
            confirmation_token: confirmation_token(
                state,
                account,
                actor,
                operation_id,
                idempotency_key,
            ),
            idempotency_replayed: true,
        });
    }
    let snapshot = fresh(state, account, credential, tenant).await?;
    let available = snapshot
        .reset_capability
        .available_credits
        .ok_or_else(blocked)?;
    let applicable = snapshot
        .reset_capability
        .applicable_credits
        .ok_or_else(blocked)?;
    state
        .db
        .settle_accepted_quota_reset_from_observation(
            account,
            available,
            applicable,
            snapshot.observed_at.ok_or_else(blocked)?,
        )
        .await?;
    if available < 1 || applicable < 1 {
        return Err(blocked());
    }
    let operation_id = Uuid::now_v7();
    let token = confirmation_token(state, account, actor, operation_id, idempotency_key);
    let result = state
        .db
        .prepare_quota_reset(PrepareQuotaReset {
            id: operation_id,
            account: account.clone(),
            actor: actor.to_owned(),
            confirmation_hash: token_hash(state, &token),
            prepare_idempotency_hash,
            available,
            applicable,
            observed_at: snapshot.observed_at.ok_or_else(blocked)?,
        })
        .await?;
    let token = confirmation_token(
        state,
        account,
        actor,
        Uuid::parse_str(&result.operation.id).map_err(|_| AppError::Internal)?,
        idempotency_key,
    );
    Ok(PreparedReset {
        operation: result.operation,
        confirmation_token: token,
        idempotency_replayed: result.replayed,
    })
}

fn keyed_hash(state: &AppState, domain: &[u8], value: &[u8]) -> String {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(state.config.key_pepper.as_bytes())
        .expect("HMAC supports any key length");
    mac.update(domain);
    mac.update(&[0]);
    mac.update(value);
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

fn token_hash(state: &AppState, token: &str) -> String {
    keyed_hash(
        state,
        b"quota-reset-confirmation-token-v1",
        token.as_bytes(),
    )
}

fn idempotency_hash(state: &AppState, idempotency_key: &str) -> String {
    keyed_hash(
        state,
        b"quota-reset-idempotency-key-v1",
        idempotency_key.as_bytes(),
    )
}

fn confirmation_token(
    state: &AppState,
    account: &UpstreamAccountView,
    actor: &str,
    operation_id: Uuid,
    idempotency_key: &str,
) -> String {
    let material = format!(
        "{}\0{}\0{}\0{}",
        account.id, actor, operation_id, idempotency_key
    );
    format!(
        "qrct_{}",
        keyed_hash(
            state,
            b"quota-reset-confirmation-token-material-v1",
            material.as_bytes()
        )
    )
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
    idempotency_key: &str,
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
    let confirmation_hash = token_hash(state, token);
    let confirm_idempotency_hash = idempotency_hash(state, idempotency_key);
    if original.state != "prepared" {
        return match state
            .db
            .claim_quota_reset(
                account,
                operation,
                actor,
                &confirmation_hash,
                &confirm_idempotency_hash,
            )
            .await?
        {
            QuotaResetClaim::Replayed(operation) => Ok(*operation),
            QuotaResetClaim::Claimed { .. } => Err(AppError::Internal),
        };
    }
    let snapshot = fresh(state, account, credential, tenant).await?;
    if snapshot.reset_capability.available_credits != Some(original.available_credits)
        || snapshot.reset_capability.applicable_credits != Some(original.applicable_credits)
        || original.applicable_credits < 1
    {
        return Err(blocked());
    }
    // Prepare the fixed destination before the durable dispatch claim.
    let selection = state.transport_proxy_groups.select_config(
        account.id,
        account.credential_generation,
        credential,
        &account.config,
    )?;
    let dispatch_credential = &selection.credential;
    let account_header = crate::oauth::managed::codex::account_header_value(dispatch_credential)
        .map_err(|_| blocked())?;
    let _permit = state
        .upstream_quota
        .permits
        .try_acquire()
        .map_err(|_| temporarily_unavailable())?;
    // Use the existing mutation-only network factory (including its shared
    // HTTP settings integration); do not impose another reset-specific deadline.
    let http = crate::network::client_for_codex_url_without_retries(
        &state.http,
        CONSUME_URL,
        &json!({"network_scope":"public"}),
        dispatch_credential.proxy(),
        false,
    )
    .await
    .map_err(|_| temporarily_unavailable())?;
    let claim = state
        .db
        .claim_quota_reset(
            account,
            operation,
            actor,
            &confirmation_hash,
            &confirm_idempotency_hash,
        )
        .await?;
    let redeem = match claim {
        QuotaResetClaim::Claimed { redeem_request_id } => redeem_request_id,
        QuotaResetClaim::Replayed(operation) => return Ok(*operation),
    };
    // No automatic retry and no new redeem ID on any ambiguous outcome.
    let result = dispatch_once(
        &http,
        dispatch_credential,
        account_header,
        CONSUME_URL,
        &redeem,
    )
    .await;
    drop(_permit);
    let accepted = result.is_ok();
    state
        .db
        .finish_quota_reset(operation, accepted, result.err().as_deref())
        .await?;
    state
        .upstream_quota
        .entries
        .lock()
        .await
        .retain(|key, _| key.account != account.id);
    if accepted {
        // The consume response proves only that the reset was accepted. A
        // separate fresh supplier read is the evidence that may clear the
        // exact quota-exhausted health row through the existing fenced CAS.
        // Recovery failure must never replay the paid reset or change its
        // accepted durable result.
        // Always pass the stored credential, never the request-local selected
        // proxy credential: read_fresh owns selection and the normal quota,
        // cache, accepted-credit settlement and health recovery implementation.
        let snapshot = state
            .upstream_quota
            .read_fresh(
                state,
                account,
                credential,
                tenant,
                QuotaReadTrigger::ResetWorkflow,
            )
            .await;
        let error = snapshot
            .error_code
            .or(snapshot.reset_capability.credit_error_code);
        state
            .db
            .record_quota_reset_readback(account, operation, actor, error)
            .await?;
    }
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
    actor: &str,
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
            actor,
            account.credential_generation,
            account.updated_at,
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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn recover(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    tenant: &str,
    actor: &str,
    operation: &str,
    expected_updated_at: i64,
    reason: &str,
) -> Result<QuotaResetOperation, AppError> {
    let tenant_id = account.tenant_id.to_string();
    let account_id = account.id.to_string();
    let original = state
        .db
        .quota_reset_operation(&tenant_id, &account_id, operation)
        .await?;
    if !original.manual_unlock_available || original.updated_at != expected_updated_at {
        return Err(AppError::Conflict(
            "only the reviewed, locked unknown operation can be recovered".into(),
        ));
    }
    // GET only. No balance observation is treated as proof of non-execution.
    let snapshot = fresh(state, account, credential, tenant).await?;
    state
        .db
        .release_unknown_quota_reset(
            account,
            operation,
            actor,
            expected_updated_at,
            reason,
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
) -> Result<(), String> {
    let request = http
        .post(url)
        .header("chatgpt-account-id", account)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::USER_AGENT,
            crate::oauth::managed::codex::USER_AGENT,
        )
        .header("originator", crate::oauth::managed::codex::ORIGINATOR)
        .json(&json!({"redeem_request_id":redeem}));
    let request = credential
        .apply(request, unix_millis())
        .map_err(|_| "reset_prepare_credential_invalid".to_owned())?;
    let response = request.send().await.map_err(|error| {
        // Never retain Display/source/URL: reqwest errors may contain proxy
        // authentication, account credentials or supplier-controlled strings.
        let reason = match (error.is_connect(), error.is_timeout()) {
            (true, true) => "connect_timeout",
            (true, false) => "connect_failed",
            (false, true) => "timeout",
            _ if error.is_builder() => "request_invalid",
            _ => "transport_failed",
        };
        format!("reset_send_{reason}")
    })?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "reset_response_http_{}",
            response.status().as_u16()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path},
    };

    #[test]
    fn reset_readback_requires_stored_credential_after_sticky_proxy_failover() {
        let account = Uuid::now_v7();
        let proxies = ["socks5h://100.64.0.4:1080", "socks5h://100.64.0.5:1080"];
        let groups = crate::db::TransportProxyGroups::parse(
            &json!([{"account_id":account,"version":1,"proxies":proxies}]).to_string(),
            b"reset-readback-regression",
        )
        .unwrap();
        let stored = UpstreamCredential::OAuth {
            access_token: "fixture-access".into(),
            refresh_token: None,
            expires_at: None,
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: Some(proxies[0].into()),
            proxy_network_scope: Some(crate::network::OutboundScope::Private),
        };
        let first = groups
            .select_config(account, 1, &stored, &json!({}))
            .unwrap();
        assert!(first.advance_after_connect_failure(&[0]).unwrap());
        let dispatch = groups
            .select_config(account, 1, &stored, &json!({}))
            .unwrap();
        assert_eq!(dispatch.credential.proxy().unwrap().0, proxies[1]);
        // This is the former success-path failure, reproduced without sending
        // any supplier mutation. A selected credential is not a stored base.
        assert!(
            groups
                .select_config(account, 1, &dispatch.credential, &json!({}))
                .is_err()
        );
        let readback = groups
            .select_config(account, 1, &stored, &json!({}))
            .unwrap();
        assert_eq!(readback.credential.proxy().unwrap().0, proxies[1]);
        assert_eq!(stored.proxy().unwrap().0, proxies[0]);
    }

    #[tokio::test]
    async fn dispatch_timeout_uses_existing_client_policy_and_keeps_only_safe_reason() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/consume"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(200)))
            .expect(1)
            .mount(&server)
            .await;
        let credential = UpstreamCredential::OAuth {
            access_token: "fixture-private-access".into(),
            refresh_token: None,
            expires_at: None,
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: None,
            proxy_network_scope: None,
        };
        let http = reqwest::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .timeout(Duration::from_millis(40))
            .build()
            .unwrap();
        let code = dispatch_once(
            &http,
            &credential,
            reqwest::header::HeaderValue::from_static("fixture-private-account"),
            &format!("{}/consume?private=fixture-query-secret", server.uri()),
            "fixture-redeem",
        )
        .await
        .unwrap_err();
        assert_eq!(code, "reset_send_timeout");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn consumption_does_not_retry_retry_after_or_auth_errors() {
        for status in [429, 503, 401, 403] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/consume"))
                .respond_with(ResponseTemplate::new(status).insert_header("Retry-After", "0"))
                .expect(1)
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
                format!("reset_response_http_{status}")
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }

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
            "reset_response_http_307"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
