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
               reset_at = CASE WHEN upstream_supplier_availability.failure_kind = 'quota_exhausted'
                    AND excluded.failure_kind = 'rate_limited'
                    AND (upstream_supplier_availability.reset_at IS NULL OR upstream_supplier_availability.reset_at > excluded.observed_at)
                 THEN upstream_supplier_availability.reset_at ELSE excluded.reset_at END,
               reset_source = CASE WHEN upstream_supplier_availability.failure_kind = 'quota_exhausted'
                    AND excluded.failure_kind = 'rate_limited'
                    AND (upstream_supplier_availability.reset_at IS NULL OR upstream_supplier_availability.reset_at > excluded.observed_at)
                 THEN upstream_supplier_availability.reset_source ELSE excluded.reset_source END,
               observed_at = excluded.observed_at,
               request_id = excluded.request_id
             WHERE upstream_supplier_availability.observed_at < excluded.observed_at
                OR (upstream_supplier_availability.observed_at = excluded.observed_at
                    AND upstream_supplier_availability.request_id = excluded.request_id
                    AND (excluded.failure_kind = 'quota_exhausted'
                         OR upstream_supplier_availability.failure_kind = 'rate_limited'))",
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
               AND (q.reset_at IS NULL OR q.reset_at <= $2)
             ORDER BY q.observed_at, q.upstream_model",
        )
        .bind(account_id.to_string())
        .bind(super::super::unix_millis())
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    pub(crate) async fn recover_supplier_quota_from_probe(
        &self,
        account_id: Uuid,
        body: &Value,
    ) -> Result<(), AppError> {
        if body["status"] != "healthy" || body["upstream_status"] != 200 {
            return Ok(());
        }
        let Some(model) = body["actual_model"].as_str() else {
            return Ok(());
        };
        // Retain a tombstone so delayed failure persistence predating this
        // probe cannot re-open it. A later/concurrent 429 still wins.
        sqlx::query(
            "UPDATE upstream_supplier_availability
             SET failure_kind = 'available', reset_at = NULL, reset_source = 'probe_success',
                 observed_at = $5, request_id = $6
             WHERE upstream_account_id = $1 AND upstream_model = $2
               AND credential_generation = $3 AND transport_revision = $4
               AND observed_at < $5
               AND EXISTS (SELECT 1 FROM upstream_accounts a WHERE a.id = $1
                 AND a.status = 'active' AND a.credential_generation = $3 AND a.updated_at = $4)",
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
