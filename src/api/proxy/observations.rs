use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use tokio::sync::{Semaphore, oneshot};

pub(crate) mod sessions;

pub(crate) struct Observations {
    pub(crate) sessions: sessions::SessionCache,
    health: Arc<Semaphore>,
    rejected: AtomicU64,
}

impl Default for Observations {
    fn default() -> Self {
        Self {
            sessions: sessions::SessionCache::default(),
            health: Arc::new(Semaphore::new(16)),
            rejected: AtomicU64::new(0),
        }
    }
}

impl Observations {
    pub(crate) fn submit_health(
        &self,
        operation: impl std::future::Future<Output = bool> + Send + 'static,
    ) -> Option<oneshot::Receiver<bool>> {
        let Ok(permit) = self.health.clone().try_acquire_owned() else {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                stage = "delivery_health",
                outcome = "capacity",
                "early recovery deferred to terminal health owner"
            );
            return None;
        };
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = sender.send(operation.await);
        });
        Some(receiver)
    }

    pub(crate) fn render(&self) -> String {
        format!(
            "# TYPE memeloop_token_center_delivery_health_jobs gauge\nmemeloop_token_center_delivery_health_jobs {}\n# TYPE memeloop_token_center_delivery_health_rejected_total counter\nmemeloop_token_center_delivery_health_rejected_total {}\n{}",
            16 - self.health.available_permits(),
            self.rejected.load(Ordering::Relaxed),
            self.sessions.render(),
        )
    }
}
