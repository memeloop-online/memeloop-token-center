use super::*;
use std::collections::BTreeMap;

type Dimension = (usize, Vec<(&'static str, String)>);
pub(super) const TABLES: [&str; 7] = [
    "usage_daily_aggregates",
    "request_daily_aggregates",
    "session_usage_totals",
    "session_usage_hourly",
    "session_usage_daily",
    "usage_analysis_hourly",
    "usage_analysis_daily",
];

pub(super) async fn subtract_costs(
    transaction: &mut Transaction<'_, Any>,
    candidates: &[Candidate],
) -> Result<(), AppError> {
    let mut deltas: BTreeMap<Dimension, i64> = BTreeMap::new();
    for candidate in candidates {
        let delta = candidate
            .cost_micros
            .checked_sub(candidate.corrected_cost_micros)
            .filter(|value| *value > 0)
            .ok_or(AppError::internal("cost_only_subtract_costs_1"))?;
        for dimension in dimensions(candidate) {
            let total = deltas.entry(dimension).or_default();
            *total = total
                .checked_add(delta)
                .ok_or(AppError::internal("cost_only_subtract_costs_2"))?;
        }
    }
    for ((table_index, terms), delta) in deltas {
        let predicates = terms
            .iter()
            .enumerate()
            .map(|(index, (column, _))| {
                let parameter = index + 2;
                if column.ends_with("_bucket") {
                    format!("{column} = CAST(${parameter} AS BIGINT)")
                } else {
                    format!("{column} = ${parameter}")
                }
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let statement = format!(
            "UPDATE {} SET cost_micros = cost_micros - $1 WHERE {predicates} AND cost_micros >= $1",
            TABLES[table_index]
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(statement)).bind(delta);
        for (column, value) in &terms {
            query = if column.ends_with("_bucket") {
                query.bind(
                    value
                        .parse::<i64>()
                        .map_err(|_| AppError::internal("cost_only_subtract_costs_1"))?,
                )
            } else {
                query.bind(value)
            };
        }
        let updated = query.execute(&mut **transaction).await.map_err(|error| {
            tracing::error!(
                stage = "aggregate_cost_delta",
                dimension_category = TABLES[table_index],
                "cost-only projection aggregate update failed"
            );
            AppError::from(error)
        })?;
        if updated.rows_affected() != 1 {
            return Err(AppError::Conflict(format!(
                "cost-only projection stage=aggregate_cost_delta dimension={}: missing aggregate or insufficient cost",
                TABLES[table_index]
            )));
        }
    }
    Ok(())
}

fn dimensions(candidate: &Candidate) -> Vec<Dimension> {
    let common = vec![
        ("tenant_id", candidate.tenant_id.clone()),
        ("key_id", candidate.key_id.clone()),
    ];
    let outcome = vec![
        ("model", candidate.model.clone()),
        ("protocol", candidate.protocol.clone()),
        ("status_class", candidate.status_class.clone()),
        ("error_code", candidate.error_code.clone()),
        ("upstream_account_id", candidate.upstream_account_id.clone()),
        ("model_route_id", candidate.model_route_id.clone()),
        ("currency", candidate.currency.clone()),
    ];
    let day = (candidate.created_at / 86_400_000).to_string();
    let mut usage = vec![
        ("key_id", candidate.key_id.clone()),
        ("day_bucket", day.clone()),
        ("model", candidate.model.clone()),
        ("status_class", candidate.status_class.clone()),
        ("error_code", candidate.error_code.clone()),
    ];
    let mut request = common.clone();
    request.extend(outcome.clone());
    request.push(("day_bucket", day.clone()));
    request.push(("service_tier", candidate.service_tier.clone()));
    let mut session = common.clone();
    session.push(("session_id", candidate.session_id.clone()));
    session.push(("currency", candidate.currency.clone()));
    let mut result = vec![
        (0, std::mem::take(&mut usage)),
        (1, request),
        (2, session.clone()),
    ];
    for (index, column, divisor) in [
        (3, "hour_bucket", 3_600_000),
        (4, "day_bucket", 86_400_000),
        (5, "hour_bucket", 3_600_000),
        (6, "day_bucket", 86_400_000),
    ] {
        let mut terms = common.clone();
        terms.extend(outcome.iter().map(|(column, value)| {
            (
                *column,
                if *column == "protocol" {
                    canonical_protocol(value).to_owned()
                } else {
                    value.clone()
                },
            )
        }));
        terms.push((column, (candidate.created_at / divisor).to_string()));
        if index <= 4 {
            terms.push(("session_id", candidate.session_id.clone()));
        } else {
            terms.push(("service_tier", candidate.service_tier.clone()));
            terms.push((
                "source_kind",
                if candidate.protocol == "audio-transcription" {
                    "generation"
                } else {
                    "request"
                }
                .into(),
            ));
        }
        result.push((index, terms));
    }
    result
}
