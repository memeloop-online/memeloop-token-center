use std::time::Duration;

use tokio::{sync::watch, task::JoinHandle};
use uuid::Uuid;

use crate::{
    AppState, api, archive_reaper::ArchiveReaper, archive_staging::ArchiveStagingLeaseOwner,
    generation, metrics::BackgroundProjectionKind,
};

const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const ORPHANED_RESERVATION_REAPER_INTERVAL: Duration = Duration::from_secs(5 * 60);
const GENERATION_INTERVAL: Duration = Duration::from_millis(500);
const PROJECTION_INTERVAL: Duration = Duration::from_secs(1);
const OAUTH_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const OAUTH_REFRESH_AHEAD_MILLIS: i64 = 5 * 60 * 1_000;
const PROJECTION_BATCH_LIMIT: i64 = 32;

pub async fn run(state: AppState) {
    let (shutdown_sender, shutdown) = watch::channel(false);
    run_until_shutdown(state, shutdown).await;
    drop(shutdown_sender);
}

/// Runs the worker roles with a shutdown signal that can be shared by the
/// server supervisor. The archive reaper is its own Tokio task, so a slow
/// generation provider never delays cleanup claims.
pub async fn run_until_shutdown(state: AppState, mut shutdown: watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    let worker_id = format!("worker-{}", Uuid::now_v7());
    let projection_owner = Uuid::now_v7();
    let reaper_owner = ArchiveStagingLeaseOwner::new(format!("archive-reaper-{}", Uuid::now_v7()))
        .expect("reaper owner is canonical safe ASCII");
    let reaper = ArchiveReaper::new(state.db.clone(), state.archive.clone(), reaper_owner);
    let reaper_shutdown = shutdown.clone();
    let mut reaper_task = AbortTaskOnDrop::new(tokio::spawn(async move {
        reaper.run(reaper_shutdown).await;
    }));
    let mut maintenance = tokio::time::interval(MAINTENANCE_INTERVAL);
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut orphaned_reservation_reaper =
        tokio::time::interval(ORPHANED_RESERVATION_REAPER_INTERVAL);
    orphaned_reservation_reaper.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut generations = tokio::time::interval(GENERATION_INTERVAL);
    generations.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut projections = tokio::time::interval(PROJECTION_INTERVAL);
    projections.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut oauth_refresh = tokio::time::interval(OAUTH_REFRESH_INTERVAL);
    oauth_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = maintenance.tick() => {
                if let Err(error) = state.db.maintain_partitions().await {
                    tracing::error!(%error, "worker failed to maintain PostgreSQL partitions");
                }
                match state.db.expire_key_provisioning_responses(1_000).await {
                    Ok(expired) if expired > 0 => {
                        tracing::info!(expired, "worker expired encrypted key provisioning responses");
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(%error, "worker failed to expire key provisioning responses");
                    }
                }
                match state.db.delete_expired_rate_windows(100_000).await {
                    Ok(deleted) if deleted > 0 => {
                        tracing::info!(deleted, "worker deleted expired rate limit windows");
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(%error, "worker failed to delete expired rate limit windows");
                    }
                }
                match state.db.delete_expired_budget_rollups(100_000).await {
                    Ok(deleted) if deleted > 0 => {
                        tracing::info!(deleted, "worker deleted expired budget rollup detail");
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(%error, "worker failed to delete expired budget rollup detail");
                    }
                }
            }
            _ = orphaned_reservation_reaper.tick() => {
                match state.db.release_orphaned_reservations(100).await {
                    Ok(released) if released > 0 => {
                        tracing::warn!(released, "worker released orphaned usage reservations");
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(%error, "worker failed to release orphaned usage reservations");
                    }
                }
            }
            _ = generations.tick() => {
                match state.db.expire_preparing_generation_jobs(100).await {
                    Ok(expired) if expired > 0 => {
                        tracing::warn!(expired, "worker refunded expired generation archive preparations");
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(%error, "worker failed to expire generation archive preparations");
                    }
                }
                if let Err(error) = generation::process_one(&state, &worker_id).await {
                    tracing::error!(%error, "worker failed to claim or update a generation job");
                }
            }
            _ = projections.tick() => {
                process_metered_usage_projection_batch(&state, projection_owner).await;
                process_conversation_projection_batch(&state, projection_owner).await;
            }
            _ = oauth_refresh.tick() => {
                let refresh_before = crate::db::unix_millis()
                    .saturating_add(OAUTH_REFRESH_AHEAD_MILLIS);
                match state.db.list_managed_oauth_refresh_candidates(refresh_before, 20).await {
                    Ok(accounts) => {
                        for (account_id, generation) in accounts {
                            let idempotency_key = format!(
                                "oauth-worker-{}-generation-{}",
                                account_id,
                                generation
                            );
                            if let Err(error) = api::refresh_managed_upstream_oauth(
                                &state,
                                account_id,
                                &idempotency_key,
                            )
                            .await
                            {
                                tracing::warn!(%error, %account_id, "worker failed to refresh managed OAuth credential");
                            }
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "worker failed to list expiring managed OAuth credentials");
                    }
                }
            }
        }
    }

    if reaper_task.join().await.is_err() {
        tracing::error!(
            error_code = "reaper_task_failed",
            "archive staging reaper task failed"
        );
    }
}

async fn process_metered_usage_projection_batch(state: &AppState, lease_owner: Uuid) {
    let tasks = match state
        .db
        .claim_metered_usage_projection_tasks(lease_owner, PROJECTION_BATCH_LIMIT)
        .await
    {
        Ok(tasks) => tasks,
        Err(error) => {
            state
                .metrics
                .observe_background_projection(BackgroundProjectionKind::MeteredUsage, false);
            tracing::error!(%error, "worker failed to claim metered usage projection tasks");
            return;
        }
    };
    for task in tasks {
        match state
            .db
            .project_claimed_metered_usage_projection_task(lease_owner, task.reservation_id)
            .await
        {
            Ok(true) => state
                .metrics
                .observe_background_projection(BackgroundProjectionKind::MeteredUsage, true),
            Ok(false) => {}
            Err(error) => {
                state
                    .metrics
                    .observe_background_projection(BackgroundProjectionKind::MeteredUsage, false);
                tracing::error!(
                    %error,
                    reservation_id = %task.reservation_id,
                    "worker failed to project metered usage task"
                );
            }
        }
    }
}

async fn process_conversation_projection_batch(state: &AppState, lease_owner: Uuid) {
    let tasks = match state
        .db
        .claim_conversation_projection_tasks(lease_owner, PROJECTION_BATCH_LIMIT)
        .await
    {
        Ok(tasks) => tasks,
        Err(error) => {
            state
                .metrics
                .observe_background_projection(BackgroundProjectionKind::Conversation, false);
            tracing::error!(%error, "worker failed to claim conversation projection tasks");
            return;
        }
    };
    for task in tasks {
        match state
            .db
            .project_claimed_conversation_projection_task(lease_owner, task.request_id)
            .await
        {
            Ok(true) => state
                .metrics
                .observe_background_projection(BackgroundProjectionKind::Conversation, true),
            Ok(false) => {}
            Err(error) => {
                state
                    .metrics
                    .observe_background_projection(BackgroundProjectionKind::Conversation, false);
                tracing::error!(
                    %error,
                    request_id = %task.request_id,
                    "worker failed to project conversation task"
                );
            }
        }
    }
}

struct AbortTaskOnDrop {
    handle: Option<JoinHandle<()>>,
}

impl AbortTaskOnDrop {
    fn new(handle: JoinHandle<()>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    async fn join(&mut self) -> Result<(), tokio::task::JoinError> {
        self.handle
            .take()
            .expect("reaper task is joined once")
            .await
    }
}

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        config::Config,
        db::{CreateKeyInput, FinishProxyRequest, StartProxyRequest},
        metrics::RuntimeMetrics,
        model::{EnforcementMode, KeyPolicy, TokenUsage},
    };
    use rust_decimal::Decimal;

    use super::*;

    #[tokio::test]
    async fn projection_tick_drains_metered_usage_and_records_a_fixed_metric() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory
                .path()
                .join("worker-metered-projection.db")
                .display()
        );
        let state = AppState::initialize(Config::for_test(database_url))
            .await
            .unwrap();
        let pepper = state.config.key_pepper.as_bytes();
        let issued = state
            .db
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "worker-metered-projection".to_owned(),
                    principal_external_id: "member".to_owned(),
                    alias: "worker-metered-projection".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy {
                        enforcement_mode: EnforcementMode::MeteredUnlimited,
                        ..KeyPolicy::default()
                    },
                    initial_balance: Decimal::ONE,
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let key = state
            .db
            .authenticate_key(&issued.key, pepper)
            .await
            .unwrap();
        let price = state
            .db
            .upsert_model_price(
                "worker-metered-projection",
                "USD",
                Decimal::ONE,
                Decimal::ONE,
            )
            .await
            .unwrap();
        let request_id = Uuid::now_v7();
        let reservation = state
            .db
            .start_proxy_request(StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 1,
                output_token_ceiling: 1,
                protocol: "openai",
                model: "worker-metered-projection",
                request_object: "gap://worker-metered-projection/request",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        state
            .db
            .finish_proxy_request(FinishProxyRequest {
                request_id,
                tenant_id: key.tenant_id,
                reservation: &reservation,
                input_token_ceiling: 1,
                output_token_ceiling: 1,
                requested_service_tier: None,
                status_code: 200,
                duration_ms: 1,
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..TokenUsage::default()
                },
                charge_contract_ceiling: false,
                error_code: None,
                response_object: "gap://worker-metered-projection/response",
                conversation: None,
            })
            .await
            .unwrap();

        process_metered_usage_projection_batch(&state, Uuid::now_v7()).await;

        assert!(
            state
                .db
                .claim_metered_usage_projection_tasks(Uuid::now_v7(), PROJECTION_BATCH_LIMIT)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(state.metrics.render(&RuntimeMetrics::default()).contains(
            "memeloop_token_center_background_projections_total{queue=\"metered_usage\",outcome=\"completed\"} 1"
        ));
    }
}
