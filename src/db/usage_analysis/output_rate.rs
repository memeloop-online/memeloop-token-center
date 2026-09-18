use super::*;
use crate::model::UsageOutputRate;

/// Retained terminal records are the authority for provenance. Evaluate current
/// compaction observations on every read so late conversation projection cannot
/// permanently contaminate a throughput rollup. No accounting rows are changed.
pub(super) async fn attach_output_rates(
    snapshot: &mut UsageAnalysisSnapshot,
    projections: &mut BTreeMap<(String, String), UsageMetricsAccumulator>,
    tenant_id: &str,
    tenant_scoped: bool,
    filter: &UsageAnalysisFilter,
    range: ValidatedUsageAnalysisRange,
) -> Result<(), AppError> {
    let bucket_ms = match range.granularity {
        UsageAnalysisGranularity::Hour => 3_600_000,
        UsageAnalysisGranularity::Day => 86_400_000,
    };
    let tenant = if tenant_scoped {
        "f.tenant_id = $1"
    } else {
        "CAST($1 AS TEXT) = ''"
    };
    // Both range and scope constrain the indexed compact fact relation before
    // primary-key terminal-record lookups. Archived-away records are excluded,
    // never silently promoted from historical unproven token counters.
    let sql = format!(
        r#"
        SELECT (f.created_at / {bucket_ms}) * {bucket_ms} AS bucket_start,
               CAST(COUNT(*) AS BIGINT) AS requests,
               CAST(SUM(r.output_tokens) AS BIGINT) AS output_tokens,
               CAST(SUM(r.duration_ms) AS BIGINT) AS duration_ms
          FROM request_stats_facts f
          JOIN request_records r ON r.id = f.request_id AND r.created_at = f.created_at
          LEFT JOIN key_records k ON k.id = f.key_id AND k.tenant_id = f.tenant_id
          LEFT JOIN principals p ON p.id = k.principal_id AND p.tenant_id = f.tenant_id
         WHERE {tenant} AND f.created_at >= $2 AND f.created_at <= $3
           AND ($4 = '' OR f.key_id = $4)
           AND ($5 = '' OR f.model = $5)
           AND ($6 = '' OR ($6 = 'anthropic' AND (f.protocol = 'anthropic' OR f.protocol LIKE 'anthropic-%'))
                        OR ($6 = 'openai' AND (f.protocol = 'openai' OR f.protocol LIKE 'openai-%') AND f.protocol <> 'openai-image'))
           AND ($7 = '' OR $7 = 'success')
           AND ($8 = '' OR f.error_code = $8)
           AND ($9 = '' OR ($9 = 'unassigned' AND f.upstream_account_id = '') OR f.upstream_account_id = $9)
           AND ($10 = '' OR f.model_route_id = $10)
           AND ($11 = '' OR LOWER(COALESCE(k.alias,
                   '__retired_credential__')) LIKE $11 ESCAPE '\')
           AND ($12 = '' OR LOWER(COALESCE(p.external_id,
                   '__retired_principal__')) LIKE $12 ESCAPE '\')
           AND r.completed_at IS NOT NULL AND r.status_code >= 200 AND r.status_code < 300
           AND (r.error_code IS NULL OR r.error_code = '')
           AND r.usage_basis = 'provider_reported' AND r.duration_ms > 0
           AND r.output_tokens >= 0
           AND (f.protocol = 'openai' OR (f.protocol LIKE 'openai-%' AND f.protocol <> 'openai-image')
                OR f.protocol = 'anthropic' OR f.protocol LIKE 'anthropic-%')
           AND NOT EXISTS (
               SELECT 1 FROM conversation_observations o
                WHERE o.request_id = r.id AND o.key_id = r.key_id
                  AND o.cluster_id = r.conversation_cluster_id AND o.compaction = 1
           )
         GROUP BY (f.created_at / {bucket_ms}) * {bucket_ms}
    "#
    );
    let rows = snapshot
        .fetch_all(
            "output_rate",
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(tenant_id)
                .bind(range.from_created_at)
                .bind(range.to_created_at)
                .bind(filter.key_id.map(|id| id.to_string()).unwrap_or_default())
                .bind(filter.model.as_deref().unwrap_or_default())
                .bind(filter.protocol.as_deref().unwrap_or_default())
                .bind(filter.status.as_deref().unwrap_or_default())
                .bind(filter.error_code.as_deref().unwrap_or_default())
                .bind(
                    filter
                        .upstream_account_id
                        .as_ref()
                        .map(UsageAnalysisUpstreamFilter::sql_value)
                        .unwrap_or_default(),
                )
                .bind(filter.route_id.map(|id| id.to_string()).unwrap_or_default())
                .bind(search_prefix(filter.key_alias.as_deref()))
                .bind(search_prefix(filter.principal.as_deref())),
        )
        .await?;
    let mut total = UsageOutputRate::default();
    projections
        .entry(("summary".to_owned(), "summary".to_owned()))
        .or_default();
    // An explicit zero sample count differs from older APIs lacking this field.
    for ((kind, _), accumulator) in projections.iter_mut() {
        if kind == "time" || kind == "summary" {
            accumulator.output_rate = Some(UsageOutputRate::default());
        }
    }
    for row in rows {
        let rate = UsageOutputRate {
            requests: row.try_get("requests")?,
            output_tokens: row.try_get("output_tokens")?,
            duration_ms: row.try_get("duration_ms")?,
        };
        total.requests = total.requests.saturating_add(rate.requests);
        total.output_tokens = total.output_tokens.saturating_add(rate.output_tokens);
        total.duration_ms = total.duration_ms.saturating_add(rate.duration_ms);
        let bucket: i64 = row.try_get("bucket_start")?;
        if let Some(accumulator) = projections.get_mut(&("time".to_owned(), bucket.to_string())) {
            accumulator.output_rate = Some(rate);
        }
    }
    for ((kind, _), accumulator) in projections.iter_mut() {
        if kind == "summary" {
            accumulator.output_rate = Some(total.clone());
        }
    }
    Ok(())
}
