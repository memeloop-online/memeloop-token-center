//! Strategy selection follows authorization; these queries never grant routes.
use super::routing::TransientHealthSignal;
use super::{AppError, Database, GroupRoutingStrategy};
use sqlx::Row;
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct CandidateGroupStrategy {
    pub id: String,
    pub priority: i32,
    pub version: i64,
    pub strategy: GroupRoutingStrategy,
    pub health: String,
    pub generation: i64,
}

pub(crate) struct ScopedTransientHealthSignal {
    pub(crate) account_id: Uuid,
    pub(crate) generation: i64,
    pub(crate) policy_scope: String,
    pub(crate) signal: TransientHealthSignal,
}

impl Database {
    #[cfg(test)]
    pub(crate) async fn hold_group_snapshot_pool_for_tests(
        &self,
    ) -> Vec<sqlx::pool::PoolConnection<sqlx::Any>> {
        let mut held = Vec::new();
        for _ in 0..self.pool.options().get_max_connections() {
            held.push(self.pool.acquire().await.expect("test pool connection"));
        }
        held
    }
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
            .candidate_group_strategies(tenant_id, &[(route_id, account_id, 1)])
            .await?
            .pop()
            .map(|(_, _, binding)| binding))
    }

    /// One SQL statement pins priorities, membership, nullable configuration
    /// and versions for the entire candidate set on both database backends.
    pub(crate) async fn candidate_group_strategies(
        &self,
        tenant_id: Uuid,
        candidates: &[(Uuid, Uuid, i64)],
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
            .map(|index| {
                format!(
                    "(${},${},CAST(${} AS BIGINT))",
                    index * 3 + 2,
                    index * 3 + 3,
                    index * 3 + 4
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let statement = format!("WITH input(route_id,account_id,generation) AS (VALUES {values}), candidate_scope AS (
            SELECT DISTINCT input.route_id, input.account_id, input.generation,
                CASE WHEN a.status <> 'active' OR a.credential_generation <> input.generation THEN 'authentication'
                     WHEN COALESCE(h.consecutive_failures,0) = 0 THEN 'healthy'
                     ELSE h.last_failure_kind END AS health
            FROM input
            JOIN model_routes r ON r.id = input.route_id AND r.tenant_id = $1
            JOIN upstream_accounts a ON a.id = input.account_id AND a.tenant_id = $1
            LEFT JOIN upstream_account_health h ON h.upstream_account_id = a.id AND h.credential_generation = input.generation
        ), bindings AS (
            SELECT c.route_id, c.account_id, g.id, g.routing_priority, g.strategy_version, g.routing_strategy, 'provider' AS kind, c.health, c.generation
            FROM candidate_scope c
            JOIN model_route_included_provider_groups inclusion ON inclusion.model_route_id = c.route_id AND inclusion.tenant_id = $1
            JOIN provider_groups g ON g.id = inclusion.provider_group_id AND g.tenant_id = $1
            JOIN upstream_account_provider_groups m ON m.tenant_id = $1 AND m.provider_group_id = g.id AND m.upstream_account_id = c.account_id
            WHERE g.routing_strategy IS NOT NULL
            UNION ALL
            SELECT c.route_id, c.account_id, g.id, g.routing_priority, g.strategy_version, g.routing_strategy, 'route' AS kind, c.health, c.generation
            FROM candidate_scope c
            JOIN model_route_group_memberships m ON m.model_route_id = c.route_id AND m.tenant_id = $1
            JOIN route_groups g ON g.id = m.route_group_id AND g.tenant_id = $1
            WHERE g.routing_strategy IS NOT NULL
        ), ranked AS (
            SELECT bindings.*, ROW_NUMBER() OVER (PARTITION BY route_id, account_id, generation ORDER BY routing_priority DESC, id ASC, kind ASC) AS position FROM bindings
        ) SELECT route_id, account_id, id, routing_priority, strategy_version, routing_strategy, kind, health, generation
            FROM ranked WHERE position = 1");
        // Interpolation contains only host-generated placeholder positions;
        // every identifier value remains a bound parameter.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(statement)).bind(tenant_id.to_string());
        for (route, account, generation) in candidates {
            query = query
                .bind(route.to_string())
                .bind(account.to_string())
                .bind(*generation);
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
                        health: row.try_get("health")?,
                        generation: row.try_get("generation")?,
                        strategy: serde_json::from_str(
                            &row.try_get::<String, _>("routing_strategy")?,
                        )
                        .map_err(|_| AppError::Internal)?,
                    },
                ))
            })
            .collect()
    }

    /// Read only exact v2 policy scopes. The legacy schema-103 signal table is
    /// intentionally excluded so rolling old writers can never feed v2
    /// short-window decisions.
    pub(crate) async fn group_routing_v2_transient_signals(
        &self,
        scopes: &[(Uuid, i64, String)],
    ) -> Result<Vec<ScopedTransientHealthSignal>, AppError> {
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        if scopes.len() > 1024 {
            return Err(AppError::Internal);
        }
        let values = (0..scopes.len())
            .map(|index| {
                format!(
                    "(${},CAST(${} AS BIGINT),${})",
                    index * 3 + 1,
                    index * 3 + 2,
                    index * 3 + 3
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let statement = format!(
            "WITH input(account_id,generation,policy_scope) AS (VALUES {values})
             SELECT input.account_id, input.generation, input.policy_scope,
                    signal.sample_count, signal.ewma_micros, signal.last_observed_at,
                    signal.recovery_successes, signal.revision,
                    signal.transient_window_ms, signal.window_started_at
               FROM input
               JOIN upstream_accounts account ON account.id = input.account_id
                    AND account.status = 'active'
                    AND account.credential_generation = input.generation
               JOIN group_routing_v2_transient_health_signals signal
                 ON signal.upstream_account_id = input.account_id
                AND signal.credential_generation = input.generation
                AND signal.policy_scope = input.policy_scope"
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(statement));
        for (account, generation, policy_scope) in scopes {
            query = query
                .bind(account.to_string())
                .bind(*generation)
                .bind(policy_scope);
        }
        query
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| {
                Ok(ScopedTransientHealthSignal {
                    account_id: Uuid::parse_str(&row.try_get::<String, _>("account_id")?)
                        .map_err(|_| AppError::Internal)?,
                    generation: row.try_get("generation")?,
                    policy_scope: row.try_get("policy_scope")?,
                    signal: TransientHealthSignal {
                        sample_count: row.try_get("sample_count")?,
                        ewma_micros: row.try_get("ewma_micros")?,
                        last_observed_at: row.try_get("last_observed_at")?,
                        recovery_successes: row.try_get("recovery_successes")?,
                        revision: row.try_get("revision")?,
                        transient_window_ms: row.try_get("transient_window_ms")?,
                        window_started_at: row.try_get("window_started_at")?,
                    },
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
