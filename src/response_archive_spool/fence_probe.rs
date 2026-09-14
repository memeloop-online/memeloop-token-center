//! Fixture-scoped deterministic barrier/count for cancellation fence ownership.
use std::{
    collections::HashMap,
    sync::{
        Arc, LazyLock, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::oneshot;

pub(crate) struct Probe {
    calls: AtomicUsize,
    entered: Mutex<Option<oneshot::Sender<()>>>,
    released: Mutex<Option<oneshot::Receiver<()>>>,
}

impl Probe {
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

static PROBES: LazyLock<Mutex<HashMap<String, Weak<Probe>>>> = LazyLock::new(Default::default);

pub(crate) fn install(
    state: &crate::AppState,
) -> (Arc<Probe>, oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered, entering) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let probe = Arc::new(Probe {
        calls: AtomicUsize::new(0),
        entered: Mutex::new(Some(entered)),
        released: Mutex::new(Some(released)),
    });
    let mut probes = PROBES.lock().unwrap();
    probes.retain(|_, probe| probe.strong_count() > 0);
    assert!(
        probes
            .insert(state.config.database_url.clone(), Arc::downgrade(&probe))
            .is_none()
    );
    (probe, entering, release)
}

pub(super) async fn observe(state: &crate::AppState) {
    let probe = PROBES
        .lock()
        .unwrap()
        .get(&state.config.database_url)
        .and_then(Weak::upgrade);
    if let Some(probe) = probe {
        probe.calls.fetch_add(1, Ordering::SeqCst);
        let entered = probe.entered.lock().unwrap().take();
        let released = probe.released.lock().unwrap().take();
        if let Some(entered) = entered {
            let _ = entered.send(());
        }
        if let Some(released) = released {
            let _ = released.await;
        }
    }
}
