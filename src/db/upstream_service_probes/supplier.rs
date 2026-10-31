use super::*;

/// Provider response evidence only; local admission 429s never enter here.
/// An unknown reset is NULL, not a fabricated deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SupplierQuotaEvidence {
    pub(crate) exhausted: bool,
    pub(crate) reset_at: Option<i64>,
    pub(crate) reset_source: &'static str,
}

impl Database {
    pub(crate) async fn supplier_quota_probe_deadline(
        &self,
        account_id: Uuid,
        model: &str,
    ) -> Result<Option<i64>, AppError> {
        sqlx::query_scalar(
            "SELECT MAX(deadline) FROM (
             SELECT q.reset_at AS deadline FROM upstream_supplier_availability q
             JOIN upstream_accounts a ON a.id = q.upstream_account_id
               AND a.credential_generation = q.credential_generation AND a.updated_at = q.transport_revision
             WHERE a.id = $1 AND q.upstream_model = $2 AND q.reset_at > $3
               AND q.verification_after IS NULL
             UNION ALL
             SELECT h.cooldown_until AS deadline FROM upstream_account_health h
             JOIN upstream_accounts a ON a.id = h.upstream_account_id
               AND a.credential_generation = h.credential_generation AND a.updated_at = h.transport_revision
             WHERE a.id = $1 AND h.last_failure_kind IN ('quota_exhausted', 'rate_limited')
               AND h.cooldown_until > $3
             ) deadlines",
        ).bind(account_id.to_string()).bind(model).bind(super::super::unix_millis())
            .fetch_one(&self.pool).await.map_err(Into::into)
    }

    pub(crate) async fn store_supplier_quota(
        &self,
        route: &crate::provider::ResolvedUpstream,
        request_id: Uuid,
        observed_at: i64,
        evidence: SupplierQuotaEvidence,
    ) -> Result<(), AppError> {
        // This endpoint supplies model evidence only, never account scope.
        sqlx::query(
            "INSERT INTO upstream_supplier_availability
             (upstream_account_id, upstream_model, credential_generation, transport_revision,
              failure_kind, reset_at, reset_source, observed_at, request_id)
             SELECT id, $2, $3, $4, $5, $6, $7, $8, $9 FROM upstream_accounts
             WHERE id = $1 AND status = 'active'
               AND credential_generation = $3 AND updated_at = $4
             ON CONFLICT (upstream_account_id, upstream_model, credential_generation, transport_revision)
             DO UPDATE SET
               failure_kind = CASE WHEN upstream_supplier_availability.failure_kind = 'quota_exhausted'
                    AND excluded.failure_kind = 'rate_limited'
                    AND (upstream_supplier_availability.reset_at IS NULL OR upstream_supplier_availability.reset_at > excluded.observed_at)
                 THEN upstream_supplier_availability.failure_kind ELSE excluded.failure_kind END,
               reset_at = CASE WHEN upstream_supplier_availability.reset_at > excluded.observed_at
                    AND (excluded.reset_at IS NULL OR upstream_supplier_availability.reset_at > excluded.reset_at)
                 THEN upstream_supplier_availability.reset_at ELSE excluded.reset_at END,
               reset_source = CASE WHEN upstream_supplier_availability.reset_at > excluded.observed_at
                    AND (excluded.reset_at IS NULL OR upstream_supplier_availability.reset_at > excluded.reset_at)
                 THEN upstream_supplier_availability.reset_source ELSE excluded.reset_source END,
               observed_at = CASE WHEN excluded.observed_at > upstream_supplier_availability.observed_at
                 THEN excluded.observed_at ELSE upstream_supplier_availability.observed_at END,
               request_id = CASE WHEN excluded.observed_at >= upstream_supplier_availability.observed_at
                 THEN excluded.request_id ELSE upstream_supplier_availability.request_id END,
               verification_after = CASE WHEN excluded.observed_at >= upstream_supplier_availability.verification_after
                 THEN NULL ELSE upstream_supplier_availability.verification_after END
             WHERE upstream_supplier_availability.success_at IS NULL
                OR excluded.observed_at >= upstream_supplier_availability.success_at",
        )
        .bind(route.account_id.to_string())
        .bind(&route.upstream_model)
        .bind(route.credential_generation)
        .bind(route.transport_revision)
        .bind(if evidence.exhausted { "quota_exhausted" } else { "rate_limited" })
        .bind(evidence.reset_at)
        .bind(evidence.reset_source)
        .bind(observed_at)
        .bind(request_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn supplier_quota_recovery_models(
        &self,
        account_id: Uuid,
    ) -> Result<Vec<String>, AppError> {
        sqlx::query_scalar(
            "SELECT q.upstream_model FROM upstream_supplier_availability q
             JOIN upstream_accounts a ON a.id = q.upstream_account_id
               AND a.credential_generation = q.credential_generation
               AND a.updated_at = q.transport_revision
             WHERE a.id = $1 AND a.status = 'active' AND q.upstream_model <> ''
               AND q.failure_kind <> 'available'
               AND (q.failure_kind = 'quota_exhausted' OR q.reset_at IS NOT NULL)
               AND (q.reset_at IS NULL OR q.reset_at <= $2 OR q.verification_after IS NOT NULL)
             ORDER BY q.observed_at, q.upstream_model",
        )
        .bind(account_id.to_string())
        .bind(super::super::unix_millis())
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Fresh accepted-reset readback is not exact-model success. It permits
    /// independent verification without discarding either exclusion or timing.
    /// Newer/concurrent failures retain their deadline; no reset POST occurs here.
    pub(crate) async fn schedule_supplier_quota_verification(
        &self,
        account_id: Uuid,
        generation: i64,
        revision: i64,
        observation_started_at: i64,
    ) -> Result<(), AppError> {
        sqlx::query(
            "UPDATE upstream_supplier_availability SET verification_after = $4
             WHERE upstream_account_id = $1 AND credential_generation = $2
               AND transport_revision = $3 AND observed_at < $4
               AND failure_kind <> 'available'
               AND EXISTS (SELECT 1 FROM upstream_accounts a WHERE a.id = $1
                 AND a.status = 'active' AND a.driver = 'openai-codex'
                 AND a.credential_generation = $2 AND a.updated_at = $3)",
        )
        .bind(account_id.to_string())
        .bind(generation)
        .bind(revision)
        .bind(observation_started_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn recover_supplier_quota_from_probe(
        &self,
        account_id: Uuid,
        body: &Value,
    ) -> Result<(), AppError> {
        if body["status"] != "healthy"
            || !body["upstream_status"]
                .as_u64()
                .is_some_and(|status| (200..300).contains(&status))
        {
            return Ok(());
        }
        let Some(model) = body["actual_model"].as_str() else {
            return Ok(());
        };
        // Retain a tombstone so delayed failure persistence predating this
        // probe cannot re-open it. A later/concurrent 429 still wins.
        sqlx::query(
            "INSERT INTO upstream_supplier_availability
             (upstream_account_id, upstream_model, credential_generation, transport_revision,
              failure_kind, reset_at, reset_source, observed_at, request_id, success_at)
             SELECT id, $2, $3, $4, 'available', NULL, 'probe_success', $5, $6, $5
             FROM upstream_accounts WHERE id = $1 AND status = 'active'
               AND credential_generation = $3 AND updated_at = $4
             ON CONFLICT (upstream_account_id, upstream_model, credential_generation, transport_revision)
             DO UPDATE SET
               failure_kind = CASE WHEN upstream_supplier_availability.observed_at < excluded.observed_at
                 THEN excluded.failure_kind ELSE upstream_supplier_availability.failure_kind END,
               reset_at = CASE WHEN upstream_supplier_availability.observed_at < excluded.observed_at
                 THEN NULL ELSE upstream_supplier_availability.reset_at END,
               reset_source = CASE WHEN upstream_supplier_availability.observed_at < excluded.observed_at
                 THEN excluded.reset_source ELSE upstream_supplier_availability.reset_source END,
               observed_at = CASE WHEN upstream_supplier_availability.observed_at < excluded.observed_at
                 THEN excluded.observed_at ELSE upstream_supplier_availability.observed_at END,
               request_id = CASE WHEN upstream_supplier_availability.observed_at < excluded.observed_at
                 THEN excluded.request_id ELSE upstream_supplier_availability.request_id END,
               success_at = excluded.success_at,
               verification_after = CASE WHEN upstream_supplier_availability.observed_at < excluded.observed_at
                 THEN NULL ELSE upstream_supplier_availability.verification_after END
             WHERE upstream_supplier_availability.success_at IS NULL
                OR upstream_supplier_availability.success_at < excluded.success_at",
        )
        .bind(account_id.to_string())
        .bind(model)
        .bind(body["credential_generation"].as_i64().unwrap_or(-1))
        .bind(body["transport_revision"].as_i64().unwrap_or(-1))
        .bind(body["started_at"].as_i64().unwrap_or(0))
        .bind(body["request_id"].as_str().unwrap_or(""))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
