use std::time::Duration;

use crate::AppState;

const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

pub async fn run(state: AppState) {
    let mut interval = tokio::time::interval(MAINTENANCE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if let Err(error) = state.db.maintain_partitions().await {
            tracing::error!(%error, "worker failed to maintain PostgreSQL partitions");
        }
    }
}
