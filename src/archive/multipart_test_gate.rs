//! Exact-location unit fixture; never compiled into a running service.
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};
use tokio::sync::oneshot;
type Gate = (oneshot::Sender<()>, oneshot::Receiver<()>);
static GATES: LazyLock<Mutex<HashMap<String, Gate>>> = LazyLock::new(Default::default);
pub(crate) struct Registration(String);
impl Drop for Registration {
    fn drop(&mut self) {
        GATES.lock().unwrap().remove(&self.0);
    }
}
pub(crate) fn install(
    location: &str,
) -> (oneshot::Receiver<()>, oneshot::Sender<()>, Registration) {
    let (entered, observed) = oneshot::channel();
    let (release, wait) = oneshot::channel();
    assert!(
        GATES
            .lock()
            .unwrap()
            .insert(location.to_owned(), (entered, wait))
            .is_none()
    );
    (observed, release, Registration(location.to_owned()))
}
pub(super) async fn wait(location: &str) {
    let gate = GATES.lock().unwrap().remove(location);
    if let Some((entered, wait)) = gate {
        let _ = entered.send(());
        let _ = wait.await;
    }
}
