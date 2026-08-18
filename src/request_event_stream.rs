use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

pub(crate) const GLOBAL_REQUEST_EVENT_STREAMS: usize = 32;
pub(crate) const REQUEST_EVENT_STREAMS_PER_SERVICE: usize = 4;

#[derive(Clone)]
pub(crate) struct RequestEventStreamLimiter {
    global: Arc<Semaphore>,
    per_service: Arc<Mutex<HashMap<Option<Uuid>, Weak<Semaphore>>>>,
}

pub(crate) struct RequestEventStreamPermit {
    _global: OwnedSemaphorePermit,
    _service: OwnedSemaphorePermit,
}

impl Default for RequestEventStreamLimiter {
    fn default() -> Self {
        Self {
            global: Arc::new(Semaphore::new(GLOBAL_REQUEST_EVENT_STREAMS)),
            per_service: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl RequestEventStreamLimiter {
    pub(crate) fn try_acquire(&self, service_id: Option<Uuid>) -> Option<RequestEventStreamPermit> {
        let global = self.global.clone().try_acquire_owned().ok()?;
        let service = {
            let mut limits = self
                .per_service
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            limits.retain(|_, limit| limit.strong_count() > 0);
            if let Some(limit) = limits.get(&service_id).and_then(Weak::upgrade) {
                limit
            } else {
                let limit = Arc::new(Semaphore::new(REQUEST_EVENT_STREAMS_PER_SERVICE));
                limits.insert(service_id, Arc::downgrade(&limit));
                limit
            }
        }
        .try_acquire_owned()
        .ok()?;
        Some(RequestEventStreamPermit {
            _global: global,
            _service: service,
        })
    }

    #[cfg(test)]
    pub(crate) fn global_available_permits(&self) -> usize {
        self.global.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_enforces_both_service_and_global_caps() {
        let limiter = RequestEventStreamLimiter::default();
        let service = Uuid::now_v7();
        let mut permits = (0..REQUEST_EVENT_STREAMS_PER_SERVICE)
            .map(|_| limiter.try_acquire(Some(service)).expect("service permit"))
            .collect::<Vec<_>>();
        assert!(limiter.try_acquire(Some(service)).is_none());

        for index in 1..=(GLOBAL_REQUEST_EVENT_STREAMS - REQUEST_EVENT_STREAMS_PER_SERVICE) {
            permits.push(
                limiter
                    .try_acquire(Some(Uuid::from_u128(index as u128)))
                    .expect("global permit"),
            );
        }
        assert_eq!(limiter.global_available_permits(), 0);
        assert!(limiter.try_acquire(Some(Uuid::now_v7())).is_none());
        drop(permits);
        assert_eq!(
            limiter.global_available_permits(),
            GLOBAL_REQUEST_EVENT_STREAMS
        );
    }
}
