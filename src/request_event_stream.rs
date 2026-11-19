use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Default)]
pub(crate) struct RequestEventStreams {
    active: Arc<AtomicUsize>,
}

pub(crate) struct RequestEventStreamActivity {
    active: Arc<AtomicUsize>,
}

impl RequestEventStreams {
    pub(crate) fn start(&self) -> RequestEventStreamActivity {
        self.active.fetch_add(1, Ordering::Relaxed);
        RequestEventStreamActivity {
            active: self.active.clone(),
        }
    }

    pub(crate) fn active_count(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }
}

impl Drop for RequestEventStreamActivity {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_activity_counts_every_owner_and_releases_on_drop() {
        let streams = RequestEventStreams::default();
        let owners = (0..40).map(|_| streams.start()).collect::<Vec<_>>();
        assert_eq!(streams.active_count(), 40);
        let shared = streams.clone();
        drop(owners);
        assert_eq!(shared.active_count(), 0);
    }
}
