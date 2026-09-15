use super::*;
use crate::{
    provider::AuthorizedUpstreamCandidate, upstream_quota::observations::RoutingQuotaObservation,
};
use std::collections::BTreeMap;

impl Database {
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
        for row in query.fetch_all(&self.pool).await? {
            let value: RoutingQuotaObservation =
                serde_json::from_str(&row.try_get::<String, _>("observation_json")?)
                    .map_err(|_| AppError::Internal)?;
            if value.observed_at <= now
                && value.valid_until > now
                && value
                    .windows
                    .iter()
                    .all(|window| window.reset_at.is_none_or(|at| at > now))
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
