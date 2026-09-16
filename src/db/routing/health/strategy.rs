use super::*;

/// A tenant-scoped, credential-free snapshot; no row means the candidate is
/// inactive, absent, or stale, not that it is healthy.
#[derive(Clone, Debug)]
pub(crate) struct GroupRoutingHealth {
    pub(crate) consecutive_failures: i64,
    pub(crate) last_failure_kind: String,
    pub(crate) cooldown_until: i64,
    pub(crate) probe_lease_until: i64,
    pub(crate) updated_at: i64,
}

impl GroupRoutingHealth {
    pub(crate) fn is_transient(&self) -> bool {
        matches!(
            self.last_failure_kind.as_str(),
            "connection" | "unavailable" | "invalid_response"
        )
    }

    pub(crate) fn effective_cooldown_until(&self, override_ms: Option<u64>) -> i64 {
        if self.is_transient()
            && let Some(duration) = override_ms
        {
            return self.updated_at.saturating_add(duration.min(60_000) as i64);
        }
        self.cooldown_until
    }
}

impl Database {
    pub(crate) async fn group_routing_health(
        &self,
        tenant_id: Uuid,
        upstream_account_id: Uuid,
        credential_generation: i64,
    ) -> Result<Option<GroupRoutingHealth>, AppError> {
        let row = sqlx::query(
            "SELECT health.consecutive_failures, health.last_failure_kind,
                    health.cooldown_until, health.probe_lease_until, health.updated_at
             FROM upstream_accounts account
             LEFT JOIN upstream_account_health health
               ON health.upstream_account_id = account.id
              AND health.credential_generation = $3
             WHERE account.id = $1 AND account.tenant_id = $2
               AND account.status = 'active' AND account.credential_generation = $3",
        )
        .bind(upstream_account_id.to_string())
        .bind(tenant_id.to_string())
        .bind(credential_generation)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(GroupRoutingHealth {
                consecutive_failures: row
                    .try_get::<Option<i64>, _>("consecutive_failures")?
                    .unwrap_or(0),
                last_failure_kind: row
                    .try_get::<Option<String>, _>("last_failure_kind")?
                    .unwrap_or_default(),
                cooldown_until: row
                    .try_get::<Option<i64>, _>("cooldown_until")?
                    .unwrap_or(0),
                probe_lease_until: row
                    .try_get::<Option<i64>, _>("probe_lease_until")?
                    .unwrap_or(0),
                updated_at: row.try_get::<Option<i64>, _>("updated_at")?.unwrap_or(0),
            })
        })
        .transpose()
    }

    /// Policy affects only this admission, never stored cooldowns. Even a zero
    /// override must acquire the existing cross-process exclusive probe lease.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn claim_upstream_account_attempt_with_strategy(
        &self,
        tenant_id: Uuid,
        upstream_account_id: Uuid,
        credential_generation: i64,
        health: UpstreamHealthConfig,
        allow_transient_probe: bool,
        cooldown_override_ms: Option<u64>,
        transient_only: bool,
    ) -> Result<UpstreamAttemptAdmission, AppError> {
        let now = unix_millis();
        let unavailable = |cooldown_until, probe_lease_until, transient_wait_eligible| {
            UpstreamAttemptAdmission::Unavailable {
                cooldown_until,
                probe_lease_until,
                shared_probe_eligible: false,
                transient_wait_eligible,
            }
        };
        let Some(snapshot) = self
            .group_routing_health(tenant_id, upstream_account_id, credential_generation)
            .await?
        else {
            return Ok(unavailable(0, 0, false));
        };
        if snapshot.consecutive_failures == 0 {
            return Ok(
                match self
                    .ensure_healthy_admission_epoch(upstream_account_id, credential_generation, now)
                    .await?
                {
                    Some(failure_epoch) => UpstreamAttemptAdmission::Healthy { failure_epoch },
                    None => unavailable(0, 0, true),
                },
            );
        }
        let transient = snapshot.is_transient();
        let cooldown = snapshot.effective_cooldown_until(cooldown_override_ms);
        let wait_eligible = transient && allow_transient_probe;
        if cooldown > now
            || snapshot.probe_lease_until > now
            || (transient && !allow_transient_probe)
            || (transient_only && !transient)
        {
            return Ok(unavailable(
                cooldown,
                snapshot.probe_lease_until,
                wait_eligible,
            ));
        }
        let lease_token = Uuid::now_v7();
        let result = sqlx::query(
            "UPDATE upstream_account_health
             SET probe_lease_until = $1, probe_lease_token = $2, updated_at = $3
             WHERE upstream_account_id = $4 AND credential_generation = $5
               AND consecutive_failures = $6 AND consecutive_failures > 0
               AND last_failure_kind = $7 AND updated_at = $8
               AND cooldown_until = $9 AND probe_lease_until <= $3
               AND (cooldown_until <= $3 OR
                    (last_failure_kind IN ('connection', 'unavailable', 'invalid_response')
                     AND $10 = 1 AND $11 <= $3))
               AND EXISTS (SELECT 1 FROM upstream_accounts account
                   WHERE account.id = upstream_account_health.upstream_account_id
                     AND account.tenant_id = $12 AND account.status = 'active'
                     AND account.credential_generation = $5)",
        )
        .bind(now.saturating_add(health.probe_lease_millis))
        .bind(lease_token.to_string())
        .bind(now)
        .bind(upstream_account_id.to_string())
        .bind(credential_generation)
        .bind(snapshot.consecutive_failures)
        .bind(&snapshot.last_failure_kind)
        .bind(snapshot.updated_at)
        .bind(snapshot.cooldown_until)
        .bind(i64::from(
            cooldown_override_ms.is_some() && allow_transient_probe,
        ))
        .bind(cooldown)
        .bind(tenant_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(if result.rows_affected() == 1 {
            UpstreamAttemptAdmission::Probe { lease_token }
        } else {
            unavailable(cooldown, snapshot.probe_lease_until, wait_eligible)
        })
    }
}

#[cfg(test)]
mod tests;
