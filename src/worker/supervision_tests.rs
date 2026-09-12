use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use super::*;

struct OnDrop(Arc<AtomicUsize>);

#[tokio::test(start_paused = true)]
async fn generation_shutdown_drops_inflight_attempt_without_failure_settlement() {
    let (stop, shutdown) = watch::channel(false);
    let (dispatched, dispatch_observed) = tokio::sync::oneshot::channel();
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = OnDrop(dropped.clone());
    let task = tokio::spawn(crate::generation::finish_attempt_until_shutdown(
        async move {
            let _guard = guard;
            dispatched.send(()).unwrap();
            std::future::pending::<()>().await;
            // This stands for process_one's failure/retry/terminal settlement
            // continuation. Shutdown must drop it, never manufacture an error
            // that enters that continuation.
            panic!("shutdown must not invoke failure settlement or retry");
        },
        shutdown,
    ));
    dispatch_observed.await.unwrap();
    stop.send(true).unwrap();
    assert!(!task.await.unwrap().unwrap());
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn stalled_provider_roles_do_not_block_other_lanes_or_spawn_more_work() {
    let (external_stop, external_shutdown) = watch::channel(false);
    let (role_stop, role_shutdown) = watch::channel(false);
    let mut roles = JoinSet::new();
    let entered = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let (progress, mut observed) = tokio::sync::mpsc::unbounded_channel();
    for name in ["generation", "oauth"] {
        let entered = entered.clone();
        let dropped = dropped.clone();
        let shutdown = role_shutdown.clone();
        roles.spawn(async move {
            run_periodic(Duration::from_secs(1), shutdown, || async {
                entered.fetch_add(1, Ordering::SeqCst);
                let _drop = OnDrop(dropped.clone());
                std::future::pending::<()>().await;
            })
            .await;
            name
        });
    }
    for name in ["projection", "maintenance", "orphan_reaper"] {
        let progress = progress.clone();
        let shutdown = role_shutdown.clone();
        roles.spawn(async move {
            run_periodic(Duration::from_secs(1), shutdown, || async {
                progress.send(name).unwrap();
            })
            .await;
            name
        });
    }
    let supervisor = tokio::spawn(supervise_roles(
        roles,
        role_stop,
        external_shutdown,
        Duration::from_secs(30),
    ));
    for _ in 0..3 {
        observed.recv().await.unwrap();
    }
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut second_tick = Vec::new();
    for _ in 0..3 {
        second_tick.push(observed.recv().await.unwrap());
    }
    second_tick.sort_unstable();
    assert_eq!(second_tick, ["maintenance", "orphan_reaper", "projection"]);
    assert_eq!(
        entered.load(Ordering::SeqCst),
        2,
        "one in-flight operation per provider role"
    );
    external_stop.send(true).unwrap();
    let mut stopped = role_shutdown.clone();
    wait_for_shutdown(&mut stopped).await;
    let deadline_start = tokio::time::Instant::now();
    supervisor.await.unwrap();
    assert_eq!(deadline_start.elapsed(), Duration::from_secs(30));
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        2,
        "all aborted operations joined before return"
    );
}

#[tokio::test(start_paused = true)]
async fn unexpected_role_exit_stops_and_joins_siblings() {
    let (_external_stop, external_shutdown) = watch::channel(false);
    let (role_stop, mut role_shutdown) = watch::channel(false);
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = OnDrop(dropped.clone());
    let mut roles = JoinSet::new();
    roles.spawn(async { "exited" });
    roles.spawn(async move {
        let _guard = guard;
        wait_for_shutdown(&mut role_shutdown).await;
        "sibling"
    });
    supervise_roles(roles, role_stop, external_shutdown, Duration::from_secs(30)).await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn closed_sender_stops_without_starting_another_operation() {
    let (stop, shutdown) = watch::channel(false);
    drop(stop);
    run_periodic(Duration::from_secs(1), shutdown, || async {
        panic!("closed shutdown channel must win over an immediately ready tick");
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_supervisor_aborts_owned_roles() {
    let (_external_stop, external_shutdown) = watch::channel(false);
    let (role_stop, _role_shutdown) = watch::channel(false);
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = OnDrop(dropped.clone());
    let (started, ready) = tokio::sync::oneshot::channel();
    let mut roles = JoinSet::new();
    roles.spawn(async move {
        let _guard = guard;
        started.send(()).unwrap();
        std::future::pending::<()>().await;
        "blocked"
    });
    let supervisor = tokio::spawn(supervise_roles(
        roles,
        role_stop,
        external_shutdown,
        Duration::from_secs(30),
    ));
    ready.await.unwrap();
    supervisor.abort();
    assert!(supervisor.await.unwrap_err().is_cancelled());
    tokio::task::yield_now().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}
