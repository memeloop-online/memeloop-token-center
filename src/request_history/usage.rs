use super::{
    ClickHouse, Dimension, HistoryCursor, HistoryError, HistoryQuery, HistoryScope,
    HistorySnapshot, PredicateValue,
};
use crate::{
    db::{
        RequestListFilter, UsageAnalysisFilter, UsageAnalysisProjection,
        UsageAnalysisUpstreamFilter,
    },
    model::*,
};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Default)]
pub(crate) struct Metrics {
    pub requests: i64,
    pub success: i64,
    pub failed: i64,
    input: i64,
    output: i64,
    cached: i64,
    writes: i64,
    units: i64,
    durations: i64,
    duration_sum: i64,
    histogram: [i64; 12],
    costs: BTreeMap<String, i64>,
    rate: UsageOutputRate,
}

pub(crate) const BOUNDS: [i64; 12] = [
    10,
    50,
    100,
    250,
    500,
    1000,
    2500,
    5000,
    10000,
    30000,
    60000,
    i64::MAX,
];
pub(crate) fn percentile(count: i64, histogram: &[i64; 12], percent: i64) -> (Option<i64>, bool) {
    if count == 0 {
        return (None, false);
    }
    let target = (i128::from(count) * i128::from(percent) + 99) / 100;
    let mut total = 0_i128;
    for (index, value) in histogram.iter().enumerate() {
        total += i128::from(*value);
        if total >= target {
            return (Some(BOUNDS[index].min(60000)), index == 11);
        }
    }
    (None, false)
}
fn number(value: &Value, field: &str) -> i64 {
    value[field].as_i64().unwrap_or_default()
}
fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value[field].as_str().unwrap_or_default()
}
impl Metrics {
    pub(crate) fn add(&mut self, snapshot: &HistorySnapshot) {
        let fact = &snapshot.read_model["fact"];
        let payload = snapshot.payload.as_ref().unwrap_or(&Value::Null);
        self.requests += 1;
        let success = text(fact, "status_class") == "success";
        self.success += i64::from(success);
        // Every terminal client failure remains in the overall denominator,
        // independently from financial certainty and upstream-health evidence.
        self.failed += i64::from(!success);
        let input = number(fact, "input_tokens");
        let cached = number(fact, "cached_input_tokens");
        let writes = number(fact, "cache_write_tokens");
        self.input += input.saturating_sub(cached).saturating_sub(writes).max(0);
        self.output += number(fact, "output_tokens");
        self.cached += cached;
        self.writes += writes;
        self.units += number(fact, "generation_units");
        if let Some(duration) = fact["duration_ms"].as_i64() {
            self.durations += 1;
            self.duration_sum += duration;
            self.histogram[BOUNDS
                .iter()
                .position(|bound| duration <= *bound)
                .unwrap_or(11)] += 1;
        }
        if let (Some(cost), Some(currency)) =
            (fact["cost_micros"].as_i64(), fact["currency"].as_str())
            && !currency.is_empty()
        {
            *self.costs.entry(currency.into()).or_default() += cost;
        }
        let protocol = text(payload, "protocol");
        if success
            && snapshot.identity.source == "native"
            && (protocol == "openai"
                || (protocol.starts_with("openai-") && protocol != "openai-image")
                || protocol == "anthropic"
                || protocol.starts_with("anthropic-"))
            && payload["status_code"]
                .as_i64()
                .is_some_and(|s| (200..300).contains(&s))
            && payload["error_code"].as_str().is_none_or(str::is_empty)
            && payload["usage_basis"] == "provider_reported"
            && payload["compaction"] != true
            && !payload["completed_at"].is_null()
            && let (Some(duration), Some(output)) = (
                payload["duration_ms"].as_i64().filter(|v| *v > 0),
                payload["output_tokens"].as_i64().filter(|v| *v >= 0),
            )
        {
            self.rate.requests += 1;
            self.rate.output_tokens += output;
            self.rate.duration_ms += duration;
        }
    }
    pub(crate) fn finish(&self, p95: bool, output_rate: bool) -> UsageAnalysisMetrics {
        let (percentile, capped) = percentile(self.durations, &self.histogram, 95);
        UsageAnalysisMetrics {
            output_rate: output_rate.then(|| self.rate.clone()),
            requests: self.requests,
            success: self.success,
            failed: self.failed,
            input_tokens: self.input,
            output_tokens: self.output,
            cached_input_tokens: self.cached,
            cache_write_tokens: self.writes,
            generation_units: self.units,
            avg_duration_ms: (self.durations > 0)
                .then(|| self.duration_sum as f64 / self.durations as f64),
            p95_duration_ms: if p95 { percentile } else { None },
            p95_is_capped: p95 && capped,
            costs: self
                .costs
                .iter()
                .map(|(currency, cost)| UsageAnalysisCost {
                    currency: currency.clone(),
                    cost: micros_to_decimal_string(*cost),
                })
                .collect(),
        }
    }
    pub(crate) fn monitoring(&self) -> MonitoringMetrics {
        let m = self.finish(true, false);
        let all_input = self.input + self.cached + self.writes;
        MonitoringMetrics {
            requests: self.requests,
            successful_requests: self.success,
            failed_requests: self.failed,
            total_tokens: all_input + self.output,
            cache_rate: (all_input > 0).then(|| self.cached as f64 / all_input as f64),
            avg_duration_ms: m.avg_duration_ms,
            p95_duration_ms: m.p95_duration_ms,
            p95_is_capped: m.p95_is_capped,
            costs: m.costs,
        }
    }
}

impl ClickHouse {
    pub(crate) async fn scan(
        &self,
        query: HistoryQuery,
        visit: impl FnMut(HistorySnapshot) -> Result<(), HistoryError>,
    ) -> Result<(), HistoryError> {
        self.scan_mode(query, false, visit).await
    }

    pub(crate) async fn scan_mode(
        &self,
        mut query: HistoryQuery,
        statistics: bool,
        mut visit: impl FnMut(HistorySnapshot) -> Result<(), HistoryError>,
    ) -> Result<(), HistoryError> {
        loop {
            let mut page = self.read_snapshots(&query, statistics).await?;
            page.truncate(query.limit);
            let Some(last) = page.last() else {
                break;
            };
            query.cursor = Some(HistoryCursor {
                before_created_at: last.identity.created_at,
                before_id: last.identity.request_id,
            });
            for snapshot in page {
                visit(snapshot)?;
            }
        }
        Ok(())
    }

    pub(crate) async fn pricing_usage(
        &self,
        query: HistoryQuery,
    ) -> Result<Vec<(String, i64, i64, i64)>, HistoryError> {
        let mut models = BTreeMap::<String, (i64, i64, i64)>::new();
        let pending = query.predicates.iter().any(|p| {
            p.dimension == Dimension::Status
                && matches!(&p.value, PredicateValue::Text(value) if value == "pending")
        });
        self.scan_mode(query, true, |snapshot| {
            let settled = snapshot.read_model["fact"]["settled_fact"] == true;
            if !pending && !settled {
                return Ok(());
            }
            if let Some(stats) = snapshot.statistics {
                let model = snapshot
                    .payload
                    .as_ref()
                    .map(|p| {
                        text(
                            snapshot
                                .read_model
                                .get("fact")
                                .filter(|f| f["model"].is_string())
                                .unwrap_or(p),
                            "model",
                        )
                    })
                    .unwrap_or_default()
                    .to_owned();
                let metrics = models.entry(model).or_default();
                metrics.0 += 1;
                metrics.1 += stats.input_tokens;
                metrics.2 += stats.output_tokens;
            }
            Ok(())
        })
        .await?;
        let mut result: Vec<_> = models
            .into_iter()
            .map(|(model, (calls, input, output))| (model, calls, input, output))
            .collect();
        result.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        result.truncate(100);
        Ok(result)
    }

    pub(crate) async fn usage(
        &self,
        scope: HistoryScope,
        filter: &UsageAnalysisFilter,
        projection: UsageAnalysisProjection,
        from: i64,
        to: i64,
        granularity: &str,
    ) -> Result<UsageAnalysisResponse, HistoryError> {
        let request_filter = RequestListFilter {
            limit: 500,
            from_created_at: Some(from),
            to_created_at: Some(to),
            key_id: filter.key_id,
            model: filter.model.clone(),
            error_code: filter.error_code.clone(),
            upstream_account_id: match filter.upstream_account_id {
                Some(UsageAnalysisUpstreamFilter::Account(id)) => Some(id),
                _ => None,
            },
            route_id: filter.route_id,
            key_alias: filter.key_alias.clone(),
            principal: filter.principal.clone(),
            ..Default::default()
        };
        let query = crate::request_history_api::query_from_request_filter(scope, &request_filter)?;
        let mut summary = Metrics::default();
        let mut times = BTreeMap::<i64, Metrics>::new();
        let mut dimensions = BTreeMap::<(&str, String), (String, Metrics)>::new();
        let mut sessions = BTreeMap::<(uuid::Uuid, String), (String, String, Metrics)>::new();
        let mut heatmap = BTreeMap::<i64, Metrics>::new();
        let mut modalities = BTreeMap::<(String, String), i64>::new();
        let mut billing_units = BTreeMap::<(String, String), i64>::new();
        let bucket_ms = if granularity == "hour" {
            3_600_000
        } else {
            86_400_000
        };
        let full = projection == UsageAnalysisProjection::Full;
        let main = full
            || matches!(
                projection,
                UsageAnalysisProjection::Overview
                    | UsageAnalysisProjection::Trends
                    | UsageAnalysisProjection::Dimensions
                    | UsageAnalysisProjection::OutputRate
            );
        self.scan_mode(query, true, |snapshot| {
            let fact = &snapshot.read_model["fact"];
            if !fact.is_object() {
                return Ok(());
            }
            let payload = snapshot.payload.as_ref().ok_or(HistoryError::Mismatch)?;
            let dimension = |name: &str| fact.get(name).unwrap_or(&payload[name]);
            let raw_protocol = dimension("protocol").as_str().unwrap_or_default();
            let protocol = if snapshot.identity.source == "generation" {
                "generation"
            } else if raw_protocol == "anthropic" || raw_protocol.starts_with("anthropic-") {
                "anthropic"
            } else if raw_protocol == "audio-transcription" {
                "audio-transcription"
            } else if raw_protocol == "openai-image" {
                "openai-image"
            } else {
                "openai"
            };
            let success = fact["status_class"] == "success";
            if filter.protocol.as_deref().is_some_and(|v| v != protocol)
                || filter
                    .status
                    .as_deref()
                    .is_some_and(|v| (v == "success") != success)
                || (filter.upstream_account_id == Some(UsageAnalysisUpstreamFilter::Unassigned)
                    && dimension("upstream_account_id")
                        .as_str()
                        .is_some_and(|v| !v.is_empty()))
            {
                return Ok(());
            }
            let created = snapshot.identity.created_at;
            if main {
                summary.add(&snapshot);
                times
                    .entry(created / bucket_ms * bucket_ms)
                    .or_default()
                    .add(&snapshot);
            }
            let identity = &payload["credential_identity"];
            let key_alias = identity["key_alias"]
                .as_str()
                .unwrap_or("__retired_credential__");
            if full || projection == UsageAnalysisProjection::Dimensions {
                let upstream = dimension("upstream_account_id")
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .unwrap_or("unassigned");
                let upstream_label = snapshot.read_model["upstream_name"].as_str().unwrap_or(
                    if upstream == "unassigned" {
                        "Unassigned"
                    } else {
                        upstream
                    },
                );
                let status = if success { "success" } else { "error" };
                for (kind, id, label) in [
                    (
                        "model",
                        dimension("model").as_str().unwrap_or_default(),
                        dimension("model").as_str().unwrap_or_default(),
                    ),
                    ("key", &snapshot.identity.key_id.to_string(), key_alias),
                    ("upstream", upstream, upstream_label),
                    ("protocol", protocol, protocol),
                    ("status", status, if success { "success" } else { "failed" }),
                    (
                        "error",
                        dimension("error_code").as_str().unwrap_or_default(),
                        dimension("error_code").as_str().unwrap_or_default(),
                    ),
                ] {
                    if kind == "error" && id.is_empty() {
                        continue;
                    }
                    let entry = dimensions
                        .entry((kind, id.into()))
                        .or_insert_with(|| (label.into(), Metrics::default()));
                    entry.1.add(&snapshot);
                }
            }
            if full || projection == UsageAnalysisProjection::Sessions {
                let context = &payload["session_context"];
                let id = context["session_id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("unlinked:{}", snapshot.identity.key_id));
                let model = dimension("model").as_str().unwrap_or_default();
                let entry = sessions
                    .entry((snapshot.identity.key_id, id))
                    .or_insert_with(|| (model.into(), key_alias.into(), Metrics::default()));
                if model < entry.0.as_str() {
                    entry.0 = model.into();
                }
                entry.2.add(&snapshot);
            }
            if full || projection == UsageAnalysisProjection::Heatmap {
                let hour = ((created / 3_600_000) + 72) % 168;
                heatmap.entry(hour).or_default().add(&snapshot);
            }
            if (full || projection == UsageAnalysisProjection::Overview)
                && (snapshot.identity.source == "generation" || protocol == "audio-transcription")
            {
                let currency = text(fact, "currency");
                let unit = fact
                    .get("billing_unit")
                    .unwrap_or(&payload["usage"]["generation"]["billing_unit"])
                    .as_str()
                    .unwrap_or_default();
                let modality = if protocol == "audio-transcription" {
                    "audio"
                } else {
                    fact["modality"].as_str().unwrap_or_default()
                };
                *modalities
                    .entry((modality.into(), currency.into()))
                    .or_default() += number(fact, "generation_units");
                *billing_units
                    .entry((unit.into(), currency.into()))
                    .or_default() += number(fact, "generation_units");
            }
            Ok(())
        })
        .await?;
        let rate = full
            || matches!(
                projection,
                UsageAnalysisProjection::Trends | UsageAnalysisProjection::OutputRate
            );
        let mut response = UsageAnalysisResponse {
            from_created_at: from,
            to_created_at: to,
            granularity: granularity.into(),
            time_zone: "UTC".into(),
            p95_is_approximate: true,
            p95_method: "fixed_histogram_upper_bound_capped_60000ms".into(),
            upstream_grouping: "stable_account".into(),
            summary: summary.finish(true, rate),
            time_series: times
                .into_iter()
                .map(|(bucket_start, m)| UsageAnalysisTimeBucket {
                    bucket_start,
                    metrics: m.finish(true, rate),
                })
                .collect(),
            by_model: vec![],
            by_key: vec![],
            by_session: vec![],
            by_upstream: vec![],
            by_protocol: vec![],
            by_status: vec![],
            errors: vec![],
            heatmap: heatmap
                .into_iter()
                .map(|(hour_of_week, m)| UsageAnalysisHeatmapBucket {
                    hour_of_week,
                    metrics: m.finish(true, false),
                })
                .collect(),
            generation_units_by_modality: modalities
                .into_iter()
                .map(
                    |((modality, currency), units)| UsageAnalysisGenerationUnitsByModality {
                        modality,
                        currency,
                        units,
                    },
                )
                .collect(),
            generation_units_by_billing_unit: billing_units
                .into_iter()
                .map(|((billing_unit, currency), units)| {
                    UsageAnalysisGenerationUnitsByBillingUnit {
                        billing_unit,
                        currency,
                        units,
                    }
                })
                .collect(),
        };
        for ((kind, id), (label, m)) in dimensions {
            let bucket = UsageAnalysisBucket {
                id,
                label,
                metrics: m.finish(true, false),
            };
            match kind {
                "model" => response.by_model.push(bucket),
                "key" => response.by_key.push(bucket),
                "upstream" => response.by_upstream.push(bucket),
                "protocol" => response.by_protocol.push(bucket),
                "status" => response.by_status.push(bucket),
                "error" => response.errors.push(bucket),
                _ => unreachable!(),
            }
        }
        for buckets in [
            &mut response.by_model,
            &mut response.by_key,
            &mut response.by_upstream,
            &mut response.by_protocol,
            &mut response.by_status,
            &mut response.errors,
        ] {
            buckets.sort_by(|a, b| {
                b.metrics
                    .requests
                    .cmp(&a.metrics.requests)
                    .then_with(|| a.id.cmp(&b.id))
            });
            buckets.truncate(100);
        }
        response.by_session = sessions
            .into_iter()
            .map(
                |((key_id, id), (model, key_alias, m))| UsageAnalysisSessionBucket {
                    unlinked: id.starts_with("unlinked:"),
                    label: if id.starts_with("unlinked:") || model.is_empty() {
                        key_alias.clone()
                    } else {
                        format!("{key_alias} · {model}")
                    },
                    id,
                    key_id,
                    key_alias,
                    metrics: m.finish(false, false),
                },
            )
            .collect();
        response.by_session.sort_by(|a, b| {
            b.metrics
                .requests
                .cmp(&a.metrics.requests)
                .then_with(|| a.key_id.cmp(&b.key_id))
                .then_with(|| a.id.cmp(&b.id))
        });
        response.by_session.truncate(100);
        if projection == UsageAnalysisProjection::OutputRate {
            let rate = response.summary.output_rate.take();
            response.summary = UsageAnalysisMetrics {
                output_rate: rate,
                ..Default::default()
            };
            response.time_series.retain(|row| {
                row.metrics
                    .output_rate
                    .as_ref()
                    .is_some_and(|r| r.requests > 0)
            });
            for row in &mut response.time_series {
                row.metrics = UsageAnalysisMetrics {
                    output_rate: row.metrics.output_rate.take(),
                    ..Default::default()
                };
            }
        }
        Ok(response)
    }
}
