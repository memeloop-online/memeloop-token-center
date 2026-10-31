use super::{ClickHouse, HistoryError, HistoryQuery, usage::Metrics};
use crate::model::{MonitoringMetrics, MonitoringTerminalOutcome};
use std::collections::BTreeMap;

pub(crate) struct HistoryMonitoring {
    pub summary: MonitoringMetrics,
    pub latest: Option<i64>,
    pub pairs: Vec<HistoryMonitoringPair>,
}
pub(crate) struct HistoryMonitoringPair {
    pub upstream: String,
    pub model: String,
    pub metrics: MonitoringMetrics,
    pub outcomes: Vec<MonitoringTerminalOutcome>,
}
impl ClickHouse {
    pub(crate) async fn account_availability(
        &self,
        query: HistoryQuery,
    ) -> Result<BTreeMap<String, (MonitoringMetrics, Vec<MonitoringTerminalOutcome>)>, HistoryError>
    {
        let mut accounts = BTreeMap::<String, (Metrics, Vec<MonitoringTerminalOutcome>)>::new();
        self.scan(query, |snapshot| {
            let fact = &snapshot.read_model["fact"];
            if !fact.is_object() {
                return Ok(());
            }
            let payload = snapshot.payload.as_ref().ok_or(HistoryError::Mismatch)?;
            let Some(upstream) = payload["upstream_account_id"].as_str() else {
                return Ok(());
            };
            let account = accounts.entry(upstream.to_owned()).or_default();
            account.0.add(&snapshot);
            account.1.push(MonitoringTerminalOutcome {
                id: snapshot.identity.request_id.to_string(),
                source: if snapshot.identity.source == "native" {
                    "request"
                } else {
                    "generation"
                }
                .into(),
                created_at: snapshot.identity.created_at,
                status: if fact["status_class"] == "success" {
                    "success"
                } else {
                    "failure"
                }
                .into(),
                duration_ms: fact["duration_ms"].as_i64().unwrap_or_default(),
                error_code: payload["error_code"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned),
            });
            account.1.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| b.id.cmp(&a.id))
                    .then_with(|| b.source.cmp(&a.source))
            });
            account.1.truncate(5);
            Ok(())
        })
        .await?;
        Ok(accounts
            .into_iter()
            .map(|(id, (m, events))| (id, (m.monitoring(), events)))
            .collect())
    }

    pub(crate) async fn monitoring(
        &self,
        query: HistoryQuery,
    ) -> Result<HistoryMonitoring, HistoryError> {
        let mut summary = Metrics::default();
        let mut pairs =
            BTreeMap::<(String, String), (Metrics, Vec<MonitoringTerminalOutcome>)>::new();
        let mut latest = None;
        self.scan(query, |snapshot| {
            let fact = &snapshot.read_model["fact"];
            if !fact.is_object() {
                return Ok(());
            }
            let payload = snapshot.payload.as_ref().ok_or(HistoryError::Mismatch)?;
            summary.add(&snapshot);
            latest = Some(latest.unwrap_or(0).max(snapshot.identity.created_at));
            if let Some(upstream) = payload["upstream_account_id"].as_str() {
                let model = payload["model"].as_str().unwrap_or_default();
                let pair = pairs.entry((upstream.into(), model.into())).or_default();
                pair.0.add(&snapshot);
                pair.1.push(MonitoringTerminalOutcome {
                    id: snapshot.identity.request_id.to_string(),
                    source: if snapshot.identity.source == "native" {
                        "request"
                    } else {
                        "generation"
                    }
                    .into(),
                    created_at: snapshot.identity.created_at,
                    status: if fact["status_class"] == "success" {
                        "success"
                    } else {
                        "failure"
                    }
                    .into(),
                    duration_ms: fact["duration_ms"].as_i64().unwrap_or_default(),
                    error_code: payload["error_code"]
                        .as_str()
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned),
                });
                pair.1.sort_by(|a, b| {
                    b.created_at
                        .cmp(&a.created_at)
                        .then_with(|| b.id.cmp(&a.id))
                });
                pair.1.truncate(5);
            }
            Ok(())
        })
        .await?;
        let mut pairs = pairs
            .into_iter()
            .map(
                |((upstream, model), (metrics, outcomes))| HistoryMonitoringPair {
                    upstream,
                    model,
                    metrics: metrics.monitoring(),
                    outcomes,
                },
            )
            .collect::<Vec<_>>();
        pairs.sort_by(|a, b| {
            b.metrics
                .requests
                .cmp(&a.metrics.requests)
                .then_with(|| a.upstream.cmp(&b.upstream))
                .then_with(|| a.model.cmp(&b.model))
        });
        pairs.truncate(10);
        Ok(HistoryMonitoring {
            summary: summary.monitoring(),
            latest,
            pairs,
        })
    }
}
