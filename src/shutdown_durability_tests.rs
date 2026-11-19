use super::*;

#[tokio::test]
async fn server_shutdown_joins_proxy_drain_before_stopping_worker() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let (draining, drain_started) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let (worker_stop, mut worker_shutdown) = watch::channel(false);
    let observed_worker = worker_shutdown.clone();
    let worker = tokio::spawn(async move {
        worker_shutdown.changed().await.unwrap();
        assert!(*worker_shutdown.borrow());
    });
    let process = tokio::spawn(serve_with_worker_shutdown(
        listener,
        axum::Router::new(),
        async { stopped.await.unwrap() },
        Some(worker),
        Some(worker_stop),
        None,
        async {
            draining.send(()).unwrap();
            released.await.unwrap();
        },
    ));
    stop.send(()).unwrap();
    drain_started.await.unwrap();
    assert!(!process.is_finished());
    assert!(!*observed_worker.borrow());
    release.send(()).unwrap();
    assert_eq!(process.await.unwrap(), Ok(()));
    assert!(*observed_worker.borrow());
}
