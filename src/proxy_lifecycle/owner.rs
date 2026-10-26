use super::*;
use crate::db::{ProxyRequestOwner, REQUEST_OWNER_HEARTBEAT_MILLIS, REQUEST_OWNER_LEASE_MILLIS};
use tokio::sync::watch;

pub(super) struct RequestOwnerLease {
    task: tokio::task::JoinHandle<()>,
}

impl RequestOwnerLease {
    pub(super) async fn acquire(
        database: Database,
        identity: ProxyRequestOwner,
        lost: watch::Sender<bool>,
    ) -> Result<Self, AppError> {
        let started = tokio::time::Instant::now();
        let acquired = tokio::time::timeout(
            Duration::from_millis(REQUEST_OWNER_HEARTBEAT_MILLIS as u64),
            database.acquire_proxy_request_owner(&identity),
        )
        .await;
        if !matches!(acquired, Ok(Ok(true))) {
            lost.send_replace(true);
            return Err(AppError::internal("owner_acquire_1"));
        }
        let task = tokio::spawn(async move {
            renew_owner_until_lost(started, lost, || async {
                if database.renew_proxy_request_owner(&identity).await? {
                    Ok(RenewalStatus::Active)
                } else if database.proxy_request_owner_finished(&identity).await? {
                    Ok(RenewalStatus::Finished)
                } else {
                    Ok(RenewalStatus::Lost)
                }
            })
            .await;
        });
        Ok(Self { task })
    }
}

impl Drop for RequestOwnerLease {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum RenewalStatus {
    Active,
    Finished,
    Lost,
}

async fn renew_owner_until_lost<F, Renewal>(
    started: tokio::time::Instant,
    lost: watch::Sender<bool>,
    mut renew: F,
) where
    F: FnMut() -> Renewal,
    Renewal: std::future::Future<Output = Result<RenewalStatus, AppError>>,
{
    let lifetime = Duration::from_millis(REQUEST_OWNER_LEASE_MILLIS as u64);
    let interval = Duration::from_millis(REQUEST_OWNER_HEARTBEAT_MILLIS as u64);
    let mut confirmed = started;
    loop {
        let deadline = confirmed + lifetime;
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => break,
            _ = tokio::time::sleep(interval) => {}
        }
        let renewal_started = tokio::time::Instant::now();
        match tokio::time::timeout_at(deadline.min(renewal_started + interval), renew()).await {
            Ok(Ok(RenewalStatus::Active)) => confirmed = renewal_started,
            Ok(Ok(RenewalStatus::Finished)) => return,
            Ok(Ok(RenewalStatus::Lost)) => break,
            Ok(Err(_)) | Err(_) => {}
        }
    }
    lost.send_replace(true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn live_owner_renews_beyond_thirty_minutes_without_a_total_deadline() {
        let (lost, receiver) = watch::channel(false);
        let renewals = Arc::new(AtomicUsize::new(0));
        let observed = renewals.clone();
        let task = tokio::spawn(renew_owner_until_lost(
            tokio::time::Instant::now(),
            lost,
            move || {
                observed.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(RenewalStatus::Active))
            },
        ));
        tokio::task::yield_now().await;
        for _ in 0..100 {
            tokio::time::advance(Duration::from_millis(REQUEST_OWNER_HEARTBEAT_MILLIS as u64))
                .await;
            tokio::task::yield_now().await;
            assert!(!*receiver.borrow());
        }
        assert!(renewals.load(Ordering::SeqCst) >= 100);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_cleanup_timeout_drops_the_owner_and_stops_renewal() {
        let (lost, _receiver) = watch::channel(false);
        let renewals = Arc::new(AtomicUsize::new(0));
        let observed = renewals.clone();
        let task = tokio::spawn(renew_owner_until_lost(
            tokio::time::Instant::now(),
            lost,
            move || {
                observed.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(RenewalStatus::Active))
            },
        ));
        let counter = Arc::new(ProxyLifecycleCounter::default());
        let mut permit = counter.start();
        permit.owner = Some(RequestOwnerLease { task });
        assert!(
            tokio::time::timeout(MAX_PROXY_FINALIZATION_LIFETIME, async move {
                let _permit = permit;
                std::future::pending::<()>().await;
            })
            .await
            .is_err()
        );
        assert_eq!(counter.active(), 0);
        let stopped = renewals.load(Ordering::SeqCst);
        assert_eq!(stopped, 1);
        tokio::time::advance(Duration::from_secs(1900)).await;
        tokio::task::yield_now().await;
        assert_eq!(renewals.load(Ordering::SeqCst), stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn unrenewable_owner_loses_authority_at_the_last_confirmed_lease() {
        let (lost, mut receiver) = watch::channel(false);
        let started = tokio::time::Instant::now();
        renew_owner_until_lost(started, lost, || {
            std::future::ready(Err(AppError::internal(
                "owner_unrenewable_owner_loses_authority_at_the_last_confirmed_le",
            )))
        })
        .await;
        assert!(*receiver.borrow_and_update());
        assert_eq!(
            started.elapsed(),
            Duration::from_millis(REQUEST_OWNER_LEASE_MILLIS as u64)
        );
    }
}
