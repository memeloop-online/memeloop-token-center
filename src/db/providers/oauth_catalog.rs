use sqlx::Row;
use uuid::Uuid;

use crate::{
    db::unix_millis,
    error::AppError,
    oauth::managed::{codex, kimi},
    provider::{UpstreamCredential, open_credential},
};

fn same_refresh_identity(
    driver: &str,
    previous: &UpstreamCredential,
    next: &UpstreamCredential,
) -> bool {
    if next.validate(unix_millis()).is_err() || previous.adapter_state() != next.adapter_state() {
        return false;
    }
    match driver {
        "kimi-oauth" => {
            kimi::validate_credential(previous).is_ok() && kimi::validate_credential(next).is_ok()
        }
        "openai-codex" => {
            codex::account_header_value(previous).is_ok()
                && codex::account_header_value(next).is_ok()
        }
        _ => false,
    }
}

pub(super) async fn carry_forward_refresh_catalog(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    account_id: Uuid,
    account: &sqlx::any::AnyRow,
    credential: &UpstreamCredential,
    key_material: &[u8],
) -> Result<(), AppError> {
    let driver: String = account.try_get("driver")?;
    if !matches!(driver.as_str(), "kimi-oauth" | "openai-codex") {
        return Ok(());
    }
    let generation: i64 = account.try_get("credential_generation")?;
    let previous: Option<String> = sqlx::query_scalar(
        "SELECT credential_ciphertext FROM upstream_credentials WHERE upstream_account_id = $1 AND generation = $2 AND revoked_at IS NULL",
    )
    .bind(account_id.to_string())
    .bind(generation)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(previous) = previous else {
        return Ok(());
    };
    let previous = open_credential(&previous, key_material)?;
    if !same_refresh_identity(&driver, &previous, credential) {
        return Ok(());
    }
    let tenant_id: String = account.try_get("tenant_id")?;
    let snapshot_id = Uuid::now_v7().to_string();
    let copied = sqlx::query(
        "INSERT INTO upstream_model_catalog_snapshots (id, tenant_id, upstream_account_id, credential_generation, source_kind, fetched_at, model_count) SELECT $1, snapshot.tenant_id, snapshot.upstream_account_id, $2, snapshot.source_kind, snapshot.fetched_at, snapshot.model_count FROM upstream_model_catalog_state catalog JOIN upstream_model_catalog_snapshots snapshot ON snapshot.id = catalog.current_snapshot_id AND snapshot.tenant_id = catalog.tenant_id AND snapshot.upstream_account_id = catalog.upstream_account_id AND snapshot.credential_generation = catalog.credential_generation WHERE catalog.upstream_account_id = $3 AND catalog.tenant_id = $4 AND catalog.credential_generation = $5 AND catalog.status IN ('ready', 'stale', 'syncing')",
    )
    .bind(&snapshot_id)
    .bind(generation + 1)
    .bind(account_id.to_string())
    .bind(&tenant_id)
    .bind(generation)
    .execute(&mut **transaction)
    .await?;
    if copied.rows_affected() == 0 {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO upstream_models (snapshot_id, tenant_id, upstream_account_id, model_id, protocol, context_window, reservation_token_bound, reservation_bound_source, created_at) SELECT $1, model.tenant_id, model.upstream_account_id, model.model_id, model.protocol, model.context_window, model.reservation_token_bound, model.reservation_bound_source, model.created_at FROM upstream_models model JOIN upstream_model_catalog_state catalog ON catalog.current_snapshot_id = model.snapshot_id AND catalog.tenant_id = model.tenant_id AND catalog.upstream_account_id = model.upstream_account_id WHERE catalog.upstream_account_id = $2 AND catalog.tenant_id = $3 AND catalog.credential_generation = $4",
    )
    .bind(&snapshot_id)
    .bind(account_id.to_string())
    .bind(&tenant_id)
    .bind(generation)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "UPDATE upstream_model_catalog_state SET current_snapshot_id = $1, credential_generation = $2, status = 'stale', expires_at = $3, sync_lease_id = NULL, sync_lease_expires_at = NULL WHERE upstream_account_id = $4 AND tenant_id = $5 AND credential_generation = $6",
    )
    .bind(&snapshot_id)
    .bind(generation + 1)
    .bind(unix_millis())
    .bind(account_id.to_string())
    .bind(&tenant_id)
    .bind(generation)
    .execute(&mut **transaction)
    .await?;
    sqlx::query("DELETE FROM upstream_model_catalog_snapshots WHERE upstream_account_id = $1 AND tenant_id = $2 AND id <> $3")
        .bind(account_id.to_string())
        .bind(&tenant_id)
        .bind(&snapshot_id)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests;
