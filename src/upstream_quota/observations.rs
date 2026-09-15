//! Cross-process quota evidence. This read path never performs supplier I/O.
use crate::{AppState, error::AppError, provider::AuthorizedUpstreamCandidate};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RoutingQuotaObservation {
    pub account_id: Uuid,
    pub generation: i64,
    pub config_revision: i64,
    pub provider: String,
    pub observed_at: i64,
    pub valid_until: i64,
    pub windows: Vec<RoutingQuotaWindow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RoutingQuotaWindow {
    pub id: String,
    pub period_seconds: Option<i64>,
    pub reset_at: Option<i64>,
    pub reset_is_estimated: bool,
    pub remaining_fraction: Option<f64>,
    pub exhausted: Option<bool>,
}

#[derive(Clone, Debug)]
pub(crate) struct QuotaObservationTarget {
    pub account_id: Uuid,
    pub tenant_id: Uuid,
    pub tenant_external_id: String,
    pub generation: i64,
    pub config_revision: i64,
}

fn project(
    account: &crate::provider::UpstreamAccountView,
    snapshot: &super::QuotaSnapshot,
    now: i64,
) -> Option<RoutingQuotaObservation> {
    if snapshot.error_code.is_some() || snapshot.stale || snapshot.freshness != "fresh" {
        return None;
    }
    if snapshot.windows.len() > 64
        || snapshot
            .windows
            .iter()
            .any(|window| window.id.is_empty() || window.id.len() > 512)
    {
        return None;
    }
    let observed_at = snapshot.observed_at?;
    let mut valid_until = snapshot.stale_after?;
    if observed_at > now || valid_until <= now {
        return None;
    }
    let windows = snapshot
        .windows
        .iter()
        .map(|window| {
            if let Some(at) = window.reset_at {
                valid_until = valid_until.min(at);
            }
            let remaining_fraction = match (window.remaining, window.limit, window.used_percent) {
                (Some(remaining), Some(limit), _)
                    if remaining.is_finite()
                        && limit.is_finite()
                        && limit > 0.0
                        && (0.0..=limit).contains(&remaining) =>
                {
                    Some(remaining / limit)
                }
                (_, _, Some(used)) if used.is_finite() && (0.0..=100.0).contains(&used) => {
                    Some(1.0 - used / 100.0)
                }
                _ => None,
            };
            let exhausted = if window.limit_reached == Some(true)
                || window.allowed == Some(false)
                || remaining_fraction == Some(0.0)
            {
                Some(true)
            } else if window.limit_reached == Some(false)
                || window.allowed == Some(true)
                || remaining_fraction.is_some_and(|left| left > 0.0)
            {
                Some(false)
            } else {
                None
            };
            RoutingQuotaWindow {
                id: window.id.clone(),
                period_seconds: window.period_seconds,
                reset_at: window.reset_at,
                reset_is_estimated: window.reset_is_estimated,
                remaining_fraction,
                exhausted,
            }
        })
        .collect::<Vec<_>>();
    if valid_until <= now || windows.is_empty() {
        return None;
    }
    Some(RoutingQuotaObservation {
        account_id: account.id,
        generation: account.credential_generation,
        config_revision: account.updated_at,
        provider: account.driver.clone(),
        observed_at,
        valid_until,
        windows,
    })
}

pub(crate) async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut ticks = tokio::time::interval(Duration::from_millis(
        state.config.quota_observation_interval_millis.into(),
    ));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! { biased;
            _ = crate::worker::wait_for_shutdown(&mut shutdown) => return,
            _ = ticks.tick() => {}
        }
        tokio::select! { biased;
            _ = crate::worker::wait_for_shutdown(&mut shutdown) => return,
            result = refresh_batch(&state) => if let Err(error) = result {
                tracing::warn!(%error, "quota observation batch failed");
            }
        }
    }
}

async fn refresh_batch(state: &AppState) -> Result<(), AppError> {
    let pinned = state.clone().pin_application_plugins().await?;
    let plugins = pinned.plugins.quota_observation_plugin_ids();
    if plugins.is_empty() {
        return Ok(());
    }
    let targets = state
        .db
        .quota_observation_targets(
            &plugins,
            crate::db::unix_millis(),
            state.config.quota_observation_batch_limit.into(),
        )
        .await?;
    let results = stream::iter(targets)
        .map(|target| async move {
            let lease = Uuid::now_v7();
            let now = crate::db::unix_millis();
            let timeout_ms = i64::from(state.config.quota_observation_timeout_millis);
            if !state
                .db
                .claim_quota_observation(&target, lease, now, now + timeout_ms + 5_000)
                .await?
            {
                return Ok(());
            }
            let observation =
                tokio::time::timeout(Duration::from_millis(timeout_ms as u64), async {
                    let (account, credential) = state
                        .db
                        .upstream_account_with_credential(
                            target.account_id,
                            state.config.key_pepper.as_bytes(),
                        )
                        .await
                        .ok()?;
                    if account.tenant_id != target.tenant_id
                        || account.credential_generation != target.generation
                        || account.updated_at != target.config_revision
                        || account.status != "active"
                    {
                        return None;
                    }
                    let snapshot = state
                        .upstream_quota
                        .read(state, &account, &credential, &target.tenant_external_id)
                        .await;
                    project(&account, &snapshot, crate::db::unix_millis())
                })
                .await
                .ok()
                .flatten();
            state
                .db
                .finish_quota_observation(
                    &target,
                    lease,
                    observation.as_ref(),
                    crate::db::unix_millis()
                        + i64::from(state.config.quota_observation_interval_millis),
                )
                .await
        })
        .buffer_unordered(4)
        .collect::<Vec<Result<(), AppError>>>()
        .await;
    for result in results {
        result?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projection_preserves_unknowns_and_rejects_stale_failed_or_reset_windows() {
        let account: crate::provider::UpstreamAccountView = serde_json::from_value(json!({
            "id":Uuid::from_u128(1),"tenant_id":Uuid::from_u128(2),"name":"fixture","driver":"kimi-oauth",
            "auth_kind":"oauth","connection_method":"native_oauth","credential_generation":2,"status":"active",
            "config":{},"can_refresh":true,"can_rotate":false,"can_reauthorize":true,"route_count":0,"created_at":0,"updated_at":10
        })).unwrap();
        let mut snapshot = super::super::QuotaSnapshot::empty(&account, "tenant", None);
        snapshot.observed_at = Some(100);
        snapshot.stale_after = Some(200);
        snapshot.freshness = "fresh";
        snapshot.windows.push(super::super::QuotaWindow {
            id: "weekly".into(),
            label: "Weekly".into(),
            used_percent: Some(25.0),
            used: None,
            remaining: None,
            limit: None,
            unit: None,
            reset_at: Some(180),
            period_seconds: Some(604800),
            source: "fixture",
            reset_is_estimated: false,
            allowed: None,
            limit_reached: None,
        });
        let value = project(&account, &snapshot, 150).unwrap();
        assert_eq!(value.valid_until, 180);
        assert_eq!(value.windows[0].remaining_fraction, Some(0.75));
        assert_eq!(value.windows[0].exhausted, Some(false));
        snapshot.windows[0].used_percent = None;
        let value = project(&account, &snapshot, 150).unwrap();
        assert_eq!(value.windows[0].remaining_fraction, None);
        assert_eq!(value.windows[0].exhausted, None);
        assert!(project(&account, &snapshot, 180).is_none());
        snapshot.error_code = Some("quota_transport_failed");
        assert!(project(&account, &snapshot, 150).is_none());
        snapshot.error_code = None;
        snapshot.stale = true;
        assert!(project(&account, &snapshot, 150).is_none());
    }

    #[tokio::test]
    async fn observation_worker_stops_before_sampling_when_shutdown_is_already_set() {
        let directory = tempfile::tempdir().unwrap();
        let config = crate::config::Config::for_test(format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("quota.db").display()
        ));
        let state = AppState::initialize(config).await.unwrap();
        let (_sender, receiver) = tokio::sync::watch::channel(true);
        tokio::time::timeout(Duration::from_secs(1), run(state, receiver))
            .await
            .unwrap();
    }
}

pub(crate) async fn routing_observations(
    state: &AppState,
    tenant_id: Uuid,
    candidates: &[AuthorizedUpstreamCandidate],
    now_ms: i64,
) -> Result<BTreeMap<(Uuid, i64), RoutingQuotaObservation>, AppError> {
    state
        .db
        .routing_quota_observations(tenant_id, candidates, now_ms)
        .await
}
