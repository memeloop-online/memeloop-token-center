use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

use super::super::super::*;

pub(super) async fn rollback_entitlement_tail(
    tx: &mut Transaction<'_, Any>,
    account_id: Uuid,
    settlement_id: Uuid,
    original_entitlement_micros: i64,
    target_rollback_micros: i64,
    event_id: Uuid,
    now: i64,
) -> Result<i64, AppError> {
    let allocations = sqlx::query(
        "SELECT a.id, a.entitlement_cycle_id, a.amount_micros FROM entitlement_usage_allocations a JOIN entitlement_cycles c ON c.id = a.entitlement_cycle_id WHERE a.usage_ledger_entry_id = $1 AND a.amount_micros > 0 ORDER BY c.period_end DESC, c.id DESC, a.id DESC",
    )
    .bind(settlement_id.to_string())
    .fetch_all(&mut **tx)
    .await?;
    let remaining_entitlement_micros = allocations.iter().try_fold(0_i64, |total, row| {
        total
            .checked_add(row.try_get::<i64, _>("amount_micros")?)
            .ok_or_else(|| AppError::Conflict("settlement allocation amount overflowed".into()))
    })?;
    let current_rollback_micros = original_entitlement_micros
        .checked_sub(remaining_entitlement_micros)
        .ok_or_else(|| {
            AppError::Conflict("settlement allocations exceed their original amount".into())
        })?;
    if target_rollback_micros < current_rollback_micros {
        return Err(AppError::Conflict(
            "automatic settlement adjustments cannot reverse entitlement rollback".into(),
        ));
    }
    let mut to_rollback = target_rollback_micros - current_rollback_micros;
    let mut rollback_sequence = 0_i64;
    let mut funding_reduction_micros = 0_i64;
    for allocation in allocations {
        if to_rollback == 0 {
            break;
        }
        let allocation_id: String = allocation.try_get("id")?;
        let cycle_id: String = allocation.try_get("entitlement_cycle_id")?;
        let allocated: i64 = allocation.try_get("amount_micros")?;
        let rollback = allocated.min(to_rollback);
        // Deliberately do not change entitlement/cycle status. Allocation is
        // granted only to active, current, in-window cycles; returning prior
        // consumption must never reactivate a cancelled or expired right.
        let updated_cycle = sqlx::query(
            "UPDATE entitlement_cycles SET consumed_micros = consumed_micros - $1, updated_at = $2 WHERE id = $3 AND consumed_micros >= $1",
        )
        .bind(rollback)
        .bind(unix_millis())
        .bind(&cycle_id)
        .execute(&mut **tx)
        .await?;
        if updated_cycle.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "entitlement consumption is no longer reversible".into(),
            ));
        }
        let cycle = sqlx::query(
            "SELECT desired_micros, funded_micros, consumed_micros, status, currency FROM entitlement_cycles WHERE id = $1",
        )
        .bind(&cycle_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::Internal)?;
        let desired_micros: i64 = cycle.try_get("desired_micros")?;
        let funded_micros: i64 = cycle.try_get("funded_micros")?;
        let consumed_micros: i64 = cycle.try_get("consumed_micros")?;
        let status: String = cycle.try_get("status")?;
        let target_funded_micros = if status == "active" {
            desired_micros.max(consumed_micros)
        } else {
            consumed_micros
        };
        if target_funded_micros > funded_micros {
            return Err(AppError::Conflict(
                "entitlement funding no longer covers its consumption".into(),
            ));
        }
        let funding_reduction = funded_micros - target_funded_micros;
        if funding_reduction > 0 {
            let reduced = sqlx::query(
                "UPDATE entitlement_cycles SET funded_micros = $1, updated_at = $2 WHERE id = $3 AND funded_micros = $4",
            )
            .bind(target_funded_micros)
            .bind(now)
            .bind(&cycle_id)
            .bind(funded_micros)
            .execute(&mut **tx)
            .await?;
            if reduced.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "entitlement funding changed during settlement adjustment".into(),
                ));
            }
            sqlx::query(
                "INSERT INTO ledger_entries (id, account_id, kind, amount_micros, currency, source, idempotency_key, reference_entry_id, entitlement_cycle_id, created_at) VALUES ($1, $2, 'entitlement_adjustment', $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(account_id.to_string())
            .bind(-funding_reduction)
            .bind(cycle.try_get::<String, _>("currency")?)
            .bind(format!("settlement-adjustment:{event_id}"))
            .bind(format!("settlement-adjustment:funding:{event_id}:{cycle_id}"))
            .bind(settlement_id.to_string())
            .bind(&cycle_id)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            sqlx::query(
                "INSERT INTO settlement_adjustment_entitlement_funding_reductions (event_id, entitlement_cycle_id, amount_micros, created_at) VALUES ($1, $2, $3, $4)",
            )
            .bind(event_id.to_string())
            .bind(&cycle_id)
            .bind(funding_reduction)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            funding_reduction_micros = funding_reduction_micros
                .checked_add(funding_reduction)
                .ok_or_else(|| {
                    AppError::Conflict("settlement funding reduction overflowed".into())
                })?;
        }
        rollback_sequence = rollback_sequence.checked_add(1).ok_or(AppError::Internal)?;
        sqlx::query(
            "INSERT INTO settlement_adjustment_entitlement_rollbacks (event_id, rollback_sequence, entitlement_cycle_id, amount_micros, created_at) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(event_id.to_string())
        .bind(rollback_sequence)
        .bind(&cycle_id)
        .bind(rollback)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        if rollback == allocated {
            sqlx::query("DELETE FROM entitlement_usage_allocations WHERE id = $1")
                .bind(&allocation_id)
                .execute(&mut **tx)
                .await?;
        } else {
            sqlx::query(
                "UPDATE entitlement_usage_allocations SET amount_micros = amount_micros - $1 WHERE id = $2 AND amount_micros >= $1",
            )
            .bind(rollback)
            .bind(&allocation_id)
            .execute(&mut **tx)
            .await?;
        }
        to_rollback -= rollback;
    }
    if to_rollback != 0 {
        return Err(AppError::Conflict(
            "settlement allocation tail is no longer reversible".into(),
        ));
    }
    Ok(funding_reduction_micros)
}
