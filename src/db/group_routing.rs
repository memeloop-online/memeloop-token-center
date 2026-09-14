//! Strategy selection follows authorization; these queries never grant routes.
use super::{AppError, Database, GroupRoutingStrategy};
use sqlx::Row;
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct CandidateGroupStrategy {
    pub id: String,
    pub priority: i32,
    pub version: i64,
    pub strategy: GroupRoutingStrategy,
}

impl Database {
    pub(crate) async fn has_group_routing_strategies(
        &self,
        tenant_id: Uuid,
    ) -> Result<bool, AppError> {
        let row = sqlx::query("SELECT id FROM provider_groups WHERE tenant_id = $1 AND routing_strategy IS NOT NULL UNION ALL SELECT id FROM route_groups WHERE tenant_id = $1 AND routing_strategy IS NOT NULL LIMIT 1")
            .bind(tenant_id.to_string()).fetch_optional(&self.pool).await?;
        Ok(row.is_some())
    }

    pub(crate) async fn candidate_group_strategy(
        &self,
        tenant_id: Uuid,
        route_id: Uuid,
        account_id: Uuid,
    ) -> Result<Option<CandidateGroupStrategy>, AppError> {
        // IDs are globally unique across each group table; kind is a final
        // deterministic tie-breaker if imported IDs coincide across tables.
        let row = sqlx::query("WITH candidate_scope AS (
            SELECT r.id FROM model_routes r JOIN upstream_accounts a ON a.tenant_id = r.tenant_id
            WHERE r.tenant_id = $1 AND r.id = $2 AND a.id = $3
        ) SELECT g.id, g.routing_priority, g.strategy_version, g.routing_strategy, 'provider' AS kind
            FROM provider_groups g JOIN upstream_account_provider_groups m ON m.tenant_id = g.tenant_id AND m.provider_group_id = g.id
            WHERE g.tenant_id = $1 AND m.upstream_account_id = $3 AND g.routing_strategy IS NOT NULL
              AND EXISTS (SELECT 1 FROM candidate_scope)
              AND EXISTS (SELECT 1 FROM model_route_included_provider_groups inclusion
                  WHERE inclusion.tenant_id = g.tenant_id AND inclusion.model_route_id = $2 AND inclusion.provider_group_id = g.id)
            UNION ALL
            SELECT g.id, g.routing_priority, g.strategy_version, g.routing_strategy, 'route' AS kind
            FROM route_groups g JOIN model_route_group_memberships m ON m.tenant_id = g.tenant_id AND m.route_group_id = g.id
            WHERE g.tenant_id = $1 AND m.model_route_id = $2 AND g.routing_strategy IS NOT NULL
              AND EXISTS (SELECT 1 FROM candidate_scope)
            ORDER BY routing_priority DESC, id ASC, kind ASC LIMIT 1")
            .bind(tenant_id.to_string()).bind(route_id.to_string()).bind(account_id.to_string()).fetch_optional(&self.pool).await?;
        row.map(|row| {
            Ok(CandidateGroupStrategy {
                id: format!(
                    "{}:{}",
                    row.try_get::<String, _>("kind")?,
                    row.try_get::<String, _>("id")?
                ),
                priority: row.try_get("routing_priority")?,
                version: row.try_get("strategy_version")?,
                strategy: serde_json::from_str(&row.try_get::<String, _>("routing_strategy")?)
                    .map_err(|_| AppError::Internal)?,
            })
        })
        .transpose()
    }
}

#[cfg(test)]
mod tests;
