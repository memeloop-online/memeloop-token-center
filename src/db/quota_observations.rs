use super::*;
use crate::upstream_quota::observations::QuotaObservationTarget;
use crate::{
    provider::AuthorizedUpstreamCandidate, upstream_quota::observations::RoutingQuotaObservation,
};
use futures_util::TryStreamExt;
use std::collections::BTreeMap;

impl Database {
    /// Read-only fence before quota attempts; never opens or refreshes a credential.
    pub(crate) async fn quota_read_generation_current(
        &self,
        account_id: Uuid,
        generation: i64,
    ) -> Result<bool, AppError> {
        Ok(sqlx::query("SELECT 1 FROM upstream_accounts a JOIN upstream_credentials c ON c.upstream_account_id=a.id AND c.generation=a.credential_generation WHERE a.id=$1 AND a.credential_generation=$2 AND a.status='active' AND c.revoked_at IS NULL")
            .bind(account_id.to_string())
            .bind(generation)
            .fetch_optional(&self.pool)
            .await?
            .is_some())
    }

    pub(crate) async fn quota_observation_targets(
        &self,
        plugins: &[String],
        now: i64,
        limit: i64,
    ) -> Result<Vec<QuotaObservationTarget>, AppError> {
        let plugin_id = match self.backend {
            DatabaseBackend::PostgreSql => "CAST(g.routing_strategy AS jsonb)->>'plugin_id'",
            DatabaseBackend::Sqlite => "json_extract(g.routing_strategy,'$.plugin_id')",
        };
        let plugin_list = match self.backend {
            DatabaseBackend::PostgreSql => "SELECT jsonb_array_elements_text(CAST($1 AS jsonb))",
            DatabaseBackend::Sqlite => "SELECT value FROM json_each($1)",
        };
        // Normal quota observations are drawn only from enabled route candidates
        // attached to opt-in strategy groups. Exhausted Codex accounts are also
        // observed independently so an external quota reset can restore routing
        // without an operator opening the account page. Both paths share the same
        // persisted lease and refresh cadence below.
        let statement = format!("WITH bound AS (
            SELECT e.upstream_account_id AS id, e.tenant_id FROM model_route_eligible_upstream_accounts e
            JOIN model_routes r ON r.id=e.model_route_id AND r.tenant_id=e.tenant_id AND r.enabled=1 AND r.archived_at IS NULL
            JOIN model_route_included_provider_groups i ON i.model_route_id=r.id AND i.tenant_id=r.tenant_id
            JOIN provider_groups g ON g.id=i.provider_group_id AND g.tenant_id=r.tenant_id
            JOIN upstream_account_provider_groups m ON m.provider_group_id=g.id AND m.tenant_id=g.tenant_id AND m.upstream_account_id=e.upstream_account_id
            WHERE {plugin_id} IN ({plugin_list})
            UNION
            SELECT e.upstream_account_id AS id, e.tenant_id FROM model_route_eligible_upstream_accounts e
            JOIN model_routes r ON r.id=e.model_route_id AND r.tenant_id=e.tenant_id AND r.enabled=1 AND r.archived_at IS NULL
            JOIN model_route_group_memberships m ON m.model_route_id=r.id AND m.tenant_id=r.tenant_id
            JOIN route_groups g ON g.id=m.route_group_id AND g.tenant_id=r.tenant_id
            WHERE {plugin_id} IN ({plugin_list})
        ), targets AS (
            SELECT id,tenant_id,1 AS recovery_priority,0 AS recovery_mark FROM bound
            UNION ALL
            SELECT h.upstream_account_id AS id,a.tenant_id,0 AS recovery_priority,
                h.updated_at AS recovery_mark
            FROM upstream_account_health h
            JOIN upstream_accounts a ON a.id=h.upstream_account_id
                AND a.credential_generation=h.credential_generation
            WHERE a.status='active' AND a.driver='openai-codex'
                AND h.last_failure_kind='quota_exhausted' AND h.probe_lease_until<=$2
        ), prioritized AS (
            SELECT id,tenant_id,MIN(recovery_priority) AS recovery_priority,
                MAX(recovery_mark) AS recovery_mark
            FROM targets GROUP BY id,tenant_id
        ) SELECT a.id,a.tenant_id,t.external_id,a.credential_generation,a.updated_at,
                CASE WHEN p.recovery_priority=0 THEN 1 ELSE 0 END AS recovering_quota,
                p.recovery_mark,
                COALESCE(q.last_attempt_at,0) AS previous_attempt_at,
                COALESCE(q.next_refresh_at,0) AS previous_next_refresh_at
            FROM prioritized p JOIN upstream_accounts a ON a.id=p.id AND a.tenant_id=p.tenant_id
            JOIN tenants t ON t.id=a.tenant_id AND t.status='active'
            LEFT JOIN upstream_quota_observations q ON q.upstream_account_id=a.id
            WHERE a.status='active' AND a.driver IN ('openai-codex','kimi-oauth','google-antigravity')
                AND COALESCE(q.lease_until,0)<=$2
                AND (q.upstream_account_id IS NULL OR q.next_refresh_at<=$2
                    OR q.config_revision<>a.updated_at OR q.credential_generation<>a.credential_generation
                    OR (p.recovery_priority=0 AND p.recovery_mark>COALESCE(q.last_attempt_at,0)))
            ORDER BY p.recovery_priority,COALESCE(q.last_attempt_at,0),a.id LIMIT $3");
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(serde_json::to_string(plugins).map_err(|_| AppError::Internal)?)
            .bind(now)
            .bind(limit.clamp(1, 16))
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| {
                Ok(QuotaObservationTarget {
                    account_id: Uuid::parse_str(&row.try_get::<String, _>("id")?)
                        .map_err(|_| AppError::Internal)?,
                    tenant_id: Uuid::parse_str(&row.try_get::<String, _>("tenant_id")?)
                        .map_err(|_| AppError::Internal)?,
                    tenant_external_id: row.try_get("external_id")?,
                    generation: row.try_get("credential_generation")?,
                    config_revision: row.try_get("updated_at")?,
                    recovering_quota: row.try_get::<i64, _>("recovering_quota")? == 1,
                    recovery_mark: row.try_get("recovery_mark")?,
                    previous_attempt_at: row.try_get("previous_attempt_at")?,
                    previous_next_refresh_at: row.try_get("previous_next_refresh_at")?,
                })
            })
            .collect()
    }

    pub(crate) async fn claim_quota_observation(
        &self,
        target: &QuotaObservationTarget,
        lease: Uuid,
        now: i64,
        lease_until: i64,
    ) -> Result<bool, AppError> {
        let changed = sqlx::query("INSERT INTO upstream_quota_observations (upstream_account_id,tenant_id,credential_generation,config_revision,last_attempt_at,lease_id,lease_until)
            SELECT id,tenant_id,credential_generation,updated_at,
                CASE WHEN $8>$5 THEN $8 ELSE $5 END,$6,$7 FROM upstream_accounts
            WHERE id=$1 AND tenant_id=$2 AND credential_generation=$3 AND updated_at=$4 AND status='active'
                AND ($8=0 OR EXISTS (SELECT 1 FROM upstream_account_health h
                    WHERE h.upstream_account_id=$1 AND h.credential_generation=$3
                        AND h.last_failure_kind='quota_exhausted' AND h.updated_at=$8
                        AND h.probe_lease_until<=$5))
            ON CONFLICT(upstream_account_id) DO UPDATE SET tenant_id=excluded.tenant_id,credential_generation=excluded.credential_generation,
                config_revision=excluded.config_revision,last_attempt_at=excluded.last_attempt_at,lease_id=excluded.lease_id,lease_until=excluded.lease_until
            WHERE upstream_quota_observations.lease_until<=$5 AND (upstream_quota_observations.next_refresh_at<=$5
                OR upstream_quota_observations.credential_generation<>excluded.credential_generation
                OR upstream_quota_observations.config_revision<>excluded.config_revision
                OR ($8>upstream_quota_observations.last_attempt_at
                    AND EXISTS (SELECT 1 FROM upstream_account_health h
                        WHERE h.upstream_account_id=$1 AND h.credential_generation=$3
                            AND h.last_failure_kind='quota_exhausted' AND h.updated_at=$8
                            AND h.probe_lease_until<=$5)))")
            .bind(target.account_id.to_string()).bind(target.tenant_id.to_string()).bind(target.generation).bind(target.config_revision).bind(now).bind(lease.to_string()).bind(lease_until).bind(target.recovery_mark)
            .execute(&self.pool).await?.rows_affected();
        Ok(changed == 1)
    }

    pub(crate) async fn finish_quota_observation(
        &self,
        target: &QuotaObservationTarget,
        lease: Uuid,
        observation: Option<&RoutingQuotaObservation>,
        next_refresh: i64,
    ) -> Result<(), AppError> {
        let json = observation
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| AppError::Internal)?;
        sqlx::query("UPDATE upstream_quota_observations SET observation_json=$1,valid_until=$2,next_refresh_at=$3,lease_id=NULL,lease_until=0
            WHERE upstream_account_id=$4 AND tenant_id=$5 AND credential_generation=$6 AND config_revision=$7 AND lease_id=$8
                AND EXISTS (SELECT 1 FROM upstream_accounts a WHERE a.id=$4 AND a.tenant_id=$5 AND a.credential_generation=$6 AND a.updated_at=$7 AND a.status='active')")
            .bind(json).bind(observation.map_or(0,|value|value.valid_until)).bind(next_refresh)
            .bind(target.account_id.to_string()).bind(target.tenant_id.to_string()).bind(target.generation).bind(target.config_revision).bind(lease.to_string())
            .execute(&self.pool).await?;
        Ok(())
    }

    pub(crate) async fn routing_quota_observations(
        &self,
        tenant_id: Uuid,
        candidates: &[AuthorizedUpstreamCandidate],
        now: i64,
    ) -> Result<BTreeMap<(Uuid, i64), RoutingQuotaObservation>, AppError> {
        let mut output = BTreeMap::new();
        if candidates.is_empty() {
            return Ok(output);
        }
        if candidates.len() > 1024 {
            return Err(AppError::BadRequest(
                "quota observation candidate limit exceeded".into(),
            ));
        }
        let values = (0..candidates.len())
            .map(|index| {
                format!(
                    "(${},CAST(${} AS BIGINT),CAST(${} AS BIGINT))",
                    index * 3 + 3,
                    index * 3 + 4,
                    index * 3 + 5
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let statement = format!("WITH candidates(account_id,generation,revision) AS (VALUES {values})
            SELECT DISTINCT q.observation_json FROM candidates c
            JOIN upstream_accounts a ON a.id=c.account_id AND a.tenant_id=$1 AND a.status='active'
                AND a.credential_generation=c.generation AND a.updated_at=c.revision
            JOIN upstream_quota_observations q ON q.upstream_account_id=a.id AND q.tenant_id=a.tenant_id
                AND q.credential_generation=c.generation AND q.config_revision=c.revision
            WHERE q.valid_until>$2 AND q.observation_json IS NOT NULL");
        let mut query = sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(tenant_id.to_string())
            .bind(now);
        for candidate in candidates {
            query = query
                .bind(candidate.account_id.to_string())
                .bind(candidate.credential_generation)
                .bind(candidate.transport_revision);
        }
        let mut rows = query.fetch(&self.pool);
        let mut bytes = 0_usize;
        while let Some(row) = rows.try_next().await? {
            let json: String = row.try_get("observation_json")?;
            bytes = bytes.saturating_add(json.len());
            if bytes > crate::plugin::routing::MAX_GROUP_ROUTING_JSON_BYTES {
                tracing::warn!(
                    stage = "quota_observation_payload_limit",
                    bytes,
                    budget = crate::plugin::routing::MAX_GROUP_ROUTING_JSON_BYTES,
                    account_count = candidates.len(),
                    "quota observations exceed existing routing input budget; using native routing"
                );
                return Err(AppError::BadRequest(
                    "quota_observation_payload_limit".into(),
                ));
            }
            let value: RoutingQuotaObservation =
                serde_json::from_str(&json).map_err(|_| AppError::Internal)?;
            if value.observed_at >= 0
                && value.observed_at <= now
                && value.valid_until > now
                && value.windows.len() <= 64
                && value.windows.iter().all(|window| {
                    !window.id.is_empty()
                        && window.id.len() <= 512
                        && window.remaining_fraction.is_none_or(|fraction| {
                            fraction.is_finite() && (0.0..=1.0).contains(&fraction)
                        })
                        && window.reset_at.is_none_or(|at| at > now)
                })
                && candidates.iter().any(|candidate| {
                    candidate.account_id == value.account_id
                        && candidate.credential_generation == value.generation
                        && candidate.transport_revision == value.config_revision
                        && candidate.driver == value.provider
                })
            {
                output.insert((value.account_id, value.generation), value);
            }
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests;
