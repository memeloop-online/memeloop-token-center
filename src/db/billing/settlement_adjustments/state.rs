use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

use super::super::super::*;
use super::validation::settlement_kind_name;

#[derive(Clone, Copy)]
pub(super) struct Baseline {
    pub(super) gross_micros: i64,
    pub(super) original_entitlement_micros: i64,
}

pub(super) struct AdjustmentState {
    pub(super) namespace: String,
    pub(super) desired_rebate_micros: i64,
    pub(super) version: i64,
    pub(super) decision_digest: String,
    pub(super) source: String,
    pub(super) last_event_id: String,
}

pub(super) async fn ensure_baseline(
    tx: &mut Transaction<'_, Any>,
    input: &ReconcileSettlementAdjustmentInput,
    gross_micros: i64,
    currency: &str,
) -> Result<Baseline, AppError> {
    if let Some(row) = sqlx::query(
        "SELECT request_kind, request_id, currency, gross_micros, original_entitlement_micros FROM settlement_adjustment_baselines WHERE account_id = $1 AND settlement_id = $2",
    )
    .bind(input.account_id.to_string())
    .bind(input.settlement_id.to_string())
    .fetch_optional(&mut **tx)
    .await?
    {
        let stored_kind: String = row.try_get("request_kind")?;
        let stored_request_id: String = row.try_get("request_id")?;
        let stored_currency: String = row.try_get("currency")?;
        let stored_gross: i64 = row.try_get("gross_micros")?;
        if stored_kind != settlement_kind_name(input.request_kind)
            || stored_request_id != input.request_id.to_string()
            || !stored_currency.eq_ignore_ascii_case(currency)
            || stored_gross != gross_micros
        {
            return Err(AppError::Conflict(
                "settlement adjustment baseline no longer matches settlement".into(),
            ));
        }
        return Ok(Baseline {
            gross_micros: stored_gross,
            original_entitlement_micros: row.try_get("original_entitlement_micros")?,
        });
    }
    let original_entitlement_micros: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(amount_micros), 0) AS BIGINT) FROM entitlement_usage_allocations WHERE usage_ledger_entry_id = $1",
    )
    .bind(input.settlement_id.to_string())
    .fetch_one(&mut **tx)
    .await?;
    if original_entitlement_micros < 0 || original_entitlement_micros > gross_micros {
        return Err(AppError::Conflict(
            "settlement entitlement allocations do not match gross settlement cost".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO settlement_adjustment_baselines (account_id, settlement_id, request_kind, request_id, currency, gross_micros, original_entitlement_micros, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(input.account_id.to_string())
    .bind(input.settlement_id.to_string())
    .bind(settlement_kind_name(input.request_kind))
    .bind(input.request_id.to_string())
    .bind(currency)
    .bind(gross_micros)
    .bind(original_entitlement_micros)
    .bind(unix_millis())
    .execute(&mut **tx)
    .await?;
    Ok(Baseline {
        gross_micros,
        original_entitlement_micros,
    })
}

pub(super) async fn load_states(
    tx: &mut Transaction<'_, Any>,
    account_id: Uuid,
    settlement_id: Uuid,
) -> Result<Vec<AdjustmentState>, AppError> {
    let rows = sqlx::query(
        "SELECT namespace, desired_rebate_micros, version, decision_digest, source, last_event_id FROM settlement_adjustment_states WHERE account_id = $1 AND settlement_id = $2",
    )
    .bind(account_id.to_string())
    .bind(settlement_id.to_string())
    .fetch_all(&mut **tx)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(AdjustmentState {
                namespace: row.try_get("namespace")?,
                desired_rebate_micros: row.try_get("desired_rebate_micros")?,
                version: row.try_get("version")?,
                decision_digest: row.try_get("decision_digest")?,
                source: row.try_get("source")?,
                last_event_id: row.try_get("last_event_id")?,
            })
        })
        .collect()
}

pub(super) async fn persist_state(
    tx: &mut Transaction<'_, Any>,
    input: &ReconcileSettlementAdjustmentInput,
    event_id: Uuid,
    applied_delta_micros: i64,
    now: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO settlement_adjustment_states (account_id, settlement_id, namespace, desired_rebate_micros, version, decision_digest, source, last_event_id, last_applied_delta_micros, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ON CONFLICT(account_id, settlement_id, namespace) DO UPDATE SET desired_rebate_micros = excluded.desired_rebate_micros, version = excluded.version, decision_digest = excluded.decision_digest, source = excluded.source, last_event_id = excluded.last_event_id, last_applied_delta_micros = excluded.last_applied_delta_micros, updated_at = excluded.updated_at",
    )
    .bind(input.account_id.to_string())
    .bind(input.settlement_id.to_string())
    .bind(&input.namespace)
    .bind(input.desired_rebate_micros)
    .bind(input.version)
    .bind(&input.decision_digest)
    .bind(&input.source)
    .bind(event_id.to_string())
    .bind(applied_delta_micros)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
