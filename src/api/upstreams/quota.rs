use super::super::*;
use crate::provider::UpstreamAccountView;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

const MAX_QUOTA_BATCH_ACCOUNTS: usize = 100;
const QUOTA_BATCH_CONCURRENCY: usize = 3;

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
        snapshot: crate::upstream_quota::QuotaSnapshot,
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
        return QuotaBatchResult::Error {
            upstream_account_id: account_id,
            error: QuotaBatchError {
                code: "quota_account_not_found",
            },
        };
    };
    let (account, credential) = match loaded {
        Ok(loaded) => loaded,
        Err(error) => {
            tracing::warn!(
                upstream_account_id = %account_id,
                error_category = error.diagnostic_category(),
                "batch quota account credential could not be loaded"
            );
            return QuotaBatchResult::Error {
                upstream_account_id: account_id,
                error: QuotaBatchError {
                    code: "credential_invalid",
                },
            };
        }
    };
    if account.status != "active" {
        return QuotaBatchResult::Error {
            upstream_account_id: account_id,
            error: QuotaBatchError {
                code: "quota_account_inactive",
            },
        };
    }
    let Some(tenant) = account.tenant_external_id.as_deref() else {
        return QuotaBatchResult::Error {
            upstream_account_id: account_id,
            error: QuotaBatchError {
                code: "quota_account_unavailable",
            },
        };
    };
    let snapshot = read_quota_snapshot(&state, &account, &credential, tenant, fresh, trigger).await;
    QuotaBatchResult::Success {
        upstream_account_id: account_id,
        snapshot,
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
    let loaded = state
        .db
        .upstream_accounts_with_credentials_batch(
            &body.account_ids,
            service.tenant_external_id.as_deref(),
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    let mut loaded = loaded
        .into_iter()
        .map(|item| (item.account_id, item.result))
        .collect::<HashMap<_, _>>();
    let fresh = body.fresh;
    let trigger = body.trigger.into();
    let jobs = body.account_ids.into_iter().enumerate().map(|(index, account_id)| {
        let state = state.clone();
        let loaded = loaded.remove(&account_id);
        async move {
            (
                index,
                quota_batch_result(state, account_id, loaded, fresh, trigger).await,
            )
        }
    });
    // Do not let the first slow supplier hold the queue head hostage. Results
    // still return in request order so callers can match them deterministically.
    let mut indexed_results = futures_util::stream::iter(jobs)
        .buffer_unordered(QUOTA_BATCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    indexed_results.sort_unstable_by_key(|(index, _)| *index);
    let results = indexed_results
        .into_iter()
        .map(|(_, result)| result)
        .collect();
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
}
