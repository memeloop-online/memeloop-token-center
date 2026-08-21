use super::UsageAnalysisGranularity;

pub(super) fn session_usage_dimension_sql(
    granularity: UsageAnalysisGranularity,
    tenant_scoped: bool,
) -> String {
    let (table, bucket_column) = match granularity {
        UsageAnalysisGranularity::Hour => ("session_usage_hourly", "hour_bucket"),
        UsageAnalysisGranularity::Day => ("session_usage_daily", "day_bucket"),
    };
    let tenant_predicate = if tenant_scoped {
        "AND {alias}.tenant_id = $1"
    } else {
        "AND CAST($1 AS TEXT) = ''"
    };
    let filters = |alias: &str| {
        format!(
            r#"{tenant_predicate}
              AND ($2 = '' OR {alias}.key_id = $2)
              AND ($5 = '' OR {alias}.model = $5)
              AND ($6 = '' OR {alias}.protocol = $6)
              AND ($7 = ''
                   OR ($7 = 'success' AND {alias}.status_class = 'success')
                   OR ($7 = 'error' AND {alias}.status_class = 'failure'))
              AND ($8 = '' OR {alias}.error_code = $8)
              AND ($9 = ''
                   OR ($9 = 'unassigned' AND {alias}.upstream_account_id = '')
                   OR {alias}.upstream_account_id = $9)
              AND ($10 = '' OR {alias}.model_route_id = $10)"#,
            tenant_predicate = tenant_predicate.replace("{alias}", alias),
        )
    };
    let rollup_filters = filters("rollup");
    let fact_filters = filters("fact");
    format!(
        r#"WITH session_activity AS (
               SELECT rollup.tenant_id, rollup.key_id, rollup.session_id,
                      rollup.model, rollup.protocol, rollup.status_class,
                      rollup.error_code, rollup.upstream_account_id,
                      rollup.model_route_id, rollup.currency, rollup.requests,
                      rollup.input_tokens, rollup.output_tokens,
                      rollup.duration_count, rollup.duration_sum_ms,
                      rollup.cost_micros
                 FROM {table} rollup
                WHERE rollup.{bucket_column} >= $3 AND rollup.{bucket_column} < $4
                  {rollup_filters}
               UNION ALL
               SELECT fact.tenant_id, fact.key_id, fact.session_id,
                      fact.model,
                      CASE WHEN fact.protocol = 'anthropic' OR fact.protocol LIKE 'anthropic-%'
                           THEN 'anthropic'
                           WHEN fact.protocol = 'openai-image' THEN 'openai-image'
                           ELSE 'openai' END,
                      fact.status_class, fact.error_code, fact.upstream_account_id,
                      fact.model_route_id, fact.currency, 1, fact.input_tokens,
                      fact.output_tokens, 1, fact.duration_ms, fact.cost_micros
                 FROM request_stats_facts fact
                WHERE $13 <= $14 AND fact.created_at >= $13 AND fact.created_at <= $14
                  {fact_filters}
               UNION ALL
               SELECT fact.tenant_id, fact.key_id, fact.session_id,
                      fact.model,
                      CASE WHEN fact.protocol = 'anthropic' OR fact.protocol LIKE 'anthropic-%'
                           THEN 'anthropic'
                           WHEN fact.protocol = 'openai-image' THEN 'openai-image'
                           ELSE 'openai' END,
                      fact.status_class, fact.error_code, fact.upstream_account_id,
                      fact.model_route_id, fact.currency, 1, fact.input_tokens,
                      fact.output_tokens, 1, fact.duration_ms, fact.cost_micros
                 FROM request_stats_facts fact
                WHERE $15 <= $16 AND fact.created_at >= $15 AND fact.created_at <= $16
                  {fact_filters}
           ),
           filtered_sessions AS (
               SELECT activity.*, key_record.alias AS key_alias
                 FROM session_activity activity
                 JOIN key_records key_record
                   ON key_record.id = activity.key_id
                  AND key_record.tenant_id = activity.tenant_id
                 JOIN principals principal
                   ON principal.id = key_record.principal_id
                  AND principal.tenant_id = key_record.tenant_id
                WHERE ($11 = '' OR LOWER(key_record.alias) LIKE $11 ESCAPE '\')
                  AND ($12 = '' OR LOWER(principal.external_id) LIKE $12 ESCAPE '\')
           ),
           grouped_sessions AS (
               SELECT key_id, key_alias, session_id, currency,
                      MIN(model) AS primary_model, SUM(requests) AS requests,
                      SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END)
                          AS successful_requests,
                      SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END)
                          AS failed_requests,
                      SUM(input_tokens) AS input_tokens,
                      SUM(output_tokens) AS output_tokens,
                      SUM(duration_count) AS duration_count,
                      SUM(duration_sum_ms) AS duration_sum_ms,
                      SUM(cost_micros) AS cost_micros
                 FROM filtered_sessions
                GROUP BY key_id, key_alias, session_id, currency
           ),
           with_session_totals AS (
               SELECT grouped_sessions.*,
                      SUM(requests) OVER (PARTITION BY key_id, session_id) AS session_requests,
                      MIN(primary_model) OVER (PARTITION BY key_id, session_id) AS session_model
                 FROM grouped_sessions
           ),
           ranked_sessions AS (
               SELECT with_session_totals.*,
                      DENSE_RANK() OVER (
                          ORDER BY session_requests DESC, key_id ASC, session_id ASC
                      ) AS session_rank
                 FROM with_session_totals
           )
           SELECT 'session' AS bucket_kind, session_id AS bucket_id,
                  CASE WHEN session_id LIKE 'unlinked:%' OR session_model = ''
                       THEN key_alias ELSE key_alias || ' · ' || session_model END
                      AS bucket_label,
                  key_id, key_alias, currency,
                  CAST(requests AS BIGINT) AS requests,
                  CAST(successful_requests AS BIGINT) AS successful_requests,
                  CAST(failed_requests AS BIGINT) AS failed_requests,
                  CAST(input_tokens AS BIGINT) AS input_tokens,
                  CAST(output_tokens AS BIGINT) AS output_tokens,
                  CAST(duration_count AS BIGINT) AS duration_count,
                  CAST(duration_sum_ms AS BIGINT) AS duration_sum_ms,
                  CAST(cost_micros AS BIGINT) AS cost_micros
             FROM ranked_sessions
            WHERE session_rank <= 100
            ORDER BY session_rank, session_id, currency"#,
    )
}
