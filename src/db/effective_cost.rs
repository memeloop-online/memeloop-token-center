//! Shared billing projection semantics.
//!
//! Gross request cost remains immutable for settlement and audit.  User-facing
//! analytics use the effective cost: a terminal failed text request with no
//! observed supplier usage is displayed as zero.  Requests with observed usage,
//! successful requests, and requests that are still pending retain their gross
//! value.

/// Returns the SQL expression used when a terminal request fact is projected
/// into a user-facing aggregate.  The aliases are internal SQL fragments,
/// never caller input.
pub(crate) fn request_fact_effective_cost_sql(
    fact_alias: &str,
    request_alias: &str,
    adjustment_alias: &str,
) -> String {
    format!(
        "CASE WHEN {fact_alias}.status_class = 'failure' AND {request_alias}.usage_basis = 'not_observed' THEN 0 WHEN {fact_alias}.status_class = 'failure' AND {request_alias}.usage_basis <> 'provider_reported' THEN CASE WHEN {fact_alias}.cost_micros - COALESCE({adjustment_alias}.rebate_micros, 0) < 0 THEN 0 ELSE {fact_alias}.cost_micros - COALESCE({adjustment_alias}.rebate_micros, 0) END ELSE {fact_alias}.cost_micros END"
    )
}

pub(crate) fn request_adjustment_join_sql(
    fact_alias: &str,
    feed_alias: &str,
    adjustment_alias: &str,
) -> String {
    format!(
        r#"LEFT JOIN account_settlement_feed {feed_alias}
       ON {feed_alias}.request_kind = 'text'
      AND {feed_alias}.request_id = {fact_alias}.request_id
LEFT JOIN (
       SELECT account_id, settlement_id, SUM(desired_rebate_micros) AS rebate_micros
         FROM settlement_adjustment_states
        GROUP BY account_id, settlement_id
     ) {adjustment_alias}
       ON {adjustment_alias}.account_id = {feed_alias}.account_id
      AND {adjustment_alias}.settlement_id = {feed_alias}.settlement_id"#,
    )
}

pub(crate) fn request_rollup_effective_cost_for_bucket_sql(
    rollup_alias: &str,
    fact_alias: &str,
    request_alias: &str,
    feed_alias: &str,
    adjustment_alias: &str,
    bucket_column: &str,
    bucket_millis: i64,
    analysis_rollup: bool,
) -> String {
    let effective = request_fact_effective_cost_sql(fact_alias, request_alias, adjustment_alias);
    let joins = request_adjustment_join_sql(fact_alias, feed_alias, adjustment_alias);
    let source_kind_predicate = if analysis_rollup {
        format!(
            "AND CASE WHEN {fact_alias}.protocol = 'audio-transcription' THEN 'generation' ELSE 'request' END = {rollup_alias}.source_kind"
        )
    } else {
        String::new()
    };
    let protocol_predicate = if analysis_rollup {
        format!(
            "AND CASE WHEN {fact_alias}.protocol = 'anthropic' OR {fact_alias}.protocol LIKE 'anthropic-%' THEN 'anthropic' WHEN {fact_alias}.protocol = 'openai-image' THEN 'openai-image' WHEN {fact_alias}.protocol = 'audio-transcription' THEN 'audio-transcription' ELSE 'openai' END = {rollup_alias}.protocol"
        )
    } else {
        format!("AND {fact_alias}.protocol = {rollup_alias}.protocol")
    };
    format!(
        r#"{rollup_alias}.cost_micros - COALESCE((
            SELECT SUM({fact_alias}.cost_micros - ({effective}))
              FROM request_stats_facts {fact_alias}
              LEFT JOIN request_records {request_alias}
                ON {request_alias}.id = {fact_alias}.request_id
               AND {request_alias}.created_at = {fact_alias}.created_at
              {joins}
             WHERE {fact_alias}.tenant_id = {rollup_alias}.tenant_id
               AND {fact_alias}.key_id = {rollup_alias}.key_id
               AND {fact_alias}.created_at / {bucket_millis} = {rollup_alias}.{bucket_column}
               AND {fact_alias}.model = {rollup_alias}.model
               {protocol_predicate}
               {source_kind_predicate}
               AND {fact_alias}.status_class = {rollup_alias}.status_class
               AND {fact_alias}.error_code = {rollup_alias}.error_code
               AND {fact_alias}.upstream_account_id = {rollup_alias}.upstream_account_id
               AND {fact_alias}.model_route_id = {rollup_alias}.model_route_id
               AND {fact_alias}.service_tier = {rollup_alias}.service_tier
               AND {fact_alias}.currency = {rollup_alias}.currency
        ), 0)"#,
        bucket_column = bucket_column,
        bucket_millis = bucket_millis,
        protocol_predicate = protocol_predicate,
        source_kind_predicate = source_kind_predicate,
    )
}

pub(crate) fn request_session_rollup_effective_cost_sql(
    rollup_alias: &str,
    fact_alias: &str,
    request_alias: &str,
    feed_alias: &str,
    adjustment_alias: &str,
    bucket_column: &str,
    bucket_millis: i64,
) -> String {
    let effective = request_fact_effective_cost_sql(fact_alias, request_alias, adjustment_alias);
    let joins = request_adjustment_join_sql(fact_alias, feed_alias, adjustment_alias);
    format!(
        r#"{rollup_alias}.cost_micros - COALESCE((
            SELECT SUM({fact_alias}.cost_micros - ({effective}))
              FROM request_stats_facts {fact_alias}
              LEFT JOIN request_records {request_alias}
                ON {request_alias}.id = {fact_alias}.request_id
               AND {request_alias}.created_at = {fact_alias}.created_at
              {joins}
             WHERE {fact_alias}.tenant_id = {rollup_alias}.tenant_id
               AND {fact_alias}.key_id = {rollup_alias}.key_id
               AND {fact_alias}.session_id = {rollup_alias}.session_id
               AND {fact_alias}.created_at / {bucket_millis} = {rollup_alias}.{bucket_column}
               AND {fact_alias}.model = {rollup_alias}.model
               AND CASE WHEN {fact_alias}.protocol = 'anthropic' OR {fact_alias}.protocol LIKE 'anthropic-%' THEN 'anthropic' WHEN {fact_alias}.protocol = 'openai-image' THEN 'openai-image' WHEN {fact_alias}.protocol = 'audio-transcription' THEN 'audio-transcription' ELSE 'openai' END = {rollup_alias}.protocol
               AND {fact_alias}.status_class = {rollup_alias}.status_class
               AND {fact_alias}.error_code = {rollup_alias}.error_code
               AND {fact_alias}.upstream_account_id = {rollup_alias}.upstream_account_id
               AND {fact_alias}.model_route_id = {rollup_alias}.model_route_id
               AND {fact_alias}.currency = {rollup_alias}.currency
        ), 0)"#,
        bucket_column = bucket_column,
        bucket_millis = bucket_millis,
    )
}

/// Projects a persisted session total using the same effective request cost
/// as the fact/rollup analytics sources.  Session totals predate the billing
/// provenance columns, so the correction is correlated through request facts
/// and deliberately leaves archived-only rows unchanged.
pub(crate) fn request_session_total_effective_cost_sql(
    totals_alias: &str,
    fact_alias: &str,
    request_alias: &str,
    feed_alias: &str,
    adjustment_alias: &str,
) -> String {
    let effective = request_fact_effective_cost_sql(fact_alias, request_alias, adjustment_alias);
    let joins = request_adjustment_join_sql(fact_alias, feed_alias, adjustment_alias);
    format!(
        r#"COALESCE({totals_alias}.cost_micros, 0) - COALESCE((
            SELECT SUM({fact_alias}.cost_micros - ({effective}))
              FROM request_stats_facts {fact_alias}
              LEFT JOIN request_records {request_alias}
                ON {request_alias}.id = {fact_alias}.request_id
               AND {request_alias}.created_at = {fact_alias}.created_at
              {joins}
             WHERE {fact_alias}.tenant_id = {totals_alias}.tenant_id
               AND {fact_alias}.key_id = {totals_alias}.key_id
               AND {fact_alias}.session_id = {totals_alias}.session_id
               AND {fact_alias}.currency = {totals_alias}.currency
        ), 0)"#,
    )
}

/// Fixed aliases for the request statistics source fragments, which are kept
/// as static SQL constants for the filter query planner.
pub(crate) const REQUEST_FACT_EFFECTIVE_COST_SQL: &str = "CASE WHEN f.status_class = 'failure' AND billing_request.usage_basis = 'not_observed' THEN 0 WHEN f.status_class = 'failure' AND billing_request.usage_basis <> 'provider_reported' THEN CASE WHEN f.cost_micros - COALESCE(billing_adjustments.rebate_micros, 0) < 0 THEN 0 ELSE f.cost_micros - COALESCE(billing_adjustments.rebate_micros, 0) END ELSE f.cost_micros END";

pub(crate) const REQUEST_ADJUSTMENT_JOINS: &str = r#"
LEFT JOIN account_settlement_feed billing_feed
       ON billing_feed.request_kind = 'text'
      AND billing_feed.request_id = f.request_id
LEFT JOIN (
       SELECT account_id, settlement_id, SUM(desired_rebate_micros) AS rebate_micros
         FROM settlement_adjustment_states
        GROUP BY account_id, settlement_id
     ) billing_adjustments
       ON billing_adjustments.account_id = billing_feed.account_id
      AND billing_adjustments.settlement_id = billing_feed.settlement_id"#;

pub(crate) fn effective_displayed_cost_micros(
    cost_micros: i64,
    status_code: Option<i64>,
    usage_basis: Option<&str>,
) -> i64 {
    if status_code.is_some_and(|code| !(200..400).contains(&code))
        && usage_basis == Some("not_observed")
    {
        0
    } else {
        cost_micros
    }
}

#[cfg(test)]
mod tests {
    use super::effective_displayed_cost_micros;

    #[test]
    fn only_terminal_not_observed_requests_are_zeroed() {
        assert_eq!(
            effective_displayed_cost_micros(594, Some(503), Some("not_observed")),
            0
        );
        assert_eq!(
            effective_displayed_cost_micros(594, Some(502), Some("not_observed")),
            0
        );
        assert_eq!(
            effective_displayed_cost_micros(594, Some(499), Some("not_observed")),
            0
        );
        assert_eq!(
            effective_displayed_cost_micros(594, None, Some("not_observed")),
            594
        );
        assert_eq!(
            effective_displayed_cost_micros(594, Some(200), Some("not_observed")),
            594
        );
        assert_eq!(
            effective_displayed_cost_micros(594, Some(503), Some("provider_reported")),
            594
        );
        assert_eq!(effective_displayed_cost_micros(594, Some(503), None), 594);
    }
}
