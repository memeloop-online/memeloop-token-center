//! Asynchronous, bounded sampling of shared operational leases and egress events.
//! The sampler has no connection to admission or routing decisions.
use crate::db::{Database, upstream_concurrency::ConcurrencySample};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const INTERVAL_SECONDS: u64 = 5;
const HISTORY_POINTS: usize = 180;
const SAMPLE_LIMIT: usize = 4096;
const ATTRIBUTION_LIMIT: usize = 8;
const SERIES_LIMIT: usize = 512;

type SeriesKey = (String, String, String, String);

#[derive(Default)]
struct Cache {
    history: VecDeque<Point>,
    latest: Vec<ConcurrencySample>,
    last_attempt_at: Option<i64>,
    last_error_at: Option<i64>,
}

struct Point {
    observed_at: i64,
    truncated: bool,
    counts: BTreeMap<SeriesKey, usize>,
}

#[derive(Default)]
pub(super) struct Observer {
    started: AtomicBool,
    cache: Arc<Mutex<Cache>>,
}

fn account_key(sample: &ConcurrencySample, basis: &str) -> SeriesKey {
    (
        sample.tenant_external_id.clone(),
        "upstream_account".into(),
        sample
            .upstream_account_id
            .clone()
            .unwrap_or_else(|| "unknown".into()),
        basis.into(),
    )
}

fn proxy_key(sample: &ConcurrencySample) -> Option<SeriesKey> {
    if sample.source != "request_owner"
        || sample.lease_state != "live"
        || sample.egress_state != "unfinished_attempt"
    {
        return None;
    }
    Some((
        sample.tenant_external_id.clone(),
        "network_proxy".into(),
        sample.selected_egress.as_ref()?.concurrency_identity()?,
        "live_owner_with_unfinished_egress_attempt".into(),
    ))
}

fn account_basis(sample: &ConcurrencySample) -> &'static str {
    if sample.source == "request_owner" {
        match sample.lease_state.as_str() {
            "live" => "request_owner_live_lease",
            "expired" => "request_owner_expired_lease",
            _ => "request_owner_missing_lease",
        }
    } else {
        match sample.state.as_str() {
            "running" => "generation_running_state",
            "submitting" => "generation_submitting_state",
            "cancelling" => "generation_cancelling_state",
            _ => "generation_preparing_or_queued_state",
        }
    }
}

fn counts(samples: &[ConcurrencySample]) -> BTreeMap<SeriesKey, usize> {
    let mut counts = BTreeMap::new();
    for sample in samples {
        if sample.source == "account_inventory" {
            counts
                .entry(account_key(sample, "request_owner_live_lease"))
                .or_default();
            continue;
        }
        let basis = account_basis(sample);
        *counts.entry(account_key(sample, basis)).or_default() += 1;
        if let Some(key) = proxy_key(sample) {
            *counts.entry(key).or_default() += 1;
        }
    }
    counts
}

impl Observer {
    pub(super) fn start(&self, database: Database) {
        if self.started.swap(true, Ordering::Relaxed) {
            return;
        }
        let cache = Arc::downgrade(&self.cache);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(INTERVAL_SECONDS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await; // First database observation follows one full interval.
            loop {
                ticker.tick().await;
                if cache.strong_count() == 0 {
                    return;
                }
                let result = tokio::time::timeout(
                    Duration::from_secs(3),
                    database.upstream_concurrency_samples((SAMPLE_LIMIT + 1) as i64),
                )
                .await;
                let Some(cache) = cache.upgrade() else {
                    return;
                };
                let now = chrono::Utc::now().timestamp_millis();
                let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());
                cache.last_attempt_at = Some(now);
                match result {
                    Ok(Ok(mut samples)) => {
                        let mut truncated = samples.len() > SAMPLE_LIMIT;
                        samples.truncate(SAMPLE_LIMIT);
                        let mut counts = counts(&samples);
                        truncated |= counts.len() > SERIES_LIMIT;
                        while counts.len() > SERIES_LIMIT {
                            counts.pop_last();
                        }
                        cache.history.push_back(Point {
                            observed_at: now,
                            truncated,
                            counts,
                        });
                        while cache.history.len() > HISTORY_POINTS {
                            cache.history.pop_front();
                        }
                        cache.latest = samples;
                    }
                    _ => {
                        cache.last_error_at = Some(now);
                    }
                }
                // Failures are gaps, never zero-load observations. Old cache
                // entries expire by wall time even if every subsequent read fails.
                while cache.history.front().is_some_and(|point| {
                    point.observed_at
                        < now - (INTERVAL_SECONDS * HISTORY_POINTS as u64 * 1000) as i64
                }) {
                    cache.history.pop_front();
                }
                if cache.history.is_empty() {
                    cache.latest.clear();
                }
            }
        });
    }

    pub(super) fn snapshot(&self, tenant: Option<&str>) -> Value {
        let cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        let visible = |id: &str| tenant.is_none_or(|tenant| tenant == id);
        let latest = cache.history.back();
        let stale = latest.is_none_or(|point| {
            chrono::Utc::now().timestamp_millis() - point.observed_at
                > (INTERVAL_SECONDS * 2 * 1000) as i64
        });
        let mut keys = BTreeMap::<SeriesKey, usize>::new();
        let mut history_series_truncated = false;
        for point in &cache.history {
            for (key, count) in &point.counts {
                if visible(&key.0) {
                    if keys.len() >= SERIES_LIMIT && !keys.contains_key(key) {
                        history_series_truncated = true;
                        continue;
                    }
                    let peak = keys.entry(key.clone()).or_default();
                    *peak = (*peak).max(*count);
                }
            }
        }
        let series = keys
            .into_iter()
            .map(|(key, peak)| {
                let points = cache
                    .history
                    .iter()
                    .map(|point| {
                        json!({
                            "observed_at": point.observed_at,
                            "value": point.counts.get(&key).copied().unwrap_or(0),
                            "truncated": point.truncated,
                        })
                    })
                    .collect::<Vec<_>>();
                let sample_ids = cache
                    .latest
                    .iter()
                    .filter(|sample| {
                        visible(&sample.tenant_external_id)
                            && (if key.1 == "network_proxy" {
                                proxy_key(sample).as_ref() == Some(&key)
                            } else {
                                sample.upstream_account_id.as_deref().unwrap_or("unknown") == key.2
                                    && sample.tenant_external_id == key.0
                                    && sample.source != "account_inventory"
                                    && account_basis(sample) == key.3
                            })
                    })
                    .take(ATTRIBUTION_LIMIT)
                    .map(|sample| sample.sample_id.clone())
                    .collect::<Vec<_>>();
                json!({
                    "tenant_external_id": key.0, "dimension": key.1, "identity": key.2,
                    "measurement_basis": key.3,
                    "current": latest.filter(|_| !stale).map(|point| point.counts.get(&key).copied().unwrap_or(0)),
                    "sampled_peak": peak, "points": points, "sample_ids": sample_ids,
                })
            })
            .collect::<Vec<_>>();
        let samples = cache
            .latest
            .iter()
            .filter(|sample| {
                visible(&sample.tenant_external_id) && sample.source != "account_inventory"
            })
            .collect::<Vec<_>>();
        let missing_egress = samples
            .iter()
            .filter(|sample| sample.egress_state == "missing_metadata")
            .count();
        let invalid_egress = samples
            .iter()
            .filter(|sample| sample.egress_state == "invalid_metadata")
            .count();
        let missing_account = samples
            .iter()
            .filter(|sample| sample.upstream_account_id.is_none())
            .count();
        json!({
            "metadata_gaps": {"missing_egress_samples": missing_egress, "invalid_egress_samples": invalid_egress, "missing_account_samples": missing_account},
            "contract_version": "upstream_concurrency_v1",
            "scope": "shared_database_operational_leases_and_generation_states",
            "generated_at": chrono::Utc::now().timestamp_millis(),
            "observed_at": latest.map(|point| point.observed_at),
            "last_attempt_at": cache.last_attempt_at, "last_error_at": cache.last_error_at,
            "stale": stale,
            "expired_and_missing_request_leases_excluded": true,
            "sample_interval_ms": INTERVAL_SECONDS * 1000,
            "retention_ms": INTERVAL_SECONDS * HISTORY_POINTS as u64 * 1000,
            "peak_is_sampled_lower_bound": true,
            "series": series, "samples": samples,
            "samples_truncated": history_series_truncated || latest.is_some_and(|point| point.truncated),
            "recommendation": null,
            "application_mode": "manual_only",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_isolate_tenants_and_do_not_present_expired_cache_as_current() {
        let observer = Observer::default();
        let first: SeriesKey = (
            "one".into(),
            "upstream_account".into(),
            "account-one".into(),
            "request_owner_live_lease".into(),
        );
        let second: SeriesKey = (
            "two".into(),
            "upstream_account".into(),
            "account-two".into(),
            "request_owner_live_lease".into(),
        );
        observer.cache.lock().unwrap().history.push_back(Point {
            observed_at: chrono::Utc::now().timestamp_millis(),
            truncated: false,
            counts: BTreeMap::from([(first.clone(), 2), (second, 5)]),
        });
        let snapshot = observer.snapshot(Some("one"));
        assert_eq!(snapshot["series"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["series"][0]["current"], 2);
        assert_eq!(snapshot["series"][0]["sampled_peak"], 2);
        assert!(!snapshot.to_string().contains("account-two"));
        observer
            .cache
            .lock()
            .unwrap()
            .history
            .back_mut()
            .unwrap()
            .observed_at = 1;
        let stale = observer.snapshot(Some("one"));
        assert_eq!(stale["stale"], true);
        assert!(stale["series"][0]["current"].is_null());
    }

    #[test]
    fn stale_or_completed_evidence_never_counts_as_live_proxy_concurrency() {
        let mut sample = ConcurrencySample {
            tenant_external_id: "tenant".into(),
            sample_id: "request".into(),
            source: "request_owner".into(),
            upstream_account_id: Some("account".into()),
            state: "streaming".into(),
            lease_expires_at: Some(100),
            lease_state: "live".into(),
            selected_egress: Some(
                crate::db::egress::SelectedEgress::capture(Some("http://localhost:8080")).unwrap(),
            ),
            egress_attempt_id: Some("attempt".into()),
            egress_upstream_account_id: Some("account".into()),
            egress_state: "unfinished_attempt".into(),
        };
        assert_eq!(counts(&[sample.clone()]).values().sum::<usize>(), 2);
        sample.lease_state = "expired".into();
        assert!(proxy_key(&sample).is_none());
        assert_eq!(counts(&[sample.clone()]).values().sum::<usize>(), 1);
        sample.lease_state = "live".into();
        sample.egress_state = "completed_attempt".into();
        assert!(proxy_key(&sample).is_none());
        sample.egress_state = "missing_metadata".into();
        sample.selected_egress = None;
        assert!(proxy_key(&sample).is_none());
    }
}
