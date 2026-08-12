use std::time::Duration;

use uuid::Uuid;

use crate::{AppState, generation};

const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const GENERATION_INTERVAL: Duration = Duration::from_millis(500);

pub async fn run(state: AppState) {
    let worker_id = format!("worker-{}", Uuid::now_v7());
    let mut maintenance = tokio::time::interval(MAINTENANCE_INTERVAL);
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut generations = tokio::time::interval(GENERATION_INTERVAL);
    generations.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = maintenance.tick() => {
                if let Err(error) = state.db.maintain_partitions().await {
                    tracing::error!(%error, "worker failed to maintain PostgreSQL partitions");
                }
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
                if let Err(error) = generation::process_one(&state, &worker_id).await {
                    tracing::error!(%error, "worker failed to claim or update a generation job");
                }
            }
        }
    }
}
