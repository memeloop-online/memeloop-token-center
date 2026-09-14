use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

use super::super::super::*;

pub(super) struct IdempotencyRecord {
    pub(super) request_hash: String,
    pub(super) event_id: String,
}

pub(super) async fn find_idempotency(
    tx: &mut Transaction<'_, Any>,
    account_id: Uuid,
    idempotency_key: &str,
) -> Result<Option<IdempotencyRecord>, AppError> {
    let Some(row) = sqlx::query(
        "SELECT request_hash, event_id FROM settlement_adjustment_idempotencies WHERE account_id = $1 AND idempotency_key = $2",
    )
    .bind(account_id.to_string())
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(IdempotencyRecord {
        request_hash: row.try_get("request_hash")?,
        event_id: row.try_get("event_id")?,
    }))
}

pub(super) async fn insert_idempotency(
    tx: &mut Transaction<'_, Any>,
    input: &ReconcileSettlementAdjustmentInput,
    request_hash: &str,
    event_id: &str,
) -> Result<(), AppError> {
    let inserted = sqlx::query(
        "INSERT INTO settlement_adjustment_idempotencies (account_id, idempotency_key, request_hash, event_id, created_at) VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
    )
    .bind(input.account_id.to_string())
    .bind(&input.idempotency_key)
    .bind(request_hash)
    .bind(event_id)
    .bind(unix_millis())
    .execute(&mut **tx)
    .await?;
    if inserted.rows_affected() == 1 {
        return Ok(());
    }
    let row = sqlx::query(
        "SELECT request_hash, event_id FROM settlement_adjustment_idempotencies WHERE account_id = $1 AND idempotency_key = $2",
    )
    .bind(input.account_id.to_string())
    .bind(&input.idempotency_key)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::Internal)?;
    let existing_hash: String = row.try_get("request_hash")?;
    let existing_event: String = row.try_get("event_id")?;
    if existing_hash == request_hash && existing_event == event_id {
        Ok(())
    } else {
        Err(AppError::Conflict(
            "Idempotency-Key was already used for a different settlement adjustment".into(),
        ))
    }
}

pub(super) async fn event_result(
    tx: &mut Transaction<'_, Any>,
    event_id: &str,
    replayed: bool,
) -> Result<SettlementAdjustmentReconcileResult, AppError> {
    let event = sqlx::query(
        "SELECT account_id, settlement_id, namespace, request_kind, request_id, currency, desired_rebate_micros, applied_delta_micros, cumulative_rebate_micros, version, ledger_entry_id, created_at FROM settlement_adjustment_events WHERE id = $1",
    )
    .bind(event_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::Internal)?;
    let account_id = parse_uuid(event.try_get("account_id")?)?;
    let settlement_id = parse_uuid(event.try_get("settlement_id")?)?;
    let request_kind = match event.try_get::<String, _>("request_kind")?.as_str() {
        "text" => AccountSettlementKind::Text,
        "generation" => AccountSettlementKind::Generation,
        _ => return Err(AppError::Internal),
    };
    let cumulative_rebate_micros: i64 = event.try_get("cumulative_rebate_micros")?;
    let gross_micros: i64 = sqlx::query_scalar(
        "SELECT gross_micros FROM settlement_adjustment_baselines WHERE account_id = $1 AND settlement_id = $2",
    )
    .bind(account_id.to_string())
    .bind(settlement_id.to_string())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::Internal)?;
    Ok(SettlementAdjustmentReconcileResult {
        account_id,
        settlement_id,
        namespace: event.try_get("namespace")?,
        request_kind,
        request_id: parse_uuid(event.try_get("request_id")?)?,
        currency: event.try_get("currency")?,
        desired_rebate_micros: event.try_get("desired_rebate_micros")?,
        applied_delta_micros: event.try_get("applied_delta_micros")?,
        cumulative_rebate_micros,
        remaining_rebate_micros: gross_micros
            .checked_sub(cumulative_rebate_micros)
            .ok_or(AppError::Internal)?,
        version: event.try_get("version")?,
        adjustment_entry_id: event
            .try_get::<Option<String>, _>("ledger_entry_id")?
            .map(parse_uuid)
            .transpose()?,
        event_id: parse_uuid(event_id.to_owned())?,
        created_at: event.try_get("created_at")?,
        replayed,
    })
}
