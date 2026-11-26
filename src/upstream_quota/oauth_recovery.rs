//! Bounded, coalesced rejection delivery and read-only post-renewal verification.
//! Neither role dispatches OAuth refresh; only the existing refresh worker does.
use crate::{AppState, error::AppError};
use futures_util::{StreamExt, stream};
use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;

const CAPACITY: usize = 1024;
const BATCH: usize = 32;
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Reuse the already-owned routing persistence pool for the whole automatic
/// OAuth operation. Cloning AppState retains configuration, authorization and
/// provider contracts; only its database handle changes. No pool is created.
pub(crate) fn background_state(state: &AppState) -> AppState {
    let mut background = state.clone();
    background.db = state.routing_persistence_db.clone();
    background
}

#[derive(Default)]
pub(crate) struct Rejections {
    pending: Mutex<HashMap<Uuid, i64>>,
    dropped: AtomicU64,
}

impl Rejections {
    /// Never wait for SQL, a channel slot, or another producer. A later natural
    /// request can retry dropped evidence after pressure subsides.
    pub(crate) fn submit(&self, account: Uuid, generation: i64) {
        if let Ok(mut pending) = self.pending.try_lock() {
            if let Some(queued) = pending.get_mut(&account) {
                *queued = (*queued).max(generation);
                return;
            }
            if pending.len() < CAPACITY {
                pending.insert(account, generation);
                return;
            }
        }
        let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        if dropped.is_power_of_two() {
            tracing::warn!(
                operation = "oauth_rejection_backpressure",
                dropped,
                capacity = CAPACITY,
                "OAuth evidence queue full or busy; later natural requests may retry"
            );
        }
    }

    pub(crate) async fn flush(&self, state: &AppState) {
        let batch = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .take(BATCH)
            .map(|(&id, &generation)| (id, generation))
            .collect::<Vec<_>>();
        stream::iter(batch).for_each_concurrent(4, |(id, generation)| async move {
            match tokio::time::timeout(WRITE_TIMEOUT,
                state.routing_persistence_db.observe_rejected_oauth_generation(id, generation)).await {
                Ok(Ok(())) => {
                    let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                    if pending.get(&id) == Some(&generation) { pending.remove(&id); }
                }
                result => {
                    let category = match result { Ok(Err(ref error)) => error.diagnostic_category(), _ => "timeout" };
                    tracing::warn!(operation = "oauth_rejection_persist", upstream_account_id = %id,
                        credential_generation = generation, error_category = category,
                        "OAuth evidence retained for a later background persistence attempt");
                }
            }
        }).await;
    }
}

pub(crate) async fn verify_batch(state: &AppState) -> Result<(), AppError> {
    let background = background_state(state);
    let state = &background;
    let candidates = state
        .db
        .list_rejected_oauth_verifications(crate::db::unix_millis(), 20)
        .await?;
    stream::iter(candidates)
        .for_each_concurrent(2, |(id, generation)| async move {
            if let Err(error) = verify_one(state, id, generation).await {
                tracing::warn!(operation = "oauth_verification", upstream_account_id = %id,
                credential_generation = generation, error_category = error.diagnostic_category(),
                "OAuth quota verification remains pending for background recovery");
            }
        })
        .await;
    Ok(())
}

async fn verify_one(state: &AppState, id: Uuid, rejected_generation: i64) -> Result<(), AppError> {
    let started = crate::db::unix_millis();
    if !state
        .db
        .claim_rejected_oauth_verification(id, rejected_generation, started)
        .await?
    {
        return Ok(());
    }
    // The lease survives cancellation/process loss. Only read-only verification
    // is reclaimed; no OAuth exchange or inference can be replayed here.
    let result = tokio::time::timeout(Duration::from_secs(95), async {
        let (account, credential) = state
            .db
            .upstream_account_with_credential(id, state.config.key_pepper.as_bytes())
            .await?;
        if account.status != "active" || account.credential_generation <= rejected_generation {
            return Ok(None);
        }
        let Some(tenant) = account.tenant_external_id.as_deref() else {
            return Ok(None);
        };
        let snapshot = state
            .upstream_quota
            .read_fresh(
                state,
                &account,
                &credential,
                tenant,
                super::QuotaReadTrigger::PostRenewalVerification,
            )
            .await;
        Ok::<_, AppError>(snapshot.error_code)
    })
    .await;
    match result {
        Ok(Ok(error)) => {
            state
                .db
                .finish_rejected_oauth_verification(id, rejected_generation, started, error)
                .await?;
            if let Some(error) = error {
                tracing::warn!(operation = "oauth_verification", upstream_account_id = %id,
                    error_code = error, "Post-renewal quota remains uncertain; independent reads may retry");
            }
        }
        Ok(Err(error)) => return Err(error),
        Err(_) => tracing::warn!(operation = "oauth_verification", upstream_account_id = %id,
            error_code = "timeout", "Read-only verification lease will be retried after expiry"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_is_bounded_coalesces_generations_and_reports_pressure() {
        let queue = Rejections::default();
        let account = Uuid::now_v7();
        for _ in 0..10_000 {
            queue.submit(account, 1);
        }
        queue.submit(account, 3);
        queue.submit(account, 2);
        assert_eq!(queue.pending.lock().unwrap().get(&account), Some(&3));
        for _ in 1..CAPACITY {
            queue.submit(Uuid::now_v7(), 1);
        }
        queue.submit(Uuid::now_v7(), 1);
        assert_eq!(queue.pending.lock().unwrap().len(), CAPACITY);
        assert_eq!(queue.dropped.load(Ordering::Relaxed), 1);
        // A busy consumer cannot make a producer wait for its mutex either.
        let guard = queue.pending.lock().unwrap();
        queue.submit(account, 4);
        assert_eq!(queue.dropped.load(Ordering::Relaxed), 2);
        drop(guard);
        queue.pending.lock().unwrap().remove(&account);
        queue.submit(account, 4);
        assert_eq!(queue.pending.lock().unwrap().get(&account), Some(&4));
    }
}
