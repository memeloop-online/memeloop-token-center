use super::*;

pub(crate) const TRANSIENT_EWMA_SCALE: i64 = 1_000_000;

const RECORD_TRANSIENT_HEALTH_SAMPLE_SQL: &str =
    "INSERT INTO upstream_account_transient_health_signals (
         upstream_account_id, credential_generation, sample_count,
         ewma_micros, last_observed_at, recovery_successes, revision
     ) SELECT $1, $2, 1, $3, $4, $5, 1
       FROM upstream_accounts account
      WHERE account.id = $1 AND account.status = 'active'
        AND account.credential_generation = $2
     ON CONFLICT (upstream_account_id) DO UPDATE SET
         credential_generation = excluded.credential_generation,
         sample_count = CASE
             WHEN upstream_account_transient_health_signals.credential_generation <> excluded.credential_generation
                 THEN 1
             WHEN upstream_account_transient_health_signals.sample_count < 9223372036854775807
                 THEN upstream_account_transient_health_signals.sample_count + 1
             ELSE upstream_account_transient_health_signals.sample_count
         END,
         ewma_micros = CASE
             WHEN upstream_account_transient_health_signals.credential_generation <> excluded.credential_generation
                 THEN excluded.ewma_micros
             ELSE (
                 upstream_account_transient_health_signals.ewma_micros * 3 + excluded.ewma_micros + 2
             ) / 4
         END,
         last_observed_at = CASE
             WHEN upstream_account_transient_health_signals.credential_generation < excluded.credential_generation
                  OR upstream_account_transient_health_signals.last_observed_at < excluded.last_observed_at
                 THEN excluded.last_observed_at
             ELSE upstream_account_transient_health_signals.last_observed_at
         END,
         recovery_successes = CASE
             WHEN upstream_account_transient_health_signals.credential_generation <> excluded.credential_generation
                 THEN excluded.recovery_successes
             WHEN excluded.ewma_micros > 0 THEN 0
             WHEN upstream_account_transient_health_signals.recovery_successes < 9223372036854775807
                 THEN upstream_account_transient_health_signals.recovery_successes + 1
             ELSE upstream_account_transient_health_signals.recovery_successes
         END,
         revision = CASE
             WHEN upstream_account_transient_health_signals.credential_generation <> excluded.credential_generation
                 THEN 1
             WHEN upstream_account_transient_health_signals.revision < 9223372036854775807
                 THEN upstream_account_transient_health_signals.revision + 1
             ELSE upstream_account_transient_health_signals.revision
         END
     WHERE upstream_account_transient_health_signals.credential_generation <= excluded.credential_generation
       AND EXISTS (
         SELECT 1 FROM upstream_accounts account
          WHERE account.id = upstream_account_transient_health_signals.upstream_account_id
            AND account.status = 'active'
            AND account.credential_generation = excluded.credential_generation
     )
     RETURNING sample_count, ewma_micros, last_observed_at,
               recovery_successes, revision";

/// Credential-free, generation-fenced transient observations. The integer
/// EWMA uses a fixed alpha of 1/4 so both database backends produce identical
/// values without floating-point drift.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TransientHealthSignal {
    pub(crate) sample_count: i64,
    pub(crate) ewma_micros: i64,
    pub(crate) last_observed_at: i64,
    pub(crate) recovery_successes: i64,
    pub(crate) revision: i64,
}

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
    /// Persist one conclusive success/failure sample independently of circuit
    /// state. Cancelled and otherwise inconclusive attempts never call this
    /// method. The account generation and active status fence stale requests.
    pub(crate) async fn record_transient_health_sample(
        &self,
        upstream_account_id: Uuid,
        credential_generation: i64,
        transient_failure: bool,
    ) -> Result<Option<TransientHealthSignal>, AppError> {
        let now = unix_millis();
        let sample = if transient_failure {
            TRANSIENT_EWMA_SCALE
        } else {
            0
        };
        let row = sqlx::query(RECORD_TRANSIENT_HEALTH_SAMPLE_SQL)
            .bind(upstream_account_id.to_string())
            .bind(credential_generation)
            .bind(sample)
            .bind(now)
            .bind(i64::from(!transient_failure))
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            Ok(TransientHealthSignal {
                sample_count: row.try_get("sample_count")?,
                ewma_micros: row.try_get("ewma_micros")?,
                last_observed_at: row.try_get("last_observed_at")?,
                recovery_successes: row.try_get("recovery_successes")?,
                revision: row.try_get("revision")?,
            })
        })
        .transpose()
    }

    /// A valid probe that has not yet met an explicitly active v2 recovery
    /// threshold releases only its exact lease. It leaves hard state intact
    /// and schedules the next bounded probe without replaying this request.
    pub(crate) async fn defer_upstream_account_probe_recovery(
        &self,
        upstream_account_id: Uuid,
        credential_generation: i64,
        lease_token: Uuid,
        cooldown_millis: u64,
    ) -> Result<bool, AppError> {
        let now = unix_millis();
        let result = sqlx::query(
            "UPDATE upstream_account_health
                SET cooldown_until = $1, probe_lease_until = 0,
                    probe_lease_token = '', updated_at = $2
              WHERE upstream_account_id = $3 AND credential_generation = $4
                AND consecutive_failures > 0 AND probe_lease_token = $5
                AND EXISTS (
                    SELECT 1 FROM upstream_accounts account
                     WHERE account.id = upstream_account_health.upstream_account_id
                       AND account.status = 'active'
                       AND account.credential_generation = $4
                )",
        )
        .bind(now.saturating_add(cooldown_millis.min(60_000) as i64))
        .bind(now)
        .bind(upstream_account_id.to_string())
        .bind(credential_generation)
        .bind(lease_token.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

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
        let wait_eligible = matches!(
            snapshot.last_failure_kind.as_str(),
            "connection" | "unavailable"
        ) && allow_transient_probe;
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
