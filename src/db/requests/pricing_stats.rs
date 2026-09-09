use super::super::*;
use super::stats::{
    FILTERED_ACTIVITY_SOURCE_FACTS, FILTERED_ACTIVITY_SOURCE_PENDING,
    FILTERED_ACTIVITY_SOURCE_ROLLUPS, StatsFilter, validate_stats_filter,
};

const DAY_MILLIS: i64 = 86_400_000;
const EDGE_PREDICATE: &str = "  AND (f.created_at < $17 OR f.created_at >= $18)";

fn pricing_activity_source(filter: &StatsFilter) -> String {
    if filter.status.as_deref() == Some("pending") {
        return FILTERED_ACTIVITY_SOURCE_PENDING.to_owned();
    }
    if filter.min_duration_ms.is_some()
        || filter.max_duration_ms.is_some()
        || filter.min_cost_micros.is_some()
        || filter.max_cost_micros.is_some()
    {
        return FILTERED_ACTIVITY_SOURCE_FACTS.to_owned();
    }
    // Reuse the authoritative filters rather than creating a second tenant/
    // protocol/alias policy. Split only the fact edge predicate into disjoint
    // index ranges. When there are no complete days, $17 >= $18 and the
    // additional right bound prevents overlap with the left interval.
    FILTERED_ACTIVITY_SOURCE_ROLLUPS
        .split("\nUNION ALL\n")
        .flat_map(|arm| {
            if arm.contains(EDGE_PREDICATE) {
                vec![
                    arm.replace(EDGE_PREDICATE, "  AND f.created_at < $17"),
                    arm.replace(
                        EDGE_PREDICATE,
                        "  AND f.created_at >= $18 AND f.created_at >= $17",
                    ),
                ]
            } else {
                vec![arm.to_owned()]
            }
        })
        .collect::<Vec<_>>()
        .join("\nUNION ALL\n")
}

fn pricing_stats_sql(filter: &StatsFilter) -> String {
    let source = pricing_activity_source(filter);
    // No MATERIALIZED fence: PostgreSQL can prune unused cost/status columns
    // and plan the individual rollup/edge arms. Unlike the operator snapshot,
    // this endpoint needs neither four projections nor currency/window ranks.
    format!(
        "WITH filtered_activity AS ({source})
         SELECT model,
                CAST(SUM(requests) AS BIGINT) AS calls,
                CAST(SUM(input_tokens) AS BIGINT) AS input_tokens,
                CAST(SUM(output_tokens) AS BIGINT) AS output_tokens
           FROM filtered_activity
          GROUP BY model
          ORDER BY calls DESC, model ASC
          LIMIT 100"
    )
}

impl Database {
    /// Exact same window and filters as operator statistics, with only the
    /// top-100 model projection used by the model-pricing page.
    /// Tuple fields: model, calls, input tokens, output tokens.
    pub async fn pricing_model_usage(
        &self,
        tenant_external_id: Option<&str>,
        filter: StatsFilter,
    ) -> Result<Vec<(String, i64, i64, i64)>, AppError> {
        validate_stats_filter(&filter)?;
        let from = filter.from_created_at.expect("validated start");
        let to = filter.to_created_at.expect("validated end");
        let full_day_from = from
            .saturating_add(DAY_MILLIS - 1)
            .div_euclid(DAY_MILLIS)
            .saturating_mul(DAY_MILLIS);
        let full_day_to = to
            .saturating_add(1)
            .div_euclid(DAY_MILLIS)
            .saturating_mul(DAY_MILLIS);
        // SQL is assembled exclusively from static internal source fragments.
        // Every user filter remains a bound value.
        let rows = sqlx::query(sqlx::AssertSqlSafe(pricing_stats_sql(&filter)))
            .bind(tenant_external_id.unwrap_or_default())
            .bind(filter.key_id.map(|id| id.to_string()).unwrap_or_default())
            .bind(from)
            .bind(to)
            .bind(filter.model.as_deref().unwrap_or_default())
            .bind(filter.protocol.as_deref().unwrap_or_default())
            .bind(filter.status.as_deref().unwrap_or_default())
            .bind(filter.error_code.as_deref().unwrap_or_default())
            .bind(
                filter
                    .upstream_account_id
                    .map(|id| id.to_string())
                    .unwrap_or_default(),
            )
            .bind(filter.route_id.map(|id| id.to_string()).unwrap_or_default())
            .bind(filter.min_duration_ms.unwrap_or(-1))
            .bind(filter.max_duration_ms.unwrap_or(-1))
            .bind(filter.min_cost_micros.unwrap_or(-1))
            .bind(filter.max_cost_micros.unwrap_or(-1))
            .bind(search_prefix(filter.key_alias.as_deref()))
            .bind(search_prefix(filter.principal.as_deref()))
            .bind(full_day_from)
            .bind(full_day_to)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get("model")?,
                    row.try_get("calls")?,
                    row.try_get("input_tokens")?,
                    row.try_get("output_tokens")?,
                ))
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "pricing_stats_tests.rs"]
mod tests;
