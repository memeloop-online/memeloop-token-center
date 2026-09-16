use super::super::*;
use super::stats::{
    FILTERED_ACTIVITY_SOURCE_FACTS, FILTERED_ACTIVITY_SOURCE_PENDING,
    FILTERED_ACTIVITY_SOURCE_ROLLUPS, StatsFilter, validate_stats_filter,
};

const DAY_MILLIS: i64 = 86_400_000;
const EDGE_PREDICATE: &str = "  AND (f.created_at < $17 OR f.created_at >= $18)";

// The pricing page normally asks for the global, unfiltered 30-day model
// total. The facts and rollups already carry the immutable model and usage
// fields. We still restrict them to an eligible key set so historic orphan
// facts have the same visibility as the authoritative statistics query, but
// compute that key/principal/tenant relation once instead of per UNION arm.
// The four placeholders are deliberately consecutive so this statement has a
// small bind set of its own; filtered and scoped requests retain the complete
// operator-statistics source below.
const GLOBAL_UNFILTERED_PRICING_ACTIVITY_SOURCE: &str = r#"
SELECT a.model,
       a.input_tokens,
       a.output_tokens,
       a.requests
  FROM request_daily_aggregates a
  JOIN pricing_visible_keys k ON k.id = a.key_id AND k.tenant_id = a.tenant_id
 WHERE a.day_bucket >= $3 / 86400000
   AND a.day_bucket < $4 / 86400000
UNION ALL
SELECT f.model,
       CASE WHEN f.protocol = 'audio-transcription' THEN 0 ELSE f.input_tokens END AS input_tokens,
       CASE WHEN f.protocol = 'audio-transcription' THEN 0 ELSE f.output_tokens END AS output_tokens,
       CAST(1 AS BIGINT) AS requests
  FROM request_stats_facts f
  JOIN pricing_visible_keys k ON k.id = f.key_id AND k.tenant_id = f.tenant_id
 WHERE f.created_at >= $1 AND f.created_at <= $2
   AND f.created_at < $3
UNION ALL
SELECT f.model,
       CASE WHEN f.protocol = 'audio-transcription' THEN 0 ELSE f.input_tokens END AS input_tokens,
       CASE WHEN f.protocol = 'audio-transcription' THEN 0 ELSE f.output_tokens END AS output_tokens,
       CAST(1 AS BIGINT) AS requests
  FROM request_stats_facts f
  JOIN pricing_visible_keys k ON k.id = f.key_id AND k.tenant_id = f.tenant_id
 WHERE f.created_at >= $1 AND f.created_at <= $2
   AND f.created_at >= $4 AND f.created_at >= $3
UNION ALL
SELECT a.model,
       CAST(0 AS BIGINT) AS input_tokens,
       CAST(0 AS BIGINT) AS output_tokens,
       a.requests
  FROM generation_daily_aggregates a
  JOIN pricing_visible_keys k ON k.id = a.key_id AND k.tenant_id = a.tenant_id
 WHERE a.day_bucket >= $3 / 86400000
   AND a.day_bucket < $4 / 86400000
UNION ALL
SELECT f.model,
       CAST(0 AS BIGINT) AS input_tokens,
       CAST(0 AS BIGINT) AS output_tokens,
       CAST(1 AS BIGINT) AS requests
  FROM generation_stats_facts f
  JOIN pricing_visible_keys k ON k.id = f.key_id AND k.tenant_id = f.tenant_id
 WHERE f.created_at >= $1 AND f.created_at <= $2
   AND f.created_at < $3
UNION ALL
SELECT f.model,
       CAST(0 AS BIGINT) AS input_tokens,
       CAST(0 AS BIGINT) AS output_tokens,
       CAST(1 AS BIGINT) AS requests
  FROM generation_stats_facts f
  JOIN pricing_visible_keys k ON k.id = f.key_id AND k.tenant_id = f.tenant_id
 WHERE f.created_at >= $1 AND f.created_at <= $2
   AND f.created_at >= $4 AND f.created_at >= $3
"#;

fn is_global_unfiltered(tenant_external_id: Option<&str>, filter: &StatsFilter) -> bool {
    tenant_external_id.is_none()
        && filter.key_id.is_none()
        && filter.model.is_none()
        && filter.protocol.is_none()
        && filter.status.is_none()
        && filter.error_code.is_none()
        && filter.upstream_account_id.is_none()
        && filter.route_id.is_none()
        && filter.min_duration_ms.is_none()
        && filter.max_duration_ms.is_none()
        && filter.min_cost_micros.is_none()
        && filter.max_cost_micros.is_none()
        && filter.key_alias.is_none()
        && filter.principal.is_none()
}

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

fn pricing_stats_sql(tenant_external_id: Option<&str>, filter: &StatsFilter) -> String {
    let global_unfiltered = is_global_unfiltered(tenant_external_id, filter);
    let source = if global_unfiltered {
        GLOBAL_UNFILTERED_PRICING_ACTIVITY_SOURCE.to_owned()
    } else {
        pricing_activity_source(filter)
    };
    // No MATERIALIZED fence: PostgreSQL can prune unused cost/status columns
    // and plan the individual rollup/edge arms. Unlike the operator snapshot,
    // this endpoint needs neither four projections nor currency/window ranks.
    let eligibility = if global_unfiltered {
        r#"pricing_visible_keys AS MATERIALIZED (
    SELECT k.id, k.tenant_id
      FROM key_records k
      JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
      JOIN tenants t ON t.id = k.tenant_id
),
"#
    } else {
        ""
    };
    format!(
        "WITH {eligibility}filtered_activity AS ({source})
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
        // Every non-default user filter remains a bound value. The global
        // default has its own four time binds, so it cannot accidentally reuse
        // a tenant- or credential-scoped query plan.
        let sql = pricing_stats_sql(tenant_external_id, &filter);
        let rows = if is_global_unfiltered(tenant_external_id, &filter) {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(from)
                .bind(to)
                .bind(full_day_from)
                .bind(full_day_to)
                .fetch_all(&self.pool)
                .await?
        } else {
            sqlx::query(sqlx::AssertSqlSafe(sql))
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
                .await?
        };
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
