use super::super::*;
use crate::db::{NativeCodexUpgradeReport, NativeCodexUpgradeTarget};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct PrepareNativeCodexUpgradeRequest {
    account_ids: Vec<Uuid>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct ApplyNativeCodexUpgradeRequest {
    targets: Vec<NativeCodexUpgradeTarget>,
}

/// Stage one of the native-only Codex transition. An operator must explicitly
/// list stable account ids; the response is a CAS plan that contains only ids,
/// versions, and a redacted proxy attestation.
pub(in crate::api) async fn prepare_native_codex_upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PrepareNativeCodexUpgradeRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    require_global_service(&service)?;
    let targets = state
        .db
        .prepare_native_codex_upgrade(&body.account_ids, state.config.key_pepper.as_bytes())
        .await?;
    tracing::info!(
        account_count = targets.len(),
        account_ids = ?targets.iter().map(|target| target.account_id).collect::<Vec<_>>(),
        "prepared native OpenAI Codex account migration"
    );
    Ok(Json(json!({
        "target_count": targets.len(),
        "targets": targets,
    })))
}

/// Stage two performs the transactional, version-checked conversion. It
/// cannot discover accounts by query or silently include a new account after
/// review. Repeating a completed request is idempotent and exposes no secret
/// material in either response or audit event.
pub(in crate::api) async fn apply_native_codex_upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ApplyNativeCodexUpgradeRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    require_global_service(&service)?;
    let report: NativeCodexUpgradeReport = state
        .db
        .apply_native_codex_upgrade(&body.targets, state.config.key_pepper.as_bytes())
        .await?;
    for account_id in &report.upgraded_account_ids {
        super::trigger_upstream_model_sync(state.clone(), *account_id);
    }
    tracing::info!(
        upgraded_count = report.upgraded_account_ids.len(),
        upgraded_account_ids = ?report.upgraded_account_ids,
        already_native_count = report.already_native_account_ids.len(),
        already_native_account_ids = ?report.already_native_account_ids,
        "applied native OpenAI Codex account migration"
    );
    Ok(Json(json!({
        "upgraded_count": report.upgraded_account_ids.len(),
        "upgraded_account_ids": report.upgraded_account_ids,
        "already_native_count": report.already_native_account_ids.len(),
        "already_native_account_ids": report.already_native_account_ids,
    })))
}
