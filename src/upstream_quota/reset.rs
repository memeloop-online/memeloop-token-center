//! Explicit, at-most-one locally dispatched supplier reset per durable operation.
//! A redeem_request_id is correlation only: supplier idempotency is unproven.
use super::*;
use crate::{
    db::{PrepareQuotaReset, QuotaResetClaim, QuotaResetOperation},
    error::AppError,
};

const CONSUME_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";

/// Dropping a dispatch never proves that the supplier did not consume a credit.
/// Preserve uncertainty and the durable idempotency record, but do not strand
/// an operation in submitted after its owning future has actually ended.
struct DispatchCompletion {
    db: crate::db::Database,
    operation: String,
    owner: Uuid,
    accepted: bool,
    error: Option<&'static str>,
    finished: bool,
}

impl Drop for DispatchCompletion {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let db = self.db.clone();
        let operation = self.operation.clone();
        let owner = self.owner;
        let accepted = self.accepted;
        let error = self.error;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let mut delay = Duration::from_secs(1);
                loop {
                    match db
                        .finish_quota_reset_owned(&operation, Some(owner), accepted, error)
                        .await
                    {
                        Ok(()) => return,
                        Err(error) => tracing::error!(
                            operation_id = %operation,
                            error = %error,
                            "quota reset outcome persistence will retry without redispatch"
                        ),
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(30));
                }
            });
        }
    }
}

// Test binaries may redirect the fixed supplier to a loopback fixture.
// Production builds retain the fixed supplier endpoints.
#[cfg(test)]
pub(super) fn test_supplier_url(account: &UpstreamAccountView, original: &str) -> String {
    let Some(base) = account
        .config
        .get("reset_test_supplier_url")
        .and_then(Value::as_str)
    else {
        return original.to_owned();
    };
    let base = url::Url::parse(base).unwrap();
    assert!(base.scheme() == "http" && base.host_str() == Some("127.0.0.1"));
    base.join(match original {
        CONSUME_URL => "consume",
        USAGE_URL => "usage",
        _ => "credits",
    })
    .unwrap()
    .to_string()
}

#[derive(Clone, Debug)]
pub(crate) struct CreditEvidence {
    pub fingerprint: String,
    pub granted_at_ms: i64,
    pub expires_at_ms: i64,
}

pub(crate) struct CreditInventory {
    pub credits: Vec<CreditEvidence>,
    pub fingerprint: String,
}

#[derive(Clone)]
pub(crate) struct BackgroundResetGuard {
    pub grant: crate::db::plugin_background_actions::BackgroundActionGrant,
    /// Exact host-validated plugin snapshot, separate from business identity.
    pub runtime_revision: Option<i64>,
    pub lease_owner: String,
    pub credit_fingerprint: String,
    pub inventory_fingerprint: String,
    pub not_before_ms: i64,
    pub deadline_ms: i64,
}

pub(super) fn credit_fingerprint(credit: &Value) -> String {
    use sha2::{Digest, Sha256};
    // Supplier credit identity, where present, distinguishes equal-count
    // replacements with otherwise identical lifetime fields. The host uses
    // this identity as a fresh precondition; the supplier body carries a redeem ID.
    let field = |snake: &str, camel: &str| {
        credit
            .get(snake)
            .or_else(|| credit.get(camel))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let evidence = json!({
        "id": credit.get("id").cloned().unwrap_or_else(||field("credit_id","creditId")),
        "type": field("reset_type","resetType"),
        "status": credit.get("status"),
        "granted": field("granted_at","grantedAt"),
        "expires": field("expires_at","expiresAt"),
    });
    format!("{:x}", Sha256::digest(evidence.to_string().as_bytes()))
}

fn inventory(snapshot: &QuotaSnapshot, now: i64) -> CreditInventory {
    use sha2::{Digest, Sha256};
    let mut credits = snapshot
        .reset_credits
        .iter()
        .filter_map(|credit| {
            let granted = credit.granted_at?;
            let expires = credit.expires_at?;
            (credit.status.as_deref() == Some("available") && granted <= now && now < expires).then(
                || CreditEvidence {
                    fingerprint: credit.fingerprint.clone(),
                    granted_at_ms: granted,
                    expires_at_ms: expires,
                },
            )
        })
        .collect::<Vec<_>>();
    credits.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
    // Include multiplicity and supplier counts. A same-count replacement is
    // insufficient evidence even when the selected credit is still present.
    let evidence = json!({"credits":credits.iter().map(|c|&c.fingerprint).collect::<Vec<_>>(),
        "available":snapshot.reset_capability.available_credits,
        "applicable":snapshot.reset_capability.applicable_credits});
    CreditInventory {
        credits,
        fingerprint: format!("{:x}", Sha256::digest(evidence.to_string().as_bytes())),
    }
}

pub(crate) async fn background_credit_inventory(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    tenant: &str,
) -> Result<CreditInventory, AppError> {
    let snapshot = fresh(state, account, credential, tenant).await?;
    Ok(inventory(&snapshot, unix_millis()))
}

fn validate_background_evidence(
    snapshot: &QuotaSnapshot,
    guard: &BackgroundResetGuard,
    now: i64,
) -> Result<(), AppError> {
    if snapshot
        .reset_capability
        .available_credits
        .is_none_or(|count| count < 1)
        || snapshot
            .reset_capability
            .applicable_credits
            .is_none_or(|count| count < 1)
    {
        return Err(blocked());
    }
    let current = inventory(snapshot, now);
    let target = current
        .credits
        .iter()
        .find(|credit| credit.fingerprint == guard.credit_fingerprint)
        .ok_or_else(blocked)?;
    if current.fingerprint != guard.inventory_fingerprint
        || guard.not_before_ms > now
        || guard.deadline_ms <= now
        || guard.deadline_ms > target.expires_at_ms
        || guard.not_before_ms < target.granted_at_ms
        || guard.not_before_ms >= guard.deadline_ms
    {
        return Err(blocked());
    }
    Ok(())
}

/// An authorized worker enters this path. Stable business identity spans
/// runtime, package, configuration and authorization revisions, preserving
/// one dispatch identity through upgrades and re-enablement.
pub(crate) async fn background_reset(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    guard: &BackgroundResetGuard,
) -> Result<QuotaResetOperation, AppError> {
    let actor = format!("plugin:{}", guard.grant.plugin_id);
    let key = format!(
        "background-reset-credit-v1:{}:{}",
        account.id, guard.credit_fingerprint
    );
    // Validate before creating durable work. Scheduler ticks outside the window
    // or with replaced credits leave the durable operation set unchanged.
    let snapshot = fresh(state, account, credential, &guard.grant.tenant_external_id).await?;
    validate_background_evidence(&snapshot, guard, unix_millis())?;
    let prepared = prepare(
        state,
        account,
        credential,
        &guard.grant.tenant_external_id,
        &actor,
        &key,
    )
    .await?;
    if matches!(prepared.operation.state.as_str(), "prepared" | "expired") {
        state
            .db
            .refresh_background_quota_reset_prepare(
                account,
                &prepared.operation.id,
                snapshot
                    .reset_capability
                    .available_credits
                    .ok_or_else(blocked)?,
                snapshot
                    .reset_capability
                    .applicable_credits
                    .ok_or_else(blocked)?,
                snapshot.observed_at.ok_or_else(blocked)?,
                guard,
            )
            .await?;
    }
    confirm_guarded(
        state,
        account,
        credential,
        &guard.grant.tenant_external_id,
        &actor,
        &prepared.operation.id,
        &prepared.confirmation_token,
        &key,
        Some(guard),
    )
    .await
}

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

fn temporarily_unavailable(code: &'static str) -> AppError {
    AppError::capacity_error(code)
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
        .map_err(|_| temporarily_unavailable("quota_reset_permit"))?;
    let mut budget = codex_quota_budget(&account.config)
        .map_err(|_| temporarily_unavailable("quota_reset_budget"))?;
    // The reset workflow's credit precheck uses one attempt. Consumption below
    // uses its own fixed transport after the fresh read completes.
    budget.attempt_limit = 1;
    let session = retry::ReadSession::new(
        Some(state),
        tokio::time::Instant::now() + budget.total,
        budget,
    );
    let observed_started_at = unix_millis();
    let account_header =
        crate::oauth::managed::codex::account_header_value(credential).map_err(|_| blocked())?;
    let policy =
        crate::provider::CodexTransportPolicy::parse(account.config.get("transport_policy"))
            .map_err(|_| temporarily_unavailable("quota_reset_transport"))?;
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
    let credits_url = CREDITS_URL;
    #[cfg(test)]
    let fixture_url = test_supplier_url(account, credits_url);
    #[cfg(test)]
    let credits_url = fixture_url.as_str();
    let reset = match tokio::time::timeout(
        budget.total,
        get_codex_json(
            &state.codex_clients,
            policy,
            auth,
            credits_url,
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
            return Err(temporarily_unavailable("quota_reset_read_failed"));
        }
        Err(_) => {
            log_quota_read_completed(
                account,
                QuotaReadTrigger::ResetWorkflow,
                "error",
                "quota_timeout",
                refresh_started,
            );
            return Err(temporarily_unavailable("quota_reset_timeout"));
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
        return Err(temporarily_unavailable("quota_reset_normalize"));
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
        return Err(temporarily_unavailable("quota_reset_credits_unavailable"));
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
        let operation_id =
            Uuid::parse_str(&operation.id).map_err(|_| AppError::internal("reset_prepare_1"))?;
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
        Uuid::parse_str(&result.operation.id).map_err(|_| AppError::internal("reset_prepare_2"))?,
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
    confirm_guarded(
        state,
        account,
        credential,
        tenant,
        actor,
        operation,
        token,
        idempotency_key,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn confirm_guarded(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    tenant: &str,
    actor: &str,
    operation: &str,
    token: &str,
    idempotency_key: &str,
    guard: Option<&BackgroundResetGuard>,
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
            QuotaResetClaim::Claimed { .. } => Err(AppError::internal("reset_confirm_1")),
        };
    }
    let snapshot = fresh(state, account, credential, tenant).await?;
    if let Some(guard) = guard {
        validate_background_evidence(&snapshot, guard, unix_millis())?;
    }
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
        .map_err(|_| temporarily_unavailable("quota_reset_permit"))?;
    let http = consume_client(state, dispatch_credential).await?;
    #[cfg(test)]
    let http = if account.config.get("reset_test_supplier_url").is_some() {
        crate::build_no_retry_http_client(None, &[]).unwrap()
    } else {
        http
    };
    // Keep the claim and its completion guard in one owned task. Cancellation
    // while COMMIT is in flight cannot drop the only recovery owner before the
    // claim's database outcome is known. This task never sends to the supplier.
    let db = state.db.clone();
    let claim_account = account.clone();
    let claim_operation = operation.to_owned();
    let claim_actor = actor.to_owned();
    let claim_guard = guard.cloned();
    let (claim, mut completion) = tokio::spawn(async move {
        let mut completion = DispatchCompletion {
            db: db.clone(),
            operation: claim_operation.clone(),
            owner: Uuid::now_v7(),
            accepted: false,
            error: Some("reset_dispatch_owner_ended"),
            finished: false,
        };
        let claim = db
            .claim_quota_reset_owned(
                &claim_account,
                &claim_operation,
                &claim_actor,
                &confirmation_hash,
                &confirm_idempotency_hash,
                claim_guard.as_ref(),
                Some(completion.owner),
            )
            .await?;
        if matches!(claim, QuotaResetClaim::Replayed(_)) {
            completion.finished = true;
        }
        Ok::<_, AppError>((claim, completion))
    })
    .await
    .map_err(|_| AppError::internal("quota_reset_claim_task"))??;
    let redeem = match claim {
        QuotaResetClaim::Claimed { redeem_request_id } => redeem_request_id,
        QuotaResetClaim::Replayed(operation) => return Ok(*operation),
    };
    // Each durable operation keeps one redeem ID and one supplier dispatch.
    let result = if guard.is_some_and(|guard| unix_millis() >= guard.deadline_ms) {
        Err("quota_reset_deadline_elapsed")
    } else {
        let consume_url = CONSUME_URL;
        #[cfg(test)]
        let r2_fixture_url = r2_tests::fixture_url(consume_url);
        #[cfg(test)]
        let fixture_url = r2_fixture_url.unwrap_or_else(|| test_supplier_url(account, consume_url));
        #[cfg(test)]
        let consume_url = fixture_url.as_str();
        dispatch_once(
            &http,
            dispatch_credential,
            account_header,
            consume_url,
            &redeem,
        )
        .await
    };
    drop(_permit);
    let accepted = result.is_ok();
    completion.accepted = accepted;
    completion.error = result.err();
    state
        .db
        .finish_quota_reset_owned(
            operation,
            Some(completion.owner),
            accepted,
            completion.error,
        )
        .await?;
    completion.finished = true;
    state
        .upstream_quota
        .entries
        .lock()
        .await
        .retain(|key, _| key.account != account.id);
    #[cfg(test)]
    r2_tests::after_reset_eviction().await;
    if accepted {
        // The consume response proves only that the reset was accepted. A
        // separate fresh supplier read is the evidence that may clear the
        // exact quota-exhausted health row through the existing fenced CAS.
        // Recovery failure must never replay the paid reset or change its
        // accepted durable result.
        // read_fresh selects its own transport from the stored credential,
        // never from the consume request's selected proxy credential.
        let readback = state
            .upstream_quota
            .read_fresh(
                state,
                account,
                credential,
                tenant,
                QuotaReadTrigger::ResetWorkflow,
            )
            .await;
        match readback.error_code {
            None => tracing::info!(
                upstream_account_id = %account.id,
                credential_generation = account.credential_generation,
                trigger = QuotaReadTrigger::ResetWorkflow.as_str(),
                "refreshed quota evidence after an accepted reset"
            ),
            Some(error) => tracing::warn!(
                upstream_account_id = %account.id,
                credential_generation = account.credential_generation,
                trigger = QuotaReadTrigger::ResetWorkflow.as_str(),
                error = %error,
                error_code = "quota_reset_recovery_pending",
                "accepted quota reset supplier evidence refresh is pending"
            ),
        }
    }
    // Accepted records the supplier HTTP 2xx response; quota windows remain independently observed.
    state
        .db
        .quota_reset_operation(
            &account.tenant_id.to_string(),
            &account.id.to_string(),
            operation,
        )
        .await
}

async fn consume_client(
    state: &AppState,
    dispatch_credential: &UpstreamCredential,
) -> Result<reqwest::Client, AppError> {
    #[cfg(test)]
    if r2_tests::fixture_url(CONSUME_URL).is_some() {
        return Ok(crate::build_no_retry_http_client(None, &[]).unwrap());
    }
    tokio::time::timeout(
        Duration::from_secs(5),
        crate::network::client_for_codex_url_without_retries(
            &state.http,
            CONSUME_URL,
            &json!({"network_scope":"public"}),
            dispatch_credential.proxy(),
            false,
        ),
    )
    .await
    .map_err(|_| temporarily_unavailable("quota_reset_transport_timeout"))?
    .map_err(|_| temporarily_unavailable("quota_reset_transport"))
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

pub(crate) async fn recover(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    tenant: &str,
    actor: &str,
    operation: &str,
) -> Result<QuotaResetOperation, AppError> {
    let tenant_id = account.tenant_id.to_string();
    let account_id = account.id.to_string();
    let original = state
        .db
        .quota_reset_operation(&tenant_id, &account_id, operation)
        .await?;
    if original.state != "unknown" {
        return Err(AppError::Conflict(
            "reset recovery requires an unknown result and recorded dispatch completion".into(),
        ));
    }
    // A current balance informs the operator; it cannot attribute an unknown
    // supplier outcome. Recovery releases the local slot, never consumes.
    let snapshot = fresh(state, account, credential, tenant).await?;
    state
        .db
        .release_unknown_quota_reset(
            account,
            operation,
            actor,
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
        // A slow successful consumption must not become unknown merely because
        // eight seconds elapsed. Use the configured transport policy, with no
        // extra reset response deadline and no implicit HTTP retry.
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
    async fn explicit_unknown_recovery_only_reads_credits_and_never_consumes() {
        let supplier = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/credits"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "available_count": 1,
                "credits": [{
                    "id": "fixture-credit",
                    "reset_type": "codex_rate_limits",
                    "status": "available",
                    "granted_at": "2020-01-01T00:00:00Z",
                    "expires_at": "2099-01-01T00:00:00Z"
                }]
            })))
            .expect(2)
            .mount(&supplier)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&supplier)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::initialize(crate::config::Config::for_test(format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("reset-recovery.db").display()
        )))
        .await
        .unwrap();
        let tenant = "reset-recovery-fixture";
        let credential = UpstreamCredential::OAuth {
            access_token: "fixture-only".into(),
            refresh_token: None,
            expires_at: None,
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: Some(json!({
                "schema": "openai-codex-oauth-v1", "account_id": "fixture-account"
            })),
            proxy_url: None,
            proxy_network_scope: None,
        };
        let account = state
            .db
            .create_upstream_account(
                crate::db::CreateUpstreamAccountInput {
                    tenant_external_id: tenant.into(),
                    name: "reset recovery".into(),
                    driver: "openai-codex".into(),
                    config: json!({
                        "base_url": "https://chatgpt.com",
                        "reset_test_supplier_url": format!("{}/", supplier.uri())
                    }),
                    credential: credential.clone(),
                    oauth_session_id: None,
                    oauth_driver: None,
                    oauth_refresh_url: None,
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let operation = state
            .db
            .prepare_quota_reset(PrepareQuotaReset {
                id: Uuid::now_v7(),
                account: account.clone(),
                actor: "fixture".into(),
                confirmation_hash: "fixture".into(),
                prepare_idempotency_hash: "fixture".into(),
                available: 2,
                applicable: 2,
                observed_at: unix_millis() - 1_000,
            })
            .await
            .unwrap()
            .operation;
        state
            .db
            .claim_quota_reset(&account, &operation.id, "fixture", "fixture", "fixture")
            .await
            .unwrap();
        state
            .db
            .finish_quota_reset(&operation.id, false, Some("reset_dispatch_owner_ended"))
            .await
            .unwrap();
        for _ in 0..2 {
            let recovered = recover(
                &state,
                &account,
                &credential,
                tenant,
                "operator",
                &operation.id,
            )
            .await
            .unwrap();
            assert_eq!(recovered.state, "unknown");
            assert!(recovered.settled_at.is_some());
            assert_eq!(recovered.reconciled_available_credits, Some(1));
        }
        assert_eq!(supplier.received_requests().await.unwrap().len(), 2);
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
                "reset_supplier_response_unknown"
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
            "reset_supplier_response_unknown"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
