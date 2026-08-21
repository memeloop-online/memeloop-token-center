use std::collections::BTreeMap;

use sqlx::{Row, any::AnyRow};

use super::{AppError, UsageMetricsAccumulator};

#[derive(Default)]
pub(super) struct SessionUsageAccumulator {
    pub(super) id: String,
    pub(super) key_id: String,
    pub(super) key_alias: String,
    pub(super) metrics: UsageMetricsAccumulator,
}

pub(super) fn accumulate_session_usage_row(
    projections: &mut BTreeMap<(String, String), SessionUsageAccumulator>,
    row: &AnyRow,
) -> Result<(), AppError> {
    let id: String = row.try_get("bucket_id")?;
    let key_id: String = row.try_get("key_id")?;
    let key_alias: String = row.try_get("key_alias")?;
    let session = projections.entry((id.clone(), key_id.clone())).or_default();
    if session.id.is_empty() {
        session.id = id;
        session.key_id = key_id;
        session.key_alias = key_alias;
    }
    let accumulator = &mut session.metrics;
    accumulator.requests = accumulator
        .requests
        .saturating_add(row.try_get("requests")?);
    accumulator.successful_requests = accumulator
        .successful_requests
        .saturating_add(row.try_get("successful_requests")?);
    accumulator.failed_requests = accumulator
        .failed_requests
        .saturating_add(row.try_get("failed_requests")?);
    accumulator.input_tokens = accumulator
        .input_tokens
        .saturating_add(row.try_get("input_tokens")?);
    accumulator.output_tokens = accumulator
        .output_tokens
        .saturating_add(row.try_get("output_tokens")?);
    accumulator.duration_count = accumulator
        .duration_count
        .saturating_add(row.try_get("duration_count")?);
    accumulator.duration_sum_ms = accumulator
        .duration_sum_ms
        .saturating_add(row.try_get("duration_sum_ms")?);
    let currency: String = row.try_get("currency")?;
    let cost_micros: i64 = row.try_get("cost_micros")?;
    accumulator
        .costs
        .entry(currency)
        .and_modify(|cost| *cost = cost.saturating_add(cost_micros))
        .or_insert(cost_micros);
    Ok(())
}
