use std::sync::{Arc, LazyLock, Mutex, Weak};

use tokio::sync::Notify;
use uuid::Uuid;

pub(crate) struct Gate {
    pub(crate) entered: Notify,
    pub(crate) release: Notify,
}

static GATES: LazyLock<Mutex<std::collections::HashMap<Uuid, Weak<Gate>>>> =
    LazyLock::new(Default::default);

pub(crate) fn install(request_id: Uuid) -> Arc<Gate> {
    let gate = Arc::new(Gate {
        entered: Notify::new(),
        release: Notify::new(),
    });
    let mut gates = GATES.lock().unwrap();
    gates.retain(|_, gate| gate.strong_count() > 0);
    gates.insert(request_id, Arc::downgrade(&gate));
    gate
}

pub(super) async fn wait(request_id: Uuid) {
    let gate = GATES
        .lock()
        .unwrap()
        .remove(&request_id)
        .and_then(|gate| gate.upgrade());
    if let Some(gate) = gate {
        gate.entered.notify_one();
        gate.release.notified().await;
    }
}
