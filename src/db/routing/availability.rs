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
