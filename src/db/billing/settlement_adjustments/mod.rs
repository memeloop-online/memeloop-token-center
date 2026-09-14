mod entitlements;
mod idempotency;
mod state;
mod validation;

use sqlx::Row;
use uuid::Uuid;

use super::super::*;
use entitlements::rollback_entitlement_tail;
use idempotency::{event_result, find_idempotency, insert_idempotency};
use state::{ensure_baseline, load_states, persist_state};
use validation::{canonicalize_input, request_hash, settlement_kind_name, validate_input};

pub use crate::model::{ReconcileSettlementAdjustmentInput, SettlementAdjustmentReconcileResult};

impl Database {
    /// Reconciles one monotonically increasing desired rebate for an immutable
    /// settlement. The account write lock serializes every namespace of the
    /// settlement, which makes the cumulative gross-cost cap and entitlement
    /// allocation rollback atomic on SQLite and PostgreSQL.
    pub async fn reconcile_settlement_adjustment(
        &self,
        mut input: ReconcileSettlementAdjustmentInput,
    ) -> Result<SettlementAdjustmentReconcileResult, AppError> {
        canonicalize_input(&mut input);
        validate_input(&input)?;
        let request_hash = request_hash(&input)?;
        let mut tx = self.begin_write_transaction().await?;

        // This is a real PostgreSQL row lock and, together with BEGIN
        // IMMEDIATE, is the single lock that orders all adjustment namespaces
        // and ordinary account balance mutations.
        let account_locked =
            sqlx::query("UPDATE credit_accounts SET updated_at = updated_at WHERE id = $1")
                .bind(input.account_id.to_string())
                .execute(&mut *tx)
                .await?;
        if account_locked.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }

        if let Some(existing) =
            find_idempotency(&mut tx, input.account_id, &input.idempotency_key).await?
        {
            if existing.request_hash != request_hash {
                return Err(AppError::Conflict(
                    "Idempotency-Key was already used for a different settlement adjustment".into(),
                ));
            }
            let result = event_result(&mut tx, &existing.event_id, true).await?;
            tx.commit().await?;
            return Ok(result);
        }

        let settlement = sqlx::query(
            "SELECT request_kind, request_id, currency, cost_micros FROM account_settlement_feed WHERE account_id = $1 AND settlement_id = $2",
        )
        .bind(input.account_id.to_string())
        .bind(input.settlement_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let request_kind: String = settlement.try_get("request_kind")?;
        let request_id: String = settlement.try_get("request_id")?;
        let currency: String = settlement.try_get("currency")?;
        let gross_micros: i64 = settlement.try_get("cost_micros")?;
        if request_kind != settlement_kind_name(input.request_kind)
            || request_id != input.request_id.to_string()
            || !currency.eq_ignore_ascii_case(&input.currency)
        {
            return Err(AppError::Conflict(
                "adjustment request identity does not bind the settlement".into(),
            ));
        }
        if input.desired_rebate_micros > gross_micros {
            return Err(AppError::Conflict(
                "desired rebate cannot exceed the gross settlement cost".into(),
            ));
        }

        let baseline = ensure_baseline(&mut tx, &input, gross_micros, &currency).await?;
        let states = load_states(&mut tx, input.account_id, input.settlement_id).await?;
        let mut existing = None;
        let mut cumulative_before = 0_i64;
        for state in states {
            cumulative_before = cumulative_before
                .checked_add(state.desired_rebate_micros)
                .ok_or_else(|| {
                    AppError::Conflict("settlement adjustment cumulative amount overflowed".into())
                })?;
            if state.namespace == input.namespace {
                existing = Some(state);
            }
        }

        if let Some(state) = existing.as_ref() {
            if input.version < state.version {
                return Err(AppError::Conflict(
                    "settlement adjustment version is older than current state".into(),
                ));
            }
            if input.version == state.version {
                if state.desired_rebate_micros != input.desired_rebate_micros
                    || state.decision_digest != input.decision_digest
                    || state.source != input.source
                {
                    return Err(AppError::Conflict(
                        "settlement adjustment version has a different payload".into(),
                    ));
                }
                insert_idempotency(&mut tx, &input, &request_hash, &state.last_event_id).await?;
                let result = event_result(&mut tx, &state.last_event_id, true).await?;
                tx.commit().await?;
                return Ok(result);
            }
            if input.desired_rebate_micros < state.desired_rebate_micros {
                return Err(AppError::Conflict(
                    "automatic settlement adjustments cannot lower desired rebate".into(),
                ));
            }
        }

        let current_desired = existing
            .as_ref()
            .map(|state| state.desired_rebate_micros)
            .unwrap_or(0);
        let cumulative_rebate_micros = cumulative_before
            .checked_sub(current_desired)
            .and_then(|value| value.checked_add(input.desired_rebate_micros))
            .ok_or_else(|| {
                AppError::Conflict("settlement adjustment cumulative amount overflowed".into())
            })?;
        if cumulative_rebate_micros > baseline.gross_micros {
            return Err(AppError::Conflict(
                "settlement adjustment namespaces exceed gross settlement cost".into(),
            ));
        }
        let applied_delta_micros = input
            .desired_rebate_micros
            .checked_sub(current_desired)
            .ok_or(AppError::Internal)?;

        // Net-cost funding semantics: generic funding (gross - original
        // entitlement funding) is rebated first. Only the excess is returned
        // to entitlement consumption, from the original allocation tail.
        let non_entitlement_micros = baseline
            .gross_micros
            .checked_sub(baseline.original_entitlement_micros)
            .ok_or(AppError::Internal)?;
        let target_entitlement_rollback = cumulative_rebate_micros
            .checked_sub(non_entitlement_micros)
            .unwrap_or(0)
            .max(0);
        if target_entitlement_rollback > baseline.original_entitlement_micros {
            return Err(AppError::Internal);
        }
        let now = unix_millis();
        let event_id = Uuid::now_v7();
        let funding_reduction_micros = rollback_entitlement_tail(
            &mut tx,
            input.account_id,
            input.settlement_id,
            baseline.original_entitlement_micros,
            target_entitlement_rollback,
            event_id,
            now,
        )
        .await?;

        let net_account_delta_micros = applied_delta_micros
            .checked_sub(funding_reduction_micros)
            .ok_or_else(|| {
            AppError::Conflict(
                "settlement adjustment funding reduction exceeds rebate delta".into(),
            )
        })?;
        let adjustment_entry_id = if applied_delta_micros == 0 {
            None
        } else {
            let maximum_available =
                i64::MAX
                    .checked_sub(net_account_delta_micros)
                    .ok_or_else(|| {
                        AppError::Conflict(
                            "settlement adjustment would overflow account balance".into(),
                        )
                    })?;
            let account_updated = sqlx::query(
                "UPDATE credit_accounts SET available_micros = available_micros + $1, updated_at = $2 WHERE id = $3 AND available_micros <= $4",
            )
            .bind(net_account_delta_micros)
            .bind(now)
            .bind(input.account_id.to_string())
            .bind(maximum_available)
            .execute(&mut *tx)
            .await?;
            if account_updated.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "settlement adjustment would overflow account balance".into(),
                ));
            }
            let entry_id = Uuid::now_v7();
            let inserted = sqlx::query(
                "INSERT INTO ledger_entries (id, account_id, kind, amount_micros, currency, source, idempotency_key, reference_entry_id, created_at) VALUES ($1, $2, 'settlement_adjustment', $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING",
            )
            .bind(entry_id.to_string())
            .bind(input.account_id.to_string())
            .bind(applied_delta_micros)
            .bind(&input.currency)
            .bind(&input.source)
            .bind(format!("settlement-adjustment:{request_hash}"))
            .bind(input.settlement_id.to_string())
            .bind(now)
            .execute(&mut *tx)
            .await?;
            if inserted.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "Idempotency-Key was already used by another ledger entry".into(),
                ));
            }
            Some(entry_id)
        };

        sqlx::query(
            "INSERT INTO settlement_adjustment_events (id, account_id, settlement_id, namespace, request_kind, request_id, currency, desired_rebate_micros, applied_delta_micros, cumulative_rebate_micros, version, decision_digest, source, ledger_entry_id, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        )
        .bind(event_id.to_string())
        .bind(input.account_id.to_string())
        .bind(input.settlement_id.to_string())
        .bind(&input.namespace)
        .bind(settlement_kind_name(input.request_kind))
        .bind(input.request_id.to_string())
        .bind(&input.currency)
        .bind(input.desired_rebate_micros)
        .bind(applied_delta_micros)
        .bind(cumulative_rebate_micros)
        .bind(input.version)
        .bind(&input.decision_digest)
        .bind(&input.source)
        .bind(adjustment_entry_id.map(|id| id.to_string()))
        .bind(now)
        .execute(&mut *tx)
        .await?;
        persist_state(&mut tx, &input, event_id, applied_delta_micros, now).await?;
        insert_idempotency(&mut tx, &input, &request_hash, &event_id.to_string()).await?;

        let result = SettlementAdjustmentReconcileResult {
            account_id: input.account_id,
            settlement_id: input.settlement_id,
            namespace: input.namespace,
            request_kind: input.request_kind,
            request_id: input.request_id,
            currency: input.currency,
            desired_rebate_micros: input.desired_rebate_micros,
            applied_delta_micros,
            cumulative_rebate_micros,
            remaining_rebate_micros: baseline
                .gross_micros
                .checked_sub(cumulative_rebate_micros)
                .ok_or(AppError::Internal)?,
            version: input.version,
            adjustment_entry_id,
            event_id,
            created_at: now,
            replayed: false,
        };
        tx.commit().await?;
        Ok(result)
    }
}
