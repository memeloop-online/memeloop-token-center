use sha2::{Digest, Sha256};
use sqlx::{Row, Transaction};
use uuid::Uuid;

use super::super::*;

pub use crate::model::{ReconcileSettlementAdjustmentInput, SettlementAdjustmentReconcileResult};

impl Database {
    /// Reconciles one monotonically increasing desired rebate for an immutable
    /// settlement.  The account write lock serializes every namespace of the
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

        if let Some(row) = sqlx::query(
            "SELECT request_hash, event_id FROM settlement_adjustment_idempotencies WHERE account_id = $1 AND idempotency_key = $2",
        )
        .bind(input.account_id.to_string())
        .bind(&input.idempotency_key)
        .fetch_optional(&mut *tx)
        .await?
        {
            let existing_hash: String = row.try_get("request_hash")?;
            if existing_hash != request_hash {
                return Err(AppError::Conflict(
                    "Idempotency-Key was already used for a different settlement adjustment"
                        .into(),
                ));
            }
            let event_id: String = row.try_get("event_id")?;
            let result = event_result(&mut tx, &event_id, true).await?;
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

        let states = sqlx::query(
            "SELECT namespace, desired_rebate_micros, version, decision_digest, source, last_event_id FROM settlement_adjustment_states WHERE account_id = $1 AND settlement_id = $2",
        )
        .bind(input.account_id.to_string())
        .bind(input.settlement_id.to_string())
        .fetch_all(&mut *tx)
        .await?;
        let mut existing = None;
        let mut cumulative_before = 0_i64;
        for state in states {
            let namespace: String = state.try_get("namespace")?;
            let desired: i64 = state.try_get("desired_rebate_micros")?;
            cumulative_before = cumulative_before.checked_add(desired).ok_or_else(|| {
                AppError::Conflict("settlement adjustment cumulative amount overflowed".into())
            })?;
            if namespace == input.namespace {
                existing = Some(state);
            }
        }

        if let Some(state) = existing.as_ref() {
            let current_version: i64 = state.try_get("version")?;
            let current_desired: i64 = state.try_get("desired_rebate_micros")?;
            if input.version < current_version {
                return Err(AppError::Conflict(
                    "settlement adjustment version is older than current state".into(),
                ));
            }
            if input.version == current_version {
                let digest: String = state.try_get("decision_digest")?;
                let source: String = state.try_get("source")?;
                if current_desired != input.desired_rebate_micros
                    || digest != input.decision_digest
                    || source != input.source
                {
                    return Err(AppError::Conflict(
                        "settlement adjustment version has a different payload".into(),
                    ));
                }
                let event_id: String = state.try_get("last_event_id")?;
                insert_idempotency(&mut tx, &input, &request_hash, &event_id).await?;
                let result = event_result(&mut tx, &event_id, true).await?;
                tx.commit().await?;
                return Ok(result);
            }
            if input.desired_rebate_micros < current_desired {
                return Err(AppError::Conflict(
                    "automatic settlement adjustments cannot lower desired rebate".into(),
                ));
            }
        }

        let current_desired = existing
            .as_ref()
            .map(|state| state.try_get("desired_rebate_micros"))
            .transpose()?
            .unwrap_or(0_i64);
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
        // entitlement funding) is rebated first.  Only the excess is returned
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
        .execute(&mut *tx)
        .await?;
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

#[derive(Clone, Copy)]
struct Baseline {
    gross_micros: i64,
    original_entitlement_micros: i64,
}

fn settlement_kind_name(kind: AccountSettlementKind) -> &'static str {
    match kind {
        AccountSettlementKind::Text => "text",
        AccountSettlementKind::Generation => "generation",
    }
}

fn canonicalize_input(input: &mut ReconcileSettlementAdjustmentInput) {
    input.namespace = input.namespace.trim().to_owned();
    input.currency = input.currency.trim().to_ascii_uppercase();
    input.decision_digest = input.decision_digest.trim().to_ascii_lowercase();
    input.source = input.source.trim().to_owned();
    input.idempotency_key = input.idempotency_key.trim().to_owned();
}

fn validate_input(input: &ReconcileSettlementAdjustmentInput) -> Result<(), AppError> {
    validate_idempotency_key(&input.idempotency_key, "Idempotency-Key")?;
    validate_currency(&input.currency)?;
    let mut namespace_parts = input.namespace.split(':');
    let namespace_owner = namespace_parts.next().unwrap_or_default();
    let namespace_name = namespace_parts.next().unwrap_or_default();
    if namespace_parts.next().is_some()
        || !valid_namespace_part(namespace_owner)
        || !valid_namespace_part(namespace_name)
    {
        return Err(AppError::BadRequest(
            "namespace must match [a-z][a-z0-9-]{0,63}:[a-z][a-z0-9-]{0,63}".into(),
        ));
    }
    if input.version <= 0 || input.desired_rebate_micros < 0 {
        return Err(AppError::BadRequest(
            "desired rebate must be non-negative and version must be positive".into(),
        ));
    }
    if input.decision_digest.len() != 64
        || !input
            .decision_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(AppError::BadRequest(
            "decision_digest must be a lowercase SHA-256 hex digest".into(),
        ));
    }
    if input.source.is_empty()
        || input.source.len() > 200
        || input.source.chars().any(char::is_control)
    {
        return Err(AppError::BadRequest(
            "source must contain 1 to 200 non-control characters".into(),
        ));
    }
    Ok(())
}

fn valid_namespace_part(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn request_hash(input: &ReconcileSettlementAdjustmentInput) -> Result<String, AppError> {
    let canonical = serde_json::to_vec(input).map_err(|_| AppError::Internal)?;
    Ok(format!("{:x}", Sha256::digest(canonical)))
}

async fn ensure_baseline(
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
        "SELECT COALESCE(SUM(amount_micros), 0) FROM entitlement_usage_allocations WHERE usage_ledger_entry_id = $1",
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

async fn rollback_entitlement_tail(
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
        // Deliberately do not change entitlement/cycle status.  Allocation is
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

async fn insert_idempotency(
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

async fn event_result(
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
