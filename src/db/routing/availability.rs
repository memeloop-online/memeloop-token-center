//! Scheduling projection of supplier evidence, shared by every candidate query.
//!
//! The joined `quota`, `legacy` and `service_probe` rows must already match the
//! account's current credential generation and transport revision. A rejection
//! without a provider reset is evidence, but not an infinite exclusion lease.
pub(super) const CANDIDATE_AVAILABILITY_RANK: &str = "CAST(CASE
    WHEN quota.reset_at > $6
      OR (legacy.cooldown_until > $6 AND legacy.last_failure_kind IN ('quota_exhausted', 'rate_limited'))
      THEN 2
    WHEN (quota.reset_at IS NULL AND quota.failure_kind = 'quota_exhausted')
      OR service_probe.status = 'unhealthy' THEN 1
    ELSE 0
END AS BIGINT)";

use serde::Serialize;
use sqlx::Row;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SupplierAvailabilityState {
    Eligible,
    Recoverable,
    ExhaustedUntil,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SupplierAvailabilityDecision {
    pub upstream_model: String,
    pub credential_generation: i64,
    pub transport_revision: i64,
    pub capacity_scope: String,
    pub state: SupplierAvailabilityState,
    pub dispatch_eligible: bool,
    pub reason: &'static str,
    pub evidence_at: i64,
    pub next_check_hint: Option<i64>,
}

impl super::super::Database {
    /// Presentation consumes the scheduler's exact scoped policy, rather than
    /// deriving availability from a displayed percentage or an old generation.
    pub(crate) async fn supplier_availability_decisions(
        &self,
        account_id: Uuid,
    ) -> Result<Vec<SupplierAvailabilityDecision>, super::super::AppError> {
        let now = super::super::unix_millis();
        let rank = CANDIDATE_AVAILABILITY_RANK.replace("$6", "$2");
        let statement = format!("WITH models AS (
                SELECT e.upstream_model FROM model_route_eligible_upstream_accounts e
                JOIN model_routes r ON r.id=e.model_route_id AND r.tenant_id=e.tenant_id
                WHERE e.upstream_account_id=$1 AND r.enabled=1 AND r.archived_at IS NULL
                UNION SELECT q.upstream_model FROM upstream_supplier_availability q
                JOIN upstream_accounts current_account ON current_account.id=q.upstream_account_id
                    AND current_account.credential_generation=q.credential_generation
                    AND current_account.updated_at=q.transport_revision
                WHERE q.upstream_account_id=$1
            ) SELECT models.upstream_model,a.credential_generation,
            a.updated_at AS transport_revision,COALESCE(quota.capacity_scope,'unknown') AS capacity_scope,
            COALESCE(quota.failure_kind,'unknown') AS failure_kind,quota.reset_at,
            COALESCE(quota.observed_at,0) AS observed_at,legacy.cooldown_until,legacy.last_failure_kind,
            {rank} AS probe_rank
            FROM models JOIN upstream_accounts a ON a.id=$1
            LEFT JOIN upstream_supplier_availability quota ON quota.upstream_account_id=a.id
                AND quota.upstream_model=models.upstream_model
                AND a.credential_generation=quota.credential_generation
                AND a.updated_at=quota.transport_revision
            LEFT JOIN upstream_account_health legacy ON legacy.upstream_account_id=a.id
                AND legacy.credential_generation=a.credential_generation AND legacy.transport_revision=a.updated_at
            LEFT JOIN upstream_service_probes service_probe ON service_probe.upstream_account_id=a.id
                AND service_probe.credential_generation=a.credential_generation AND service_probe.transport_revision=a.updated_at
                AND service_probe.actual_model=models.upstream_model
            WHERE a.status='active' ORDER BY models.upstream_model");
        let rows = sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(account_id.to_string())
            .bind(now)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let rank: i64 = row.try_get("probe_rank")?;
                let failure: String = row.try_get("failure_kind")?;
                let legacy_kind: Option<String> = row.try_get("last_failure_kind")?;
                let reset: Option<i64> = row.try_get("reset_at")?;
                let cooldown: Option<i64> = row.try_get("cooldown_until")?;
                let (state, reason) = match rank {
                    2 => (
                        SupplierAvailabilityState::ExhaustedUntil,
                        "supplier_timed_restriction",
                    ),
                    1 if failure == "quota_exhausted" && reset.is_none() => (
                        SupplierAvailabilityState::Recoverable,
                        "supplier_reset_unknown",
                    ),
                    1 => (
                        SupplierAvailabilityState::Recoverable,
                        "service_verification_needed",
                    ),
                    _ if failure == "available" => (
                        SupplierAvailabilityState::Eligible,
                        "newer_scoped_allowance",
                    ),
                    _ => (SupplierAvailabilityState::Unknown, "no_current_exclusion"),
                };
                let next_check_hint = if rank == 2 {
                    reset
                        .filter(|deadline| *deadline > now)
                        .into_iter()
                        .chain(cooldown.filter(|deadline| {
                            *deadline > now
                                && matches!(
                                    legacy_kind.as_deref(),
                                    Some("quota_exhausted" | "rate_limited")
                                )
                        }))
                        .max()
                } else {
                    None
                };
                Ok(SupplierAvailabilityDecision {
                    upstream_model: row.try_get("upstream_model")?,
                    credential_generation: row.try_get("credential_generation")?,
                    transport_revision: row.try_get("transport_revision")?,
                    capacity_scope: row.try_get("capacity_scope")?,
                    state,
                    dispatch_eligible: rank != 2,
                    reason,
                    evidence_at: row.try_get("observed_at")?,
                    next_check_hint,
                })
            })
            .collect()
    }
}
