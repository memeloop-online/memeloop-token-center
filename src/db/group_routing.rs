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

    #[cfg(test)]
    pub(crate) async fn candidate_group_strategy(
        &self,
        tenant_id: Uuid,
        route_id: Uuid,
        account_id: Uuid,
    ) -> Result<Option<CandidateGroupStrategy>, AppError> {
        Ok(self
            .candidate_group_strategies(tenant_id, &[(route_id, account_id)])
            .await?
            .pop()
            .map(|(_, _, binding)| binding))
    }

    /// One SQL statement pins priorities, membership, nullable configuration
    /// and versions for the entire candidate set on both database backends.
    pub(crate) async fn candidate_group_strategies(
        &self,
        tenant_id: Uuid,
        candidates: &[(Uuid, Uuid)],
    ) -> Result<Vec<(Uuid, Uuid, CandidateGroupStrategy)>, AppError> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        if candidates.len() > 1024 {
            return Err(AppError::BadRequest(
                "group routing candidate limit exceeded".into(),
            ));
        }
        let values = (0..candidates.len())
            .map(|index| format!("(${},${})", index * 2 + 2, index * 2 + 3))
            .collect::<Vec<_>>()
            .join(",");
        let statement = format!("WITH input(route_id,account_id) AS (VALUES {values}), candidate_scope AS (
            SELECT DISTINCT input.route_id, input.account_id FROM input
            JOIN model_routes r ON r.id = input.route_id AND r.tenant_id = $1
            JOIN upstream_accounts a ON a.id = input.account_id AND a.tenant_id = $1
        ), bindings AS (
            SELECT c.route_id, c.account_id, g.id, g.routing_priority, g.strategy_version, g.routing_strategy, 'provider' AS kind
            FROM candidate_scope c
            JOIN model_route_included_provider_groups inclusion ON inclusion.model_route_id = c.route_id AND inclusion.tenant_id = $1
            JOIN provider_groups g ON g.id = inclusion.provider_group_id AND g.tenant_id = $1
            JOIN upstream_account_provider_groups m ON m.tenant_id = $1 AND m.provider_group_id = g.id AND m.upstream_account_id = c.account_id
            WHERE g.routing_strategy IS NOT NULL
            UNION ALL
            SELECT c.route_id, c.account_id, g.id, g.routing_priority, g.strategy_version, g.routing_strategy, 'route' AS kind
            FROM candidate_scope c
            JOIN model_route_group_memberships m ON m.model_route_id = c.route_id AND m.tenant_id = $1
            JOIN route_groups g ON g.id = m.route_group_id AND g.tenant_id = $1
            WHERE g.routing_strategy IS NOT NULL
        ), ranked AS (
            SELECT bindings.*, ROW_NUMBER() OVER (PARTITION BY route_id, account_id ORDER BY routing_priority DESC, id ASC, kind ASC) AS position FROM bindings
        ) SELECT route_id, account_id, id, routing_priority, strategy_version, routing_strategy, kind FROM ranked WHERE position = 1");
        // Interpolation contains only host-generated placeholder positions;
        // every identifier value remains a bound parameter.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(statement)).bind(tenant_id.to_string());
        for (route, account) in candidates {
            query = query.bind(route.to_string()).bind(account.to_string());
        }
        query
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| {
                Ok((
                    Uuid::parse_str(&row.try_get::<String, _>("route_id")?)
                        .map_err(|_| AppError::Internal)?,
                    Uuid::parse_str(&row.try_get::<String, _>("account_id")?)
                        .map_err(|_| AppError::Internal)?,
                    CandidateGroupStrategy {
                        id: format!(
                            "{}:{}",
                            row.try_get::<String, _>("id")?,
                            row.try_get::<String, _>("kind")?
                        ),
                        priority: row.try_get("routing_priority")?,
                        version: row.try_get("strategy_version")?,
                        strategy: serde_json::from_str(
                            &row.try_get::<String, _>("routing_strategy")?,
                        )
                        .map_err(|_| AppError::Internal)?,
                    },
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
