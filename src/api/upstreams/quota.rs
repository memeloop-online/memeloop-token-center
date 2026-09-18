use super::super::*;
use crate::provider::UpstreamAccountView;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

const MAX_QUOTA_BATCH_ACCOUNTS: usize = 100;
const QUOTA_BATCH_CONCURRENCY: usize = 3;
// The browser permits five minutes for a list action. Leave thirty seconds to
// serialize and deliver partial results through the control-plane proxy.
const QUOTA_BATCH_TOTAL_BUDGET: Duration = Duration::from_secs(270);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct QuotaQuery {
    tenant_external_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct QuotaBatchRequest {
    account_ids: Vec<Uuid>,
    fresh: bool,
    trigger: crate::upstream_quota::QuotaRequestTrigger,
}

#[derive(Serialize)]
struct QuotaBatchResponse {
    contract_version: &'static str,
    results: Vec<QuotaBatchResult>,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum QuotaBatchResult {
    Success {
        upstream_account_id: Uuid,
        snapshot: Box<crate::upstream_quota::QuotaSnapshot>,
    },
    Error {
        upstream_account_id: Uuid,
        error: QuotaBatchError,
    },
}

#[derive(Serialize)]
struct QuotaBatchError {
    code: &'static str,
}

fn quota_batch_error(account_id: Uuid, code: &'static str) -> QuotaBatchResult {
    QuotaBatchResult::Error {
        upstream_account_id: account_id,
        error: QuotaBatchError { code },
    }
}

/// Reconstitutes one result per requested identity. A deadline never erases
/// already completed supplier reads; unfinished work is reported per account.
fn ordered_quota_batch_results(
    account_ids: &[Uuid],
    indexed_results: Vec<(usize, QuotaBatchResult)>,
) -> Vec<QuotaBatchResult> {
    let mut results = std::iter::repeat_with(|| None)
        .take(account_ids.len())
        .collect::<Vec<Option<QuotaBatchResult>>>();
    for (index, result) in indexed_results {
        if let Some(slot) = results.get_mut(index) {
            *slot = Some(result);
        }
    }
    results
        .into_iter()
        .enumerate()
        .map(|(index, result)| {
            result.unwrap_or_else(|| quota_batch_error(account_ids[index], "quota_batch_timeout"))
        })
        .collect()
}

/// Runs at most the configured number of jobs concurrently until the shared
/// deadline. Dropping the buffered stream cancels every unfinished future.
async fn collect_quota_batch_until<F, T>(
    jobs: impl IntoIterator<Item = F>,
    deadline: tokio::time::Instant,
) -> Vec<(usize, T)>
where
    F: std::future::Future<Output = (usize, T)>,
{
    if tokio::time::Instant::now() >= deadline {
        return Vec::new();
    }
    let mut jobs =
        Box::pin(futures_util::stream::iter(jobs).buffer_unordered(QUOTA_BATCH_CONCURRENCY));
    let timer = tokio::time::sleep_until(deadline);
    tokio::pin!(timer);
    let mut completed = Vec::new();
    loop {
        let next = tokio::select! {
            result = jobs.as_mut().next() => result,
            _ = &mut timer => None,
        };
        let Some(result) = next else {
            break;
        };
        completed.push(result);
    }
    drop(jobs);
    completed
}

fn validate_quota_batch_request(body: &QuotaBatchRequest) -> Result<(), AppError> {
    if body.account_ids.is_empty() || body.account_ids.len() > MAX_QUOTA_BATCH_ACCOUNTS {
        return Err(AppError::BadRequest(
            "quota batch must contain 1 to 100 account_ids".into(),
        ));
    }
    let mut unique = HashSet::with_capacity(body.account_ids.len());
    if !body.account_ids.iter().copied().all(|id| unique.insert(id)) {
        return Err(AppError::BadRequest(
            "quota batch account_ids must be unique".into(),
        ));
    }
    Ok(())
}

async fn read_quota_snapshot(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    tenant: &str,
    fresh: bool,
    trigger: crate::upstream_quota::QuotaReadTrigger,
) -> crate::upstream_quota::QuotaSnapshot {
    if fresh {
        state
            .upstream_quota
            .read_fresh(state, account, credential, tenant, trigger)
            .await
    } else {
        state
            .upstream_quota
            .read(state, account, credential, tenant, trigger)
            .await
    }
}

async fn quota_batch_result(
    state: AppState,
    account_id: Uuid,
    loaded: Option<Result<(UpstreamAccountView, UpstreamCredential), AppError>>,
    fresh: bool,
    trigger: crate::upstream_quota::QuotaReadTrigger,
) -> QuotaBatchResult {
    let Some(loaded) = loaded else {
        return quota_batch_error(account_id, "quota_account_not_found");
    };
    let (account, credential) = match loaded {
        Ok(loaded) => loaded,
        Err(error) => {
            tracing::warn!(
                upstream_account_id = %account_id,
                error_category = error.diagnostic_category(),
                "batch quota account credential could not be loaded"
            );
            return quota_batch_error(account_id, "credential_invalid");
        }
    };
    if account.status != "active" {
        return quota_batch_error(account_id, "quota_account_inactive");
    }
    let Some(tenant) = account.tenant_external_id.as_deref() else {
        return quota_batch_error(account_id, "quota_account_unavailable");
    };
    let snapshot = read_quota_snapshot(&state, &account, &credential, tenant, fresh, trigger).await;
    QuotaBatchResult::Success {
        upstream_account_id: account_id,
        snapshot: Box::new(snapshot),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct QuotaReadQuery {
    tenant_external_id: String,
    #[serde(default)]
    fresh: bool,
    #[serde(default)]
    trigger: Option<crate::upstream_quota::QuotaRequestTrigger>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct ResetConfirmation {
    confirmation_token: String,
    confirmation: String,
}

fn quota_reset_idempotency_key(headers: &HeaderMap) -> Result<&str, AppError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let value = values.next().ok_or_else(|| {
        AppError::BadRequest("exactly one Idempotency-Key is required for quota reset".into())
    })?;
    if values.next().is_some() {
        return Err(AppError::BadRequest(
            "exactly one Idempotency-Key is required for quota reset".into(),
        ));
    }
    let value = value.to_str().map_err(|_| {
        AppError::BadRequest(
            "Idempotency-Key must contain 1 to 200 visible ASCII characters".into(),
        )
    })?;
    if value.is_empty() || value.len() > 200 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(AppError::BadRequest(
            "Idempotency-Key must contain 1 to 200 visible ASCII characters".into(),
        ));
    }
    Ok(value)
}

async fn reset_account(
    state: &AppState,
    headers: &HeaderMap,
    account_id: Uuid,
    query: &QuotaQuery,
) -> Result<(UpstreamAccountView, UpstreamCredential, String), AppError> {
    let service = require_service(headers, state, "providers:write").await?;
    let tenant = query.tenant_external_id.trim();
    if tenant.is_empty() || tenant.len() > 200 {
        return Err(AppError::BadRequest(
            "reset requires explicit tenant".into(),
        ));
    }
    require_service_tenant(&service, tenant)?;
    state.db.require_upstream_tenant(account_id, tenant).await?;
    let (account, credential) = state
        .db
        .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
        .await?;
    let actor = service
        .service_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "bootstrap".into());
    Ok((account, credential, actor))
}

pub(in crate::api) async fn prepare_quota_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<QuotaQuery>,
) -> Result<impl IntoResponse, AppError> {
    let idempotency_key = quota_reset_idempotency_key(&headers)?;
    let (account, credential, actor) = reset_account(&state, &headers, account_id, &query).await?;
    let result = crate::upstream_quota::reset::prepare(
        &state,
        &account,
        &credential,
        query.tenant_external_id.trim(),
        &actor,
        idempotency_key,
    )
    .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(result)))
}

pub(in crate::api) async fn current_quota_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<QuotaQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    let tenant = query.tenant_external_id.trim();
    if tenant.is_empty() || tenant.len() > 200 {
        return Err(AppError::BadRequest(
            "reset requires explicit tenant".into(),
        ));
    }
    require_service_tenant(&service, tenant)?;
    state.db.require_upstream_tenant(account_id, tenant).await?;
    let result = state
        .db
        .current_quota_reset_operation_for_tenant(tenant, &account_id.to_string())
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(result)))
}

pub(in crate::api) async fn confirm_quota_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((account_id, operation_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<QuotaQuery>,
    Json(body): Json<ResetConfirmation>,
) -> Result<impl IntoResponse, AppError> {
    let idempotency_key = quota_reset_idempotency_key(&headers)?;
    if body.confirmation != "consume_one_supplier_reset_credit" {
        return Err(AppError::BadRequest(
            "confirmation must explicitly acknowledge one supplier reset credit".into(),
        ));
    }
    let (account, credential, actor) = reset_account(&state, &headers, account_id, &query).await?;
    let result = crate::upstream_quota::reset::confirm(
        &state,
        &account,
        &credential,
        query.tenant_external_id.trim(),
        &actor,
        &operation_id.to_string(),
        &body.confirmation_token,
        idempotency_key,
    )
    .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(result)))
}

pub(in crate::api) async fn reconcile_quota_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((account_id, operation_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<QuotaQuery>,
) -> Result<impl IntoResponse, AppError> {
    let (account, credential, actor) = reset_account(&state, &headers, account_id, &query).await?;
    let result = crate::upstream_quota::reset::reconcile(
        &state,
        &account,
        &credential,
        query.tenant_external_id.trim(),
        &actor,
        &operation_id.to_string(),
    )
    .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(result)))
}

pub(in crate::api) async fn get_quota_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((account_id, operation_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<QuotaQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    let tenant = query.tenant_external_id.trim();
    if tenant.is_empty() || tenant.len() > 200 {
        return Err(AppError::BadRequest(
            "reset requires explicit tenant".into(),
        ));
    }
    require_service_tenant(&service, tenant)?;
    let result = state
        .db
        .quota_reset_operation_for_tenant(
            tenant,
            &account_id.to_string(),
            &operation_id.to_string(),
        )
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(result)))
}

pub(in crate::api) async fn upstream_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<QuotaReadQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:read").await?;
    let tenant = query.tenant_external_id.trim();
    if tenant.is_empty() || tenant.len() > 200 {
        return Err(AppError::BadRequest(
            "quota requires an explicit tenant".into(),
        ));
    }
    require_service_tenant(&service, tenant)?;
    state.db.require_upstream_tenant(account_id, tenant).await?;
    let (account, credential) = state
        .db
        .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
        .await?;
    let trigger = query
        .trigger
        .unwrap_or(crate::upstream_quota::QuotaRequestTrigger::Manual)
        .into();
    let snapshot =
        read_quota_snapshot(&state, &account, &credential, tenant, query.fresh, trigger).await;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(snapshot)))
}

pub(in crate::api) async fn upstream_quota_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<QuotaBatchRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:read").await?;
    validate_quota_batch_request(&body)?;
    let QuotaBatchRequest {
        account_ids,
        fresh,
        trigger,
    } = body;
    let deadline = tokio::time::Instant::now() + QUOTA_BATCH_TOTAL_BUDGET;
    let loaded = match tokio::time::timeout_at(
        deadline,
        state.db.upstream_accounts_with_credentials_batch(
            &account_ids,
            service.tenant_external_id.as_deref(),
            state.config.key_pepper.as_bytes(),
        ),
    )
    .await
    {
        Ok(loaded) => loaded?,
        Err(_) => {
            return Ok((
                [(header::CACHE_CONTROL, "no-store")],
                Json(QuotaBatchResponse {
                    contract_version: "upstream_quota_batch_v1",
                    results: ordered_quota_batch_results(&account_ids, Vec::new()),
                }),
            ));
        }
    };
    let mut loaded = loaded
        .into_iter()
        .map(|item| (item.account_id, item.result))
        .collect::<HashMap<_, _>>();
    let trigger = trigger.into();
    let jobs = account_ids
        .iter()
        .copied()
        .enumerate()
        .map(|(index, account_id)| {
            let state = state.clone();
            let loaded = loaded.remove(&account_id);
            async move {
                (
                    index,
                    quota_batch_result(state, account_id, loaded, fresh, trigger).await,
                )
            }
        });
    // Do not let the first slow supplier hold the queue head hostage. The
    // shared deadline cancels stragglers while the result order remains stable.
    let indexed_results = collect_quota_batch_until(jobs, deadline).await;
    let results = ordered_quota_batch_results(&account_ids, indexed_results);
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(QuotaBatchResponse {
            contract_version: "upstream_quota_batch_v1",
            results,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    fn batch_error_code(result: &QuotaBatchResult) -> &str {
        match result {
            QuotaBatchResult::Error { error, .. } => error.code,
            QuotaBatchResult::Success { .. } => panic!("expected batch error"),
        }
    }

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn quota_batch_wire_contract_is_closed_bounded_and_tenant_free() {
        let first = Uuid::from_u128(1);
        let request: QuotaBatchRequest = serde_json::from_value(json!({
            "account_ids": [first],
            "fresh": true,
            "trigger": "bulk"
        }))
        .expect("valid batch request");
        validate_quota_batch_request(&request).expect("bounded unique batch");

        assert!(
            serde_json::from_value::<QuotaBatchRequest>(json!({
                "account_ids": [first],
                "fresh": true,
                "trigger": "bulk",
                "tenant_external_id": "caller-selected"
            }))
            .is_err()
        );
        let duplicate: QuotaBatchRequest = serde_json::from_value(json!({
            "account_ids": [first, first],
            "fresh": true,
            "trigger": "manual"
        }))
        .expect("wire shape is valid before semantic uniqueness validation");
        assert!(validate_quota_batch_request(&duplicate).is_err());
    }

    #[test]
    fn quota_batch_error_is_per_account_and_sanitized() {
        assert_eq!(
            serde_json::to_value(QuotaBatchResult::Error {
                upstream_account_id: Uuid::from_u128(2),
                error: QuotaBatchError {
                    code: "quota_account_unavailable"
                }
            })
            .expect("serialize batch result"),
            json!({
                "status": "error",
                "upstream_account_id": Uuid::from_u128(2),
                "error": { "code": "quota_account_unavailable" }
            })
        );
    }

    #[test]
    fn quota_batch_deadline_fills_a_large_request_without_reordering_completed_items() {
        let account_ids = (1..=MAX_QUOTA_BATCH_ACCOUNTS)
            .map(Uuid::from_u128)
            .collect::<Vec<_>>();
        let results = ordered_quota_batch_results(
            &account_ids,
            vec![
                (99, quota_batch_error(account_ids[99], "credential_invalid")),
                (
                    0,
                    quota_batch_error(account_ids[0], "quota_account_unavailable"),
                ),
            ],
        );

        assert_eq!(results.len(), MAX_QUOTA_BATCH_ACCOUNTS);
        assert_eq!(batch_error_code(&results[0]), "quota_account_unavailable");
        assert_eq!(batch_error_code(&results[1]), "quota_batch_timeout");
        assert_eq!(batch_error_code(&results[99]), "credential_invalid");
    }

    #[tokio::test]
    async fn quota_batch_deadline_keeps_completed_work_and_cancels_slow_work() {
        let dropped = Arc::new(AtomicBool::new(false));
        let slow_dropped = dropped.clone();
        let slow = async move {
            let _probe = DropProbe(slow_dropped);
            futures_util::future::pending::<(usize, &'static str)>().await
        };
        let jobs: Vec<Pin<Box<dyn Future<Output = (usize, &'static str)>>>> =
            vec![Box::pin(async { (0, "completed") }), Box::pin(slow)];

        let completed = collect_quota_batch_until(
            jobs,
            tokio::time::Instant::now() + Duration::from_millis(30),
        )
        .await;
        assert_eq!(completed, vec![(0, "completed")]);
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(QUOTA_BATCH_TOTAL_BUDGET, Duration::from_secs(270));
    }
}
