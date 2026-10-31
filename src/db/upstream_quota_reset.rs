use super::*;

#[derive(Clone, Serialize)]
pub(crate) struct QuotaResetAuditEvent {
    pub id: String,
    pub event: String,
    pub actor_service_id: Option<String>,
    pub error_code: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Serialize)]
pub(crate) struct QuotaResetOperation {
    pub id: String,
    pub upstream_account_id: String,
    pub state: String,
    pub credential_generation: i64,
    pub available_credits: i64,
    pub applicable_credits: i64,
    pub observed_at: i64,
    pub expires_at: i64,
    pub prepared_by_service_id: String,
    pub confirmed_by_service_id: Option<String>,
    pub confirmed_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_reconciled_at: Option<i64>,
    pub reconciled_available_credits: Option<i64>,
    pub reconciled_applicable_credits: Option<i64>,
    pub settled_at: Option<i64>,
    pub error_code: Option<String>,
    pub audit: Vec<QuotaResetAuditEvent>,
    pub dispatch_diagnostic: Option<QuotaResetDispatchDiagnostic>,
    pub quota_refresh_status: &'static str,
    pub manual_unlock_available: bool,
    /// The supplier decides which Codex rate limits a credit resets.
    pub effect: &'static str,
    pub consumes_credits: i64,
}

/// Closed diagnostic vocabulary; never serialize raw HTTP errors or bodies.
#[derive(Clone, Serialize)]
pub(crate) struct QuotaResetDispatchDiagnostic {
    pub stage: &'static str,
    pub reason: &'static str,
    pub supplier_http_status: Option<u16>,
}

impl QuotaResetDispatchDiagnostic {
    fn from_code(code: Option<&str>) -> Option<Self> {
        let code = code?;
        let (stage, reason, status) = match code {
            "reset_prepare_credential_invalid" => ("prepare", "credential_invalid", None),
            "reset_send_connect_timeout" => ("send", "connect_timeout", None),
            "reset_send_connect_failed" => ("send", "connect_failed", None),
            "reset_send_timeout" => ("send", "timeout", None),
            "reset_send_request_invalid" => ("send", "request_invalid", None),
            "reset_send_transport_failed" => ("send", "transport_failed", None),
            "reset_dispatch_unknown" => ("send", "unavailable", None),
            "reset_supplier_response_unknown" => ("response", "unavailable", None),
            _ => {
                let status = code
                    .strip_prefix("reset_response_http_")?
                    .parse::<u16>()
                    .ok()?;
                if !(100..=599).contains(&status) {
                    return None;
                }
                ("response", "supplier_http_status", Some(status))
            }
        };
        Some(Self {
            stage,
            reason,
            supplier_http_status: status,
        })
    }
}

pub(crate) struct PrepareQuotaReset {
    pub id: Uuid,
    pub account: UpstreamAccountView,
    pub actor: String,
    pub confirmation_hash: String,
    pub prepare_idempotency_hash: String,
    pub available: i64,
    pub applicable: i64,
    pub observed_at: i64,
}

pub(crate) struct PrepareQuotaResetResult {
    pub operation: QuotaResetOperation,
    pub replayed: bool,
}

pub(crate) enum QuotaResetClaim {
    Claimed { redeem_request_id: String },
    Replayed(Box<QuotaResetOperation>),
}

#[allow(clippy::too_many_arguments)]
async fn insert_audit(
    tx: &mut Transaction<'_, Any>,
    operation_id: &str,
    tenant: &str,
    account: &str,
    event: &str,
    actor: Option<&str>,
    error_code: Option<&str>,
    created_at: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO upstream_quota_reset_audit (
             id, operation_id, tenant_id, upstream_account_id, event,
             actor_service_id, error_code, created_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(operation_id)
    .bind(tenant)
    .bind(account)
    .bind(event)
    .bind(actor)
    .bind(error_code)
    .bind(created_at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Database {
    pub(crate) async fn quota_reset_prepare_replay(
        &self,
        account: &UpstreamAccountView,
        actor: &str,
        prepare_idempotency_hash: &str,
    ) -> Result<Option<QuotaResetOperation>, AppError> {
        let tenant = account.tenant_id.to_string();
        let account_id = account.id.to_string();
        let id = sqlx::query_scalar::<_, String>(
            "SELECT id FROM upstream_quota_reset_operations
             WHERE tenant_id = $1 AND upstream_account_id = $2
               AND actor_service_id = $3 AND prepare_idempotency_hash = $4",
        )
        .bind(&tenant)
        .bind(&account_id)
        .bind(actor)
        .bind(prepare_idempotency_hash)
        .fetch_optional(&self.pool)
        .await?;
        match id {
            Some(id) => self
                .quota_reset_operation(&tenant, &account_id, &id)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    pub(crate) async fn quota_reset_operation_for_tenant(
        &self,
        tenant_external: &str,
        account: &str,
        id: &str,
    ) -> Result<QuotaResetOperation, AppError> {
        let row = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(tenant_external)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)?;
        let tenant: String = row.try_get("id")?;
        self.quota_reset_operation(&tenant, account, id).await
    }

    pub(crate) async fn current_quota_reset_operation(
        &self,
        tenant: &str,
        account: &str,
    ) -> Result<Option<QuotaResetOperation>, AppError> {
        let now = unix_millis();
        sqlx::query(
            "UPDATE upstream_quota_reset_operations
             SET state = 'expired', updated_at = $1
             WHERE tenant_id = $2 AND upstream_account_id = $3
               AND state = 'prepared' AND expires_at <= $1",
        )
        .bind(now)
        .bind(tenant)
        .bind(account)
        .execute(&self.pool)
        .await?;
        let id = sqlx::query_scalar::<_, String>(
            "SELECT id FROM upstream_quota_reset_operations
             WHERE tenant_id = $1 AND upstream_account_id = $2
               AND state IN ('prepared', 'submitted', 'accepted', 'unknown')
               AND settled_at IS NULL
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )
        .bind(tenant)
        .bind(account)
        .fetch_optional(&self.pool)
        .await?;
        match id {
            Some(id) => self
                .quota_reset_operation(tenant, account, &id)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    pub(crate) async fn current_quota_reset_operation_for_tenant(
        &self,
        tenant_external: &str,
        account: &str,
    ) -> Result<Option<QuotaResetOperation>, AppError> {
        let row = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(tenant_external)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)?;
        let tenant: String = row.try_get("id")?;
        self.current_quota_reset_operation(&tenant, account).await
    }

    pub(crate) async fn prepare_quota_reset(
        &self,
        input: PrepareQuotaReset,
    ) -> Result<PrepareQuotaResetResult, AppError> {
        let now = unix_millis();
        let id = input.id.to_string();
        let account = input.account.id.to_string();
        let tenant = input.account.tenant_id.to_string();
        let mut tx = self.pool.begin().await?;
        // Expiration cannot reopen submitted/unknown operations.
        sqlx::query(
            "UPDATE upstream_quota_reset_operations
             SET state = 'expired', updated_at = $1
             WHERE upstream_account_id = $2 AND state = 'prepared' AND expires_at <= $1",
        )
        .bind(now)
        .bind(&account)
        .execute(&mut *tx)
        .await?;
        let inserted = sqlx::query(
            "INSERT INTO upstream_quota_reset_operations (
                 id, tenant_id, upstream_account_id, credential_generation,
                 transport_updated_at, actor_service_id, confirmation_hash,
                 redeem_request_id, state, available_credits, applicable_credits,
                 observed_at, expires_at, created_at, updated_at,
                 prepare_idempotency_hash
             ) VALUES (
                 $1, $2, $3, $4, $5, $6, $7, $8, 'prepared', $9, $10,
                 $11, $12, $13, $13, $14
             ) ON CONFLICT DO NOTHING",
        )
        .bind(&id)
        .bind(&tenant)
        .bind(&account)
        .bind(input.account.credential_generation)
        .bind(input.account.updated_at)
        .bind(&input.actor)
        .bind(input.confirmation_hash)
        .bind(Uuid::now_v7().to_string())
        .bind(input.available)
        .bind(input.applicable)
        .bind(input.observed_at)
        .bind(now + 120_000)
        .bind(now)
        .bind(&input.prepare_idempotency_hash)
        .execute(&mut *tx)
        .await?;
        let replayed = inserted.rows_affected() == 0;
        let operation_id = if replayed {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM upstream_quota_reset_operations
                 WHERE tenant_id = $1 AND upstream_account_id = $2
                   AND actor_service_id = $3 AND prepare_idempotency_hash = $4",
            )
            .bind(&tenant)
            .bind(&account)
            .bind(&input.actor)
            .bind(&input.prepare_idempotency_hash)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| {
                AppError::Conflict(
                    "an unresolved quota reset operation already exists for this account".into(),
                )
            })?
        } else {
            insert_audit(
                &mut tx,
                &id,
                &tenant,
                &account,
                "prepared",
                Some(&input.actor),
                None,
                now,
            )
            .await?;
            id
        };
        tx.commit().await?;
        Ok(PrepareQuotaResetResult {
            operation: self
                .quota_reset_operation(&tenant, &account, &operation_id)
                .await?,
            replayed,
        })
    }

    pub(crate) async fn quota_reset_operation(
        &self,
        tenant: &str,
        account: &str,
        id: &str,
    ) -> Result<QuotaResetOperation, AppError> {
        let now = unix_millis();
        sqlx::query(
            "UPDATE upstream_quota_reset_operations
             SET state = 'expired', updated_at = $1
             WHERE tenant_id = $2 AND upstream_account_id = $3 AND id = $4
               AND state = 'prepared' AND expires_at <= $1",
        )
        .bind(now)
        .bind(tenant)
        .bind(account)
        .bind(id)
        .execute(&self.pool)
        .await?;
        let row = sqlx::query(
            "SELECT id, upstream_account_id, state, credential_generation,
                    available_credits, applicable_credits, observed_at, expires_at,
                    actor_service_id, confirmed_by_service_id, confirmed_at,
                    created_at, updated_at, last_reconciled_at,
                    reconciled_available_credits, reconciled_applicable_credits,
                    settled_at, error_code
             FROM upstream_quota_reset_operations
             WHERE tenant_id = $1 AND upstream_account_id = $2 AND id = $3",
        )
        .bind(tenant)
        .bind(account)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let audit = sqlx::query(
            "SELECT id, event, actor_service_id, error_code, created_at
             FROM (
                 SELECT id, event, actor_service_id, error_code, created_at
                 FROM upstream_quota_reset_audit
                 WHERE operation_id = $1 AND tenant_id = $2 AND upstream_account_id = $3
                 ORDER BY created_at DESC, id DESC
                 LIMIT 32
             ) recent_audit
             ORDER BY created_at, id",
        )
        .bind(id)
        .bind(tenant)
        .bind(account)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| -> Result<QuotaResetAuditEvent, sqlx::Error> {
            Ok(QuotaResetAuditEvent {
                id: row.try_get("id")?,
                event: row.try_get("event")?,
                actor_service_id: row.try_get("actor_service_id")?,
                error_code: row.try_get("error_code")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
        let state: String = row.try_get("state")?;
        let settled_at: Option<i64> = row.try_get("settled_at")?;
        let error_code: Option<String> = row.try_get("error_code")?;
        let dispatch_diagnostic = QuotaResetDispatchDiagnostic::from_code(error_code.as_deref());
        // Display pagination must not discard the durable readback outcome.
        let latest_readback = sqlx::query_scalar::<_, String>(
            "SELECT error_code FROM upstream_quota_reset_audit
             WHERE operation_id = $1 AND tenant_id = $2 AND upstream_account_id = $3
               AND event = 'reconciled'
               AND (error_code = 'reset_readback_succeeded'
                    OR substr(error_code, 1, 23) = 'reset_readback_pending_')
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )
        .bind(id)
        .bind(tenant)
        .bind(account)
        .fetch_optional(&self.pool)
        .await?;
        let quota_refresh_status = match latest_readback.as_deref() {
            Some("reset_readback_succeeded") => "succeeded",
            Some(_) => "pending",
            None if state == "accepted" => "pending",
            None => "not_attempted",
        };
        let manual_unlock_available = state == "unknown" && settled_at.is_none();
        Ok(QuotaResetOperation {
            id: row.try_get("id")?,
            upstream_account_id: row.try_get("upstream_account_id")?,
            state,
            credential_generation: row.try_get("credential_generation")?,
            available_credits: row.try_get("available_credits")?,
            applicable_credits: row.try_get("applicable_credits")?,
            observed_at: row.try_get("observed_at")?,
            expires_at: row.try_get("expires_at")?,
            prepared_by_service_id: row.try_get("actor_service_id")?,
            confirmed_by_service_id: row.try_get("confirmed_by_service_id")?,
            confirmed_at: row.try_get("confirmed_at")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            last_reconciled_at: row.try_get("last_reconciled_at")?,
            reconciled_available_credits: row.try_get("reconciled_available_credits")?,
            reconciled_applicable_credits: row.try_get("reconciled_applicable_credits")?,
            settled_at,
            error_code,
            dispatch_diagnostic,
            quota_refresh_status,
            manual_unlock_available,
            audit,
            effect: "supplier_defined_codex_rate_limits",
            consumes_credits: 1,
        })
    }

    /// CAS is committed before dispatch. An exact confirmation replay returns
    /// durable state and can never send a second supplier request.
    pub(crate) async fn claim_quota_reset(
        &self,
        account: &UpstreamAccountView,
        id: &str,
        actor: &str,
        confirmation_hash: &str,
        idempotency_hash: &str,
    ) -> Result<QuotaResetClaim, AppError> {
        let now = unix_millis();
        let tenant = account.tenant_id.to_string();
        let account_id = account.id.to_string();
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE upstream_quota_reset_operations
             SET state = 'submitted', confirm_idempotency_hash = $1,
                 confirmed_by_service_id = $2, confirmed_at = $3, updated_at = $3
             WHERE id = $4 AND tenant_id = $5 AND upstream_account_id = $6
               AND state = 'prepared' AND expires_at > $3
               AND credential_generation = $7 AND transport_updated_at = $8
               AND actor_service_id = $2 AND confirmation_hash = $9
               AND EXISTS (
                 SELECT 1 FROM upstream_accounts account
                 WHERE account.id = upstream_account_id AND account.tenant_id = $5
                   AND account.credential_generation = $7 AND account.updated_at = $8
               )",
        )
        .bind(idempotency_hash)
        .bind(actor)
        .bind(now)
        .bind(id)
        .bind(&tenant)
        .bind(&account_id)
        .bind(account.credential_generation)
        .bind(account.updated_at)
        .bind(confirmation_hash)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() == 0 {
            let exact_replay = sqlx::query_scalar::<_, String>(
                "SELECT state FROM upstream_quota_reset_operations
                 WHERE id = $1 AND tenant_id = $2 AND upstream_account_id = $3
                   AND actor_service_id = $4 AND confirmation_hash = $5
                   AND confirmed_by_service_id = $4 AND confirm_idempotency_hash = $6
                   AND state IN ('submitted', 'accepted', 'unknown')",
            )
            .bind(id)
            .bind(&tenant)
            .bind(&account_id)
            .bind(actor)
            .bind(confirmation_hash)
            .bind(idempotency_hash)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            if exact_replay.is_some() {
                return Ok(QuotaResetClaim::Replayed(Box::new(
                    self.quota_reset_operation(&tenant, &account_id, id).await?,
                )));
            }
            return Err(AppError::Conflict(
                "reset confirmation is expired, mismatched, or uses another Idempotency-Key".into(),
            ));
        }
        let redeem_request_id: String = sqlx::query_scalar(
            "SELECT redeem_request_id FROM upstream_quota_reset_operations WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        insert_audit(
            &mut tx,
            id,
            &tenant,
            &account_id,
            "confirmation_claimed",
            Some(actor),
            None,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(QuotaResetClaim::Claimed { redeem_request_id })
    }

    pub(crate) async fn finish_quota_reset(
        &self,
        id: &str,
        accepted: bool,
        error: Option<&str>,
    ) -> Result<(), AppError> {
        let now = unix_millis();
        let state = if accepted { "accepted" } else { "unknown" };
        let event = if accepted {
            "dispatch_accepted"
        } else {
            "dispatch_unknown"
        };
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE upstream_quota_reset_operations
             SET state = $1, error_code = $2, updated_at = $3
             WHERE id = $4 AND state = 'submitted'",
        )
        .bind(state)
        .bind(error)
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() == 1 {
            let row = sqlx::query(
                "SELECT tenant_id, upstream_account_id, confirmed_by_service_id
                 FROM upstream_quota_reset_operations WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let tenant: String = row.try_get("tenant_id")?;
            let account: String = row.try_get("upstream_account_id")?;
            let actor: Option<String> = row.try_get("confirmed_by_service_id")?;
            insert_audit(
                &mut tx,
                id,
                &tenant,
                &account,
                event,
                actor.as_deref(),
                error,
                now,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn record_quota_reset_readback(
        &self,
        account: &UpstreamAccountView,
        operation: &str,
        actor: &str,
        error: Option<&str>,
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        let code = error
            .map(|error| format!("reset_readback_pending_{error}"))
            .unwrap_or_else(|| "reset_readback_succeeded".into());
        insert_audit(
            &mut tx,
            operation,
            &account.tenant_id.to_string(),
            &account.id.to_string(),
            "reconciled",
            Some(actor),
            Some(&code),
            unix_millis(),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Explicit human authorization releases only the lock. The historical
    /// unknown result, baseline, dispatch error and confirmation remain intact.
    /// A subsequent reset must have a new prepare key and confirmation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn release_unknown_quota_reset(
        &self,
        account: &UpstreamAccountView,
        operation: &str,
        actor: &str,
        expected_updated_at: i64,
        reason: &str,
        available: i64,
        applicable: i64,
        observed_at: i64,
    ) -> Result<(), AppError> {
        if !matches!(
            reason,
            "supplier_confirmed_not_applied" | "operator_authorized_retry"
        ) {
            return Err(AppError::BadRequest("unknown recovery reason".into()));
        }
        let tenant = account.tenant_id.to_string();
        let account_id = account.id.to_string();
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE upstream_quota_reset_operations
             SET settled_at = $1, updated_at = $1, last_reconciled_at = $2,
                 reconciled_available_credits = $3, reconciled_applicable_credits = $4
             WHERE id = $5 AND tenant_id = $6 AND upstream_account_id = $7
               AND state = 'unknown' AND settled_at IS NULL AND updated_at = $8
               AND $2 >= updated_at AND $3 >= 0 AND $4 >= 0
               AND EXISTS (
                 SELECT 1 FROM upstream_accounts account
                 WHERE account.id = upstream_account_id AND account.tenant_id = $6
                   AND account.credential_generation = $9 AND account.updated_at = $10
               )",
        )
        .bind(now)
        .bind(observed_at)
        .bind(available)
        .bind(applicable)
        .bind(operation)
        .bind(&tenant)
        .bind(&account_id)
        .bind(expected_updated_at)
        .bind(account.credential_generation)
        .bind(account.updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "unknown reset changed or is already unlocked; reload before reviewing recovery"
                    .into(),
            ));
        }
        insert_audit(
            &mut tx,
            operation,
            &tenant,
            &account_id,
            "reconciled",
            Some(actor),
            Some(&format!("manual_lock_released_{reason}")),
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn reconcile_quota_reset(
        &self,
        tenant: &str,
        account: &str,
        id: &str,
        actor: &str,
        credential_generation: i64,
        transport_updated_at: i64,
        available: i64,
        applicable: i64,
        observed_at: i64,
    ) -> Result<(), AppError> {
        // Read evidence alone cannot attribute a changed balance to this
        // operation. In particular, unknown never becomes accepted here. A
        // supplier-accepted operation may release its account lock once the
        // old available balance no longer applies. Credential generation and
        // transport revision are dispatch fences, not settlement fences: an
        // OAuth refresh or proxy edit must not hide later supplier evidence.
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE upstream_quota_reset_operations
             SET reconciled_available_credits = $1,
                 reconciled_applicable_credits = $2,
                 last_reconciled_at = $3,
                 settled_at = CASE
                     WHEN state = 'accepted' AND $1 < available_credits THEN $3
                     ELSE settled_at
                 END,
                 updated_at = $3
             WHERE tenant_id = $4 AND upstream_account_id = $5 AND id = $6
               AND settled_at IS NULL
               AND EXISTS (
                 SELECT 1 FROM upstream_accounts account
                 WHERE account.id = upstream_account_id AND account.tenant_id = $4
                   AND account.credential_generation = $7 AND account.updated_at = $8
               )
               AND (
                 (state = 'accepted' AND $1 < available_credits AND $3 > observed_at)
                 OR (
                   state IN ('submitted', 'accepted', 'unknown')
                   AND credential_generation = $7 AND transport_updated_at = $8
                 )
               )
             RETURNING state, settled_at",
        )
        .bind(available)
        .bind(applicable)
        .bind(observed_at)
        .bind(tenant)
        .bind(account)
        .bind(id)
        .bind(credential_generation)
        .bind(transport_updated_at)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(changed) = changed else {
            return Err(AppError::Conflict(
                "only the original dispatch scope or a later accepted-credit decrease can be reconciled".into(),
            ));
        };
        let settled_by_credit_decrease = changed.try_get::<String, _>("state")? == "accepted"
            && changed.try_get::<Option<i64>, _>("settled_at")? == Some(observed_at);
        insert_audit(
            &mut tx,
            id,
            tenant,
            account,
            "reconciled",
            Some(actor),
            settled_by_credit_decrease.then_some("accepted_credit_decrease_observed"),
            unix_millis(),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn settle_accepted_quota_reset_from_observation(
        &self,
        account: &UpstreamAccountView,
        available: i64,
        applicable: i64,
        observed_at: i64,
    ) -> Result<bool, AppError> {
        let account_id = account.id.to_string();
        let tenant = account.tenant_id.to_string();
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT id
             FROM upstream_quota_reset_operations
             WHERE tenant_id = $1 AND upstream_account_id = $2
               AND state = 'accepted' AND settled_at IS NULL
               AND available_credits > $3
               AND observed_at < $4
               AND EXISTS (
                 SELECT 1 FROM upstream_accounts account
                 WHERE account.id = upstream_account_id AND account.tenant_id = $1
                   AND account.credential_generation = $5 AND account.updated_at = $6
               )
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )
        .bind(&tenant)
        .bind(&account_id)
        .bind(available)
        .bind(observed_at)
        .bind(account.credential_generation)
        .bind(account.updated_at)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(false);
        };
        let id: String = row.try_get("id")?;
        let changed = sqlx::query(
            "UPDATE upstream_quota_reset_operations
             SET reconciled_available_credits = $1,
                 reconciled_applicable_credits = $2,
                 last_reconciled_at = $3, settled_at = $3, updated_at = $3
             WHERE id = $4 AND tenant_id = $5 AND upstream_account_id = $6
               AND state = 'accepted' AND settled_at IS NULL
               AND available_credits > $1 AND observed_at < $7
               AND EXISTS (
                 SELECT 1 FROM upstream_accounts account
                 WHERE account.id = upstream_account_id AND account.tenant_id = $5
                   AND account.credential_generation = $8 AND account.updated_at = $9
               )",
        )
        .bind(available)
        .bind(applicable)
        .bind(observed_at)
        .bind(&id)
        .bind(&tenant)
        .bind(&account_id)
        .bind(observed_at)
        .bind(account.credential_generation)
        .bind(account.updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() == 1 {
            insert_audit(
                &mut tx,
                &id,
                &tenant,
                &account_id,
                "reconciled",
                None,
                Some("accepted_credit_decrease_observed"),
                unix_millis(),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(changed.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_dispatch_diagnostic_is_allowlisted_and_legacy_safe() {
        for code in [
            "reset_send_connect_timeout",
            "reset_send_connect_failed",
            "reset_send_timeout",
            "reset_send_request_invalid",
            "reset_send_transport_failed",
            "reset_prepare_credential_invalid",
            "reset_dispatch_unknown",
            "reset_supplier_response_unknown",
        ] {
            assert!(QuotaResetDispatchDiagnostic::from_code(Some(code)).is_some());
        }
        let status =
            QuotaResetDispatchDiagnostic::from_code(Some("reset_response_http_503")).unwrap();
        assert_eq!(status.stage, "response");
        assert_eq!(status.supplier_http_status, Some(503));
        for code in [
            "https://user:secret@proxy.invalid/",
            "reset_response_http_999",
            "reset_response_http_401?access_token=secret",
            "reset_send_secret",
        ] {
            assert!(QuotaResetDispatchDiagnostic::from_code(Some(code)).is_none());
        }
    }

    #[tokio::test]
    async fn sqlite_reset_is_idempotent_audited_and_unknown_blocks_new_operations() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("reset.db").display()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        contract(&db).await;
    }

    #[tokio::test]
    async fn postgres_reset_is_idempotent_audited_and_unknown_blocks_new_operations() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let db = Database::connect(&url).await.unwrap();
        db.migrate().await.unwrap();
        contract(&db).await;
    }

    async fn contract(db: &Database) {
        let now = unix_millis();
        let tenant = Uuid::now_v7();
        let account = UpstreamAccountView {
            id: Uuid::now_v7(),
            tenant_id: tenant,
            tenant_external_id: Some(format!("quota-reset-{tenant}")),
            name: "quota fixture".into(),
            driver: "openai-codex".into(),
            auth_kind: "oauth".into(),
            connection_method: "oauth".into(),
            notes: None,
            credential_generation: 1,
            status: "active".into(),
            config: serde_json::json!({}),
            credential_expires_at: None,
            has_proxy: false,
            proxy_scheme: None,
            proxy_remote_dns: false,
            proxy_label: None,
            proxy_fingerprint: None,
            can_update_transport_proxy: true,
            import_source_identity_hash: None,
            import_source_document_sha256: None,
            can_refresh: true,
            can_rotate: false,
            can_reauthorize: true,
            route_count: 0,
            created_at: now,
            updated_at: now,
        };
        sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3)")
            .bind(tenant.to_string())
            .bind(account.tenant_external_id.as_ref().unwrap())
            .bind(now)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, created_at, updated_at) VALUES ($1, $2, 'quota fixture', 'openai-codex', 'oauth', '{}', 'active', 1, $3, $3)")
            .bind(account.id.to_string()).bind(tenant.to_string()).bind(now).execute(&db.pool).await.unwrap();
        let input = |idempotency: &str| PrepareQuotaReset {
            id: Uuid::now_v7(),
            account: account.clone(),
            actor: "actor".into(),
            confirmation_hash: "confirmation-hash".into(),
            prepare_idempotency_hash: idempotency.into(),
            available: 2,
            applicable: 1,
            observed_at: now,
        };
        let first = db.prepare_quota_reset(input("prepare-key")).await.unwrap();
        assert!(!first.replayed);
        let preflight_replay = db
            .quota_reset_prepare_replay(&account, "actor", "prepare-key")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(preflight_replay.id, first.operation.id);
        assert!(
            db.quota_reset_prepare_replay(&account, "other-actor", "prepare-key")
                .await
                .unwrap()
                .is_none(),
            "a caller cannot claim another actor's prepare replay"
        );
        let replay = db.prepare_quota_reset(input("prepare-key")).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.operation.id, first.operation.id);
        let operation = first.operation;

        let mut changed_account = account.clone();
        changed_account.credential_generation += 1;
        assert!(
            db.claim_quota_reset(
                &changed_account,
                &operation.id,
                "actor",
                "confirmation-hash",
                "confirm-key"
            )
            .await
            .is_err()
        );
        assert!(
            db.claim_quota_reset(
                &account,
                &operation.id,
                "other",
                "confirmation-hash",
                "confirm-key"
            )
            .await
            .is_err()
        );
        assert!(
            db.claim_quota_reset(&account, &operation.id, "actor", "bad", "confirm-key")
                .await
                .is_err()
        );
        assert!(
            db.reconcile_quota_reset(
                &tenant.to_string(),
                &account.id.to_string(),
                &operation.id,
                "actor",
                account.credential_generation,
                account.updated_at,
                99,
                99,
                now + 1
            )
            .await
            .is_err()
        );
        let claim = db
            .claim_quota_reset(
                &account,
                &operation.id,
                "actor",
                "confirmation-hash",
                "confirm-key",
            )
            .await
            .unwrap();
        assert!(matches!(claim, QuotaResetClaim::Claimed { .. }));
        let replay = db
            .claim_quota_reset(
                &account,
                &operation.id,
                "actor",
                "confirmation-hash",
                "confirm-key",
            )
            .await
            .unwrap();
        assert!(matches!(replay, QuotaResetClaim::Replayed(_)));
        assert!(
            db.claim_quota_reset(
                &account,
                &operation.id,
                "actor",
                "confirmation-hash",
                "different-confirm-key"
            )
            .await
            .is_err(),
            "a different confirmation key cannot replay or dispatch"
        );
        assert!(
            db.prepare_quota_reset(input("new-prepare-key"))
                .await
                .is_err(),
            "an unresolved operation blocks a differently keyed preparation"
        );
        assert!(
            !db.settle_accepted_quota_reset_from_observation(&account, 1, 0, now + 1)
                .await
                .unwrap(),
            "a submitted dispatch cannot be settled by a lower credit observation"
        );
        assert!(
            db.prepare_quota_reset(input("after-submitted-observation"))
                .await
                .is_err(),
            "submitted remains the unique active operation after observation"
        );

        db.finish_quota_reset(&operation.id, false, Some("reset_dispatch_unknown"))
            .await
            .unwrap();
        db.reconcile_quota_reset(
            &tenant.to_string(),
            &account.id.to_string(),
            &operation.id,
            "actor",
            account.credential_generation,
            account.updated_at,
            1,
            0,
            now + 2,
        )
        .await
        .unwrap();
        let unknown = db
            .quota_reset_operation(&tenant.to_string(), &account.id.to_string(), &operation.id)
            .await
            .unwrap();
        assert_eq!(unknown.state, "unknown");
        assert_eq!(unknown.settled_at, None);
        assert_eq!(unknown.available_credits, 2);
        assert_eq!(unknown.applicable_credits, 1);
        assert_eq!(unknown.reconciled_available_credits, Some(1));
        assert_eq!(unknown.reconciled_applicable_credits, Some(0));
        assert_eq!(
            unknown
                .audit
                .iter()
                .map(|event| event.event.as_str())
                .collect::<Vec<_>>(),
            [
                "prepared",
                "confirmation_claimed",
                "dispatch_unknown",
                "reconciled"
            ]
        );
        assert!(
            db.prepare_quota_reset(input("after-unknown"))
                .await
                .is_err()
        );
        assert!(
            db.claim_quota_reset(
                &account,
                &operation.id,
                "actor",
                "confirmation-hash",
                "different-confirm-key"
            )
            .await
            .is_err()
        );
        assert!(
            !db.settle_accepted_quota_reset_from_observation(&account, 0, 0, now + 3)
                .await
                .unwrap(),
            "fresh counts cannot release an unknown dispatch"
        );
        let mut changed_unknown_account = account.clone();
        changed_unknown_account.credential_generation += 1;
        changed_unknown_account.updated_at = now + 1;
        sqlx::query(
            "UPDATE upstream_accounts SET credential_generation = $1, updated_at = $2 WHERE id = $3 AND tenant_id = $4",
        )
        .bind(changed_unknown_account.credential_generation)
        .bind(changed_unknown_account.updated_at)
        .bind(account.id.to_string())
        .bind(tenant.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
        assert!(
            !db.settle_accepted_quota_reset_from_observation(
                &changed_unknown_account,
                0,
                0,
                now + 4
            )
            .await
            .unwrap(),
            "credential changes cannot settle an unknown dispatch"
        );

        assert!(unknown.manual_unlock_available);
        assert_eq!(unknown.dispatch_diagnostic.as_ref().unwrap().stage, "send");
        assert!(
            db.release_unknown_quota_reset(
                &changed_unknown_account,
                &operation.id,
                "operator",
                unknown.updated_at - 1,
                "operator_authorized_retry",
                1,
                1,
                unix_millis()
            )
            .await
            .is_err(),
            "stale review cannot release the lock"
        );
        assert!(
            db.release_unknown_quota_reset(
                &changed_unknown_account,
                &operation.id,
                "operator",
                unknown.updated_at,
                "arbitrary-secret-text",
                1,
                1,
                unix_millis()
            )
            .await
            .is_err(),
            "audit reasons are a closed vocabulary"
        );
        let mut foreign = changed_unknown_account.clone();
        foreign.tenant_id = Uuid::now_v7();
        assert!(
            db.release_unknown_quota_reset(
                &foreign,
                &operation.id,
                "operator",
                unknown.updated_at,
                "operator_authorized_retry",
                1,
                1,
                unix_millis()
            )
            .await
            .is_err(),
            "foreign tenant cannot unlock"
        );
        db.release_unknown_quota_reset(
            &changed_unknown_account,
            &operation.id,
            "operator",
            unknown.updated_at,
            "operator_authorized_retry",
            1,
            1,
            unix_millis(),
        )
        .await
        .unwrap();
        let released = db
            .quota_reset_operation(&tenant.to_string(), &account.id.to_string(), &operation.id)
            .await
            .unwrap();
        assert_eq!(released.state, "unknown");
        assert_eq!(released.error_code, unknown.error_code);
        assert_eq!(released.available_credits, unknown.available_credits);
        assert!(released.settled_at.is_some());
        assert!(!released.manual_unlock_available);
        let audit = released.audit.last().unwrap();
        assert_eq!(audit.event, "reconciled");
        assert_eq!(audit.actor_service_id.as_deref(), Some("operator"));
        assert_eq!(
            audit.error_code.as_deref(),
            Some("manual_lock_released_operator_authorized_retry")
        );
        assert!(
            db.release_unknown_quota_reset(
                &changed_unknown_account,
                &operation.id,
                "operator",
                released.updated_at,
                "operator_authorized_retry",
                1,
                1,
                unix_millis()
            )
            .await
            .is_err(),
            "unlock cannot be repeated"
        );
        assert!(
            matches!(
                db.claim_quota_reset(
                    &changed_unknown_account,
                    &operation.id,
                    "actor",
                    "confirmation-hash",
                    "confirm-key"
                )
                .await
                .unwrap(),
                QuotaResetClaim::Replayed(_)
            ),
            "old confirmation never dispatches again"
        );
        let independent = db
            .prepare_quota_reset(PrepareQuotaReset {
                account: changed_unknown_account.clone(),
                ..input("manual-recovery-new-key")
            })
            .await
            .unwrap();
        assert_ne!(independent.operation.id, released.id);
        assert_eq!(independent.operation.state, "prepared");
        assert!(
            db.release_unknown_quota_reset(
                &changed_unknown_account,
                &independent.operation.id,
                "operator",
                independent.operation.updated_at,
                "operator_authorized_retry",
                1,
                1,
                unix_millis()
            )
            .await
            .is_err(),
            "recovery cannot unlock a live prepared dispatch"
        );

        let mut accepted_account = account.clone();
        accepted_account.id = Uuid::now_v7();
        accepted_account.name = "accepted quota fixture".into();
        sqlx::query("INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, created_at, updated_at) VALUES ($1, $2, 'accepted quota fixture', 'openai-codex', 'oauth', '{}', 'active', 1, $3, $3)")
            .bind(accepted_account.id.to_string()).bind(tenant.to_string()).bind(now).execute(&db.pool).await.unwrap();
        let accepted_input = |idempotency: &str| PrepareQuotaReset {
            id: Uuid::now_v7(),
            account: accepted_account.clone(),
            actor: "actor".into(),
            confirmation_hash: "accepted-confirmation-hash".into(),
            prepare_idempotency_hash: idempotency.into(),
            available: 2,
            applicable: 2,
            observed_at: now,
        };
        let accepted = db
            .prepare_quota_reset(accepted_input("accepted-prepare"))
            .await
            .unwrap()
            .operation;
        assert!(matches!(
            db.claim_quota_reset(
                &accepted_account,
                &accepted.id,
                "actor",
                "accepted-confirmation-hash",
                "accepted-confirm"
            )
            .await
            .unwrap(),
            QuotaResetClaim::Claimed { .. }
        ));
        db.finish_quota_reset(&accepted.id, true, None)
            .await
            .unwrap();
        db.record_quota_reset_readback(
            &accepted_account,
            &accepted.id,
            "actor",
            Some("quota_timeout"),
        )
        .await
        .unwrap();
        let pending = db
            .quota_reset_operation(
                &tenant.to_string(),
                &accepted_account.id.to_string(),
                &accepted.id,
            )
            .await
            .unwrap();
        assert_eq!(pending.state, "accepted");
        assert_eq!(pending.quota_refresh_status, "pending");
        assert!(!pending.manual_unlock_available);
        db.record_quota_reset_readback(&accepted_account, &accepted.id, "actor", None)
            .await
            .unwrap();
        let read = db
            .quota_reset_operation(
                &tenant.to_string(),
                &accepted_account.id.to_string(),
                &accepted.id,
            )
            .await
            .unwrap();
        assert_eq!(read.state, "accepted");
        assert_eq!(read.quota_refresh_status, "succeeded");
        assert_eq!(read.available_credits, 2);

        // Ordinary reconciliation can evict every readback from the display window.
        for _ in 0..32 {
            db.reconcile_quota_reset(
                &tenant.to_string(),
                &accepted_account.id.to_string(),
                &accepted.id,
                "actor",
                accepted_account.credential_generation,
                accepted_account.updated_at,
                2,
                2,
                unix_millis(),
            )
            .await
            .unwrap();
        }
        let outside_window = db
            .quota_reset_operation(
                &tenant.to_string(),
                &accepted_account.id.to_string(),
                &accepted.id,
            )
            .await
            .unwrap();
        assert_eq!(outside_window.audit.len(), 32);
        assert!(
            outside_window
                .audit
                .iter()
                .all(|event| { event.event == "reconciled" && event.error_code.is_none() })
        );
        assert_eq!(outside_window.quota_refresh_status, "succeeded");

        // The latest readback wins in both directions, independent of pagination.
        for (error, expected) in [(Some("quota_timeout"), "pending"), (None, "succeeded")] {
            db.record_quota_reset_readback(&accepted_account, &accepted.id, "actor", error)
                .await
                .unwrap();
            let latest = db
                .quota_reset_operation(
                    &tenant.to_string(),
                    &accepted_account.id.to_string(),
                    &accepted.id,
                )
                .await
                .unwrap();
            assert_eq!(latest.quota_refresh_status, expected);
            assert_eq!(latest.state, "accepted");
            assert_eq!(latest.available_credits, 2);
        }

        assert!(
            db.current_quota_reset_operation_for_tenant(
                accepted_account.tenant_external_id.as_ref().unwrap(),
                &accepted_account.id.to_string()
            )
            .await
            .unwrap()
            .is_some()
        );
        assert!(
            db.reconcile_quota_reset(
                &tenant.to_string(),
                &accepted_account.id.to_string(),
                &accepted.id,
                "actor",
                accepted_account.credential_generation + 1,
                accepted_account.updated_at,
                1,
                1,
                now + 3,
            )
            .await
            .is_err(),
            "manual reconciliation cannot cross the credential fence"
        );
        assert!(
            !db.settle_accepted_quota_reset_from_observation(&accepted_account, 2, 1, now + 3)
                .await
                .unwrap(),
            "an unchanged available-credit count cannot release the accepted lock"
        );
        assert!(
            !db.settle_accepted_quota_reset_from_observation(&accepted_account, 1, 1, now)
                .await
                .unwrap(),
            "evidence that is not newer than preparation cannot settle the accepted operation"
        );
        let mut changed_generation = accepted_account.clone();
        changed_generation.credential_generation += 1;
        changed_generation.updated_at = now + 1;
        sqlx::query(
            "UPDATE upstream_accounts SET credential_generation = $1, updated_at = $2 WHERE id = $3 AND tenant_id = $4",
        )
        .bind(changed_generation.credential_generation)
        .bind(changed_generation.updated_at)
        .bind(accepted_account.id.to_string())
        .bind(tenant.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
        assert!(
            db.reconcile_quota_reset(
                &tenant.to_string(),
                &accepted_account.id.to_string(),
                &accepted.id,
                "actor",
                changed_generation.credential_generation,
                changed_generation.updated_at,
                2,
                1,
                now + 4,
            )
            .await
            .is_err(),
            "a generation change alone cannot prove that an accepted credit was consumed"
        );
        db.reconcile_quota_reset(
            &tenant.to_string(),
            &accepted_account.id.to_string(),
            &accepted.id,
            "actor",
            changed_generation.credential_generation,
            changed_generation.updated_at,
            1,
            1,
            now + 5,
        )
        .await
        .unwrap();
        let settled = db
            .quota_reset_operation(
                &tenant.to_string(),
                &accepted_account.id.to_string(),
                &accepted.id,
            )
            .await
            .unwrap();
        assert_eq!(settled.state, "accepted");
        assert_eq!(settled.settled_at, Some(now + 5));
        assert_eq!(settled.reconciled_available_credits, Some(1));
        assert_eq!(
            settled
                .audit
                .last()
                .and_then(|event| event.error_code.as_deref()),
            Some("accepted_credit_decrease_observed")
        );
        assert!(
            db.current_quota_reset_operation_for_tenant(
                accepted_account.tenant_external_id.as_ref().unwrap(),
                &accepted_account.id.to_string()
            )
            .await
            .unwrap()
            .is_none(),
            "settlement hides the old accepted operation from current recovery"
        );
        assert!(
            !db.prepare_quota_reset(PrepareQuotaReset {
                id: Uuid::now_v7(),
                account: changed_generation.clone(),
                actor: "actor".into(),
                confirmation_hash: "accepted-confirmation-hash-2".into(),
                prepare_idempotency_hash: "after-accepted-settlement".into(),
                available: 1,
                applicable: 1,
                observed_at: now + 5,
            })
            .await
            .unwrap()
            .replayed,
            "settlement releases the unique slot for a fresh preparation"
        );

        let mut observed_account = accepted_account.clone();
        observed_account.id = Uuid::now_v7();
        observed_account.name = "observed quota fixture".into();
        observed_account.credential_generation = 1;
        observed_account.updated_at = now;
        sqlx::query("INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, created_at, updated_at) VALUES ($1, $2, 'observed quota fixture', 'openai-codex', 'oauth', '{}', 'active', 1, $3, $3)")
            .bind(observed_account.id.to_string()).bind(tenant.to_string()).bind(now).execute(&db.pool).await.unwrap();
        let observed = db
            .prepare_quota_reset(PrepareQuotaReset {
                id: Uuid::now_v7(),
                account: observed_account.clone(),
                actor: "actor".into(),
                confirmation_hash: "observed-confirmation-hash".into(),
                prepare_idempotency_hash: "observed-prepare".into(),
                available: 2,
                applicable: 2,
                observed_at: now,
            })
            .await
            .unwrap()
            .operation;
        assert!(matches!(
            db.claim_quota_reset(
                &observed_account,
                &observed.id,
                "actor",
                "observed-confirmation-hash",
                "observed-confirm"
            )
            .await
            .unwrap(),
            QuotaResetClaim::Claimed { .. }
        ));
        db.finish_quota_reset(&observed.id, true, None)
            .await
            .unwrap();
        observed_account.credential_generation += 1;
        observed_account.updated_at = now + 1;
        sqlx::query(
            "UPDATE upstream_accounts SET credential_generation = $1, updated_at = $2 WHERE id = $3 AND tenant_id = $4",
        )
        .bind(observed_account.credential_generation)
        .bind(observed_account.updated_at)
        .bind(observed_account.id.to_string())
        .bind(tenant.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
        assert!(
            db.settle_accepted_quota_reset_from_observation(&observed_account, 1, 1, now + 6)
                .await
                .unwrap(),
            "background evidence also settles across OAuth and transport changes"
        );
    }
}
