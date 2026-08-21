use super::UsageAnalysisGranularity;

pub(super) fn generation_usage_dimension_sql(
    granularity: UsageAnalysisGranularity,
    tenant_scoped: bool,
) -> String {
    let (table, bucket_column) = match granularity {
        UsageAnalysisGranularity::Hour => ("generation_usage_dimensions_hourly", "hour_bucket"),
        UsageAnalysisGranularity::Day => ("generation_usage_dimensions_daily", "day_bucket"),
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
              AND ($8 = '' OR {alias}.error_code = $8)
              AND ($9 = ''
                   OR ($9 = 'unassigned' AND {alias}.upstream_account_id = '')
                   OR {alias}.upstream_account_id = $9)
              AND ($10 = '' OR {alias}.model_route_id = $10)"#,
            tenant_predicate = tenant_predicate.replace("{alias}", alias),
        )
    };
    let rollup_filters = filters("a");
    let fact_filters = filters("f");
    format!(
        r#"WITH generation_activity AS (
               SELECT a.tenant_id, a.key_id, a.model, a.status_class, a.error_code,
                      a.upstream_account_id, a.model_route_id, a.modality,
                      a.billing_unit, a.currency, a.units
                 FROM {table} a
                WHERE a.{bucket_column} >= $3 AND a.{bucket_column} < $4
                  {rollup_filters}
               UNION ALL
               SELECT f.tenant_id, f.key_id, f.model, f.status_class, f.error_code,
                      f.upstream_account_id, f.model_route_id, f.modality,
                      f.billing_unit, f.currency, f.billed_units
                 FROM generation_stats_facts f
                WHERE $13 <= $14 AND f.created_at >= $13 AND f.created_at <= $14
                  {fact_filters}
               UNION ALL
               SELECT f.tenant_id, f.key_id, f.model, f.status_class, f.error_code,
                      f.upstream_account_id, f.model_route_id, f.modality,
                      f.billing_unit, f.currency, f.billed_units
                 FROM generation_stats_facts f
                WHERE $15 <= $16 AND f.created_at >= $15 AND f.created_at <= $16
                  {fact_filters}
           ),
           filtered_generation AS (
               SELECT activity.*
                 FROM generation_activity activity
                 JOIN key_records k
                   ON k.id = activity.key_id AND k.tenant_id = activity.tenant_id
                 JOIN principals principal
                   ON principal.id = k.principal_id
                  AND principal.tenant_id = k.tenant_id
                WHERE ($6 = '' OR $6 = 'generation')
                  AND ($7 = ''
                       OR ($7 = 'success' AND activity.status_class = 'success')
                       OR ($7 = 'error' AND activity.status_class = 'failure'))
                  AND ($11 = '' OR LOWER(k.alias) LIKE $11 ESCAPE '\')
                  AND ($12 = '' OR LOWER(principal.external_id) LIKE $12 ESCAPE '\')
           )
           SELECT 'modality' AS dimension, modality AS dimension_id, currency,
                  CAST(COALESCE(SUM(units), 0) AS BIGINT) AS units
             FROM filtered_generation
            GROUP BY modality, currency
           UNION ALL
           SELECT 'billing_unit' AS dimension, billing_unit AS dimension_id, currency,
                  CAST(COALESCE(SUM(units), 0) AS BIGINT) AS units
             FROM filtered_generation
            GROUP BY billing_unit, currency"#,
    )
}
