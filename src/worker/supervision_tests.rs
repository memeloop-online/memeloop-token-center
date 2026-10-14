use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use super::*;

struct OnDrop(Arc<AtomicUsize>);

#[tokio::test(start_paused = true)]
async fn early_oauth_stop_drains_current_exchange_without_stopping_http_cleanup_roles() {
    let (stop, shutdown) = watch::channel(false);
    let (early_stop, early_shutdown) = watch::channel(false);
    let (role_stop, mut role_shutdown) = watch::channel(false);
    let (oauth_stop, oauth_shutdown) = watch::channel(false);
    let mut observed_oauth_stop = oauth_shutdown.clone();
    let saved = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(AtomicUsize::new(0));
    let joined = Arc::new(AtomicUsize::new(0));
    let guard = OnDrop(joined.clone());
    let (started, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let mut refresh_roles = JoinSet::new();
    refresh_roles.spawn({
        let saved = saved.clone();
        let entered = entered.clone();
        async move {
            let _guard = guard;
            run_periodic(Duration::from_secs(60), oauth_shutdown, || async {
                entered.fetch_add(1, Ordering::SeqCst);
                started.send(()).unwrap();
                tokio::time::sleep(Duration::from_secs(120)).await;
                saved.fetch_add(1, Ordering::SeqCst);
            })
            .await;
            "oauth_refresh"
        }
    });
    let mut roles = JoinSet::new();
    let mut ordinary_shutdown = role_shutdown.clone();
    roles.spawn(async move {
        wait_for_shutdown(&mut ordinary_shutdown).await;
        "http_cleanup"
    });
    let supervisor = tokio::spawn(supervise_roles_with_early_oauth_shutdown(
        roles,
        OAuthRefreshRoles {
            roles: refresh_roles,
            stop: oauth_stop,
            shutdown: early_shutdown,
            grace: OAUTH_REFRESH_SHUTDOWN_GRACE,
        },
        role_stop,
        shutdown,
        SHUTDOWN_GRACE,
        BlockingTasks::new(),
    ));
    observed.recv().await.unwrap();
    early_stop.send(true).unwrap();
    wait_for_shutdown(&mut observed_oauth_stop).await;
    tokio::time::advance(Duration::from_secs(121)).await;
    tokio::task::yield_now().await;
    assert_eq!(saved.load(Ordering::SeqCst), 1);
    assert_eq!(entered.load(Ordering::SeqCst), 1);
    assert_eq!(joined.load(Ordering::SeqCst), 1);
    assert!(!*role_shutdown.borrow());
    assert!(!supervisor.is_finished());
    tokio::time::advance(Duration::from_secs(1139)).await;
    assert_eq!(entered.load(Ordering::SeqCst), 1);
    stop.send(true).unwrap();
    wait_for_shutdown(&mut role_shutdown).await;
    supervisor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn early_oauth_deadline_is_not_restarted_after_http_finishes() {
    let (stop, shutdown) = watch::channel(false);
    let (early_stop, early_shutdown) = watch::channel(false);
    let (role_stop, mut role_shutdown) = watch::channel(false);
    let (oauth_stop, mut oauth_shutdown) = watch::channel(false);
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = OnDrop(dropped.clone());
    let mut refresh_roles = JoinSet::new();
    refresh_roles.spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
        "oauth_refresh"
    });
    let mut roles = JoinSet::new();
    let mut ordinary_shutdown = role_shutdown.clone();
    roles.spawn(async move {
        wait_for_shutdown(&mut ordinary_shutdown).await;
        "http_cleanup"
    });
    let supervisor = tokio::spawn(supervise_roles_with_early_oauth_shutdown(
        roles,
        OAuthRefreshRoles {
            roles: refresh_roles,
            stop: oauth_stop,
            shutdown: early_shutdown,
            grace: OAUTH_REFRESH_SHUTDOWN_GRACE,
        },
        role_stop,
        shutdown,
        SHUTDOWN_GRACE,
        BlockingTasks::new(),
    ));
    early_stop.send(true).unwrap();
    wait_for_shutdown(&mut oauth_shutdown).await;
    let started = tokio::time::Instant::now();
    tokio::time::advance(Duration::from_secs(170)).await;
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    stop.send(true).unwrap();
    wait_for_shutdown(&mut role_shutdown).await;
    supervisor.await.unwrap();
    assert_eq!(started.elapsed(), OAUTH_REFRESH_SHUTDOWN_GRACE);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn oauth_refresh_drain_preserves_current_exchange_beyond_ordinary_grace() {
    let (stop, shutdown) = watch::channel(false);
    let (role_stop, role_shutdown) = watch::channel(false);
    let ordinary_dropped = Arc::new(AtomicUsize::new(0));
    let guard = OnDrop(ordinary_dropped.clone());
    let mut roles = JoinSet::new();
    roles.spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
        "ordinary"
    });
    let entered = Arc::new(AtomicUsize::new(0));
    let saved = Arc::new(AtomicUsize::new(0));
    let observed_saved = saved.clone();
    let observed_entered = entered.clone();
    let (started, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let refresh_shutdown = role_shutdown.clone();
    let mut refresh_roles = JoinSet::new();
    refresh_roles.spawn(async move {
        run_periodic(Duration::from_secs(60), refresh_shutdown, || async {
            entered.fetch_add(1, Ordering::SeqCst);
            started.send(()).unwrap();
            tokio::time::sleep(Duration::from_secs(120)).await;
            saved.fetch_add(1, Ordering::SeqCst);
        })
        .await;
        "oauth_refresh"
    });
    let supervisor = tokio::spawn(supervise_roles_with_oauth_refresh(
        roles,
        refresh_roles,
        role_stop,
        shutdown,
        SHUTDOWN_GRACE,
        OAUTH_REFRESH_SHUTDOWN_GRACE,
        BlockingTasks::new(),
    ));
    observed.recv().await.unwrap();
    stop.send(true).unwrap();
    let mut stopping = role_shutdown.clone();
    wait_for_shutdown(&mut stopping).await;
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::task::yield_now().await;
    assert_eq!(ordinary_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(observed_saved.load(Ordering::SeqCst), 0);
    assert!(!supervisor.is_finished());
    tokio::time::advance(Duration::from_secs(100)).await;
    supervisor.await.unwrap();
    assert_eq!(observed_saved.load(Ordering::SeqCst), 1);
    assert_eq!(observed_entered.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn oauth_refresh_drain_remains_bounded_and_joins_the_cancelled_operation() {
    let (stop, shutdown) = watch::channel(false);
    let (role_stop, mut role_shutdown) = watch::channel(false);
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = OnDrop(dropped.clone());
    let (started, observed) = tokio::sync::oneshot::channel();
    let mut refresh_roles = JoinSet::new();
    refresh_roles.spawn(async move {
        let _guard = guard;
        started.send(()).unwrap();
        std::future::pending::<()>().await;
        "oauth_refresh"
    });
    let supervisor = tokio::spawn(supervise_roles_with_oauth_refresh(
        JoinSet::new(),
        refresh_roles,
        role_stop,
        shutdown,
        SHUTDOWN_GRACE,
        OAUTH_REFRESH_SHUTDOWN_GRACE,
        BlockingTasks::new(),
    ));
    observed.await.unwrap();
    stop.send(true).unwrap();
    wait_for_shutdown(&mut role_shutdown).await;
    let before = tokio::time::Instant::now();
    supervisor.await.unwrap();
    assert_eq!(before.elapsed(), OAUTH_REFRESH_SHUTDOWN_GRACE);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unexpected_oauth_refresh_exit_stops_and_joins_ordinary_roles() {
    let (_stop, shutdown) = watch::channel(false);
    let (role_stop, mut role_shutdown) = watch::channel(false);
    let mut refresh_roles = JoinSet::new();
    refresh_roles.spawn(async { "oauth_refresh" });
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = OnDrop(dropped.clone());
    let mut roles = JoinSet::new();
    roles.spawn(async move {
        let _guard = guard;
        wait_for_shutdown(&mut role_shutdown).await;
        "ordinary"
    });
    let error = tokio::spawn(supervise_roles_with_oauth_refresh(
        roles,
        refresh_roles,
        role_stop,
        shutdown,
        SHUTDOWN_GRACE,
        OAUTH_REFRESH_SHUTDOWN_GRACE,
        BlockingTasks::new(),
    ))
    .await
    .unwrap_err();
    assert!(error.is_panic());
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn deadline_joins_started_blocking_calls_after_cancelling_their_async_role() {
    let (stop, shutdown) = watch::channel(false);
    let (role_stop, mut role_shutdown) = watch::channel(false);
    let registry = BlockingTasks::new();
    let caller_registry = registry.clone();
    let closed_registry = registry.clone();
    let (started, observed) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = OnDrop(dropped.clone());
    let mut roles = JoinSet::new();
    roles.spawn(async move {
        caller_registry
            .run(move || {
                let _guard = guard;
                started.send(()).unwrap();
                blocked.recv().unwrap();
            })
            .await;
        "oauth"
    });
    let supervisor = tokio::spawn(supervise_roles(
        roles,
        role_stop,
        shutdown,
        Duration::ZERO,
        registry,
    ));
    observed.await.unwrap();
    stop.send(true).unwrap();
    wait_for_shutdown(&mut role_shutdown).await;
    tokio::task::yield_now().await;
    assert!(
        !supervisor.is_finished(),
        "running blocking call must remain owned"
    );
    assert!(
        closed_registry
            .run(|| panic!("shutdown must reject new blocking calls"))
            .await
            .is_none()
    );
    release.send(()).unwrap();
    supervisor.await.unwrap();
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
        BlockingTasks::new(),
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
    // Exercise the same signal/worker composition used by main, not a second
    // timeout wrapper that could abort the supervisor during its final join.
    let mut supervisor = Some(supervisor);
    assert!(!wait_for_server_shutdown(async {}, &mut supervisor).await);
    external_stop.send(true).unwrap();
    let mut stopped = role_shutdown.clone();
    wait_for_shutdown(&mut stopped).await;
    let deadline_start = tokio::time::Instant::now();
    supervisor.unwrap().await.unwrap();
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
    let mut supervisor = Some(tokio::spawn(supervise_roles(
        roles,
        role_stop,
        external_shutdown,
        Duration::from_secs(30),
        BlockingTasks::new(),
    )));
    assert!(wait_for_server_shutdown(std::future::pending(), &mut supervisor).await);
    assert!(supervisor.is_none());
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
        BlockingTasks::new(),
    ));
    ready.await.unwrap();
    supervisor.abort();
    assert!(supervisor.await.unwrap_err().is_cancelled());
    tokio::task::yield_now().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}
