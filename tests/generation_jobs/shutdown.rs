use super::*;
use memeloop_token_center::{AppState, config::Config, generation};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[tokio::test]
async fn dispatched_shutdown_is_durable_even_when_database_writes_are_blocked() {
    let (directory, database, key, upstream_id, price) = fixture().await;
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("generation.db").display()
    );
    let pool = AnyPool::connect(&database_url).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(tokio::sync::Notify::new());
    let app = axum::Router::new().route(
        "/api/v3/contents/generations/tasks",
        axum::routing::post({
            let requests = requests.clone();
            let observed = observed.clone();
            move |_: axum::body::Bytes| {
                let requests = requests.clone();
                let observed = observed.clone();
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    observed.notify_one();
                    std::future::pending::<axum::Json<serde_json::Value>>().await
                }
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    sqlx::query("UPDATE upstream_accounts SET driver = 'volcengine-seedance', config_json = $1 WHERE id = $2")
        .bind(json!({"base_url": format!("http://{address}")}).to_string())
        .bind(upstream_id.to_string()).execute(&pool).await.unwrap();
    let mut config = Config::for_test(database_url.clone());
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).unwrap();
    let state = AppState::initialize(config).await.unwrap();
    let reservation = reserve(&database, &key, &price).await;
    let mut create = input(&key, upstream_id, reservation.clone(), &price);
    create.driver = "volcengine-seedance".to_owned();
    create.request_object = state
        .archive
        .put_content(bytes::Bytes::from_static(b"{\"input\":{}}"))
        .await
        .unwrap();
    let job = database.create_generation_job(create).await.unwrap();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let worker_state = state.clone();
    let worker = tokio::spawn(async move {
        generation::process_one_until_shutdown(&worker_state, "shutdown-provider-worker", shutdown)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), observed.notified())
        .await
        .unwrap();
    let guarded = database
        .generation_job(key.key_id, job.job_id)
        .await
        .unwrap();
    assert_eq!(guarded.status, "submitting");
    assert_eq!(
        guarded.error_code.as_deref(),
        Some("shutdown_delivery_unknown")
    );
    assert!(guarded.upstream_job_id.is_none());
    // No shutdown-time write is possible. The pre-dispatch commit is sufficient.
    let mut lock = pool.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *lock)
        .await
        .unwrap();
    stop.send(true).unwrap();
    assert!(
        !tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    sqlx::query("ROLLBACK").execute(&mut *lock).await.unwrap();
    drop(lock);
    // Force both expiry and age-out: neither may turn this into a refund.
    sqlx::query("UPDATE generation_jobs SET lease_expires_at = 0, next_attempt_at = 0, created_at = 0 WHERE id = $1")
        .bind(job.job_id.to_string()).execute(&pool).await.unwrap();
    let restarted = AppState::initialize({
        let mut config = Config::for_test(database_url);
        config.key_pepper = String::from_utf8(PEPPER.to_vec()).unwrap();
        config
    })
    .await
    .unwrap();
    for _ in 0..3 {
        assert!(
            !generation::process_one(&restarted, "restarted-worker")
                .await
                .unwrap()
        );
    }
    assert!(
        database
            .cancel_generation_job(key.key_id, job.job_id)
            .await
            .is_err()
    );
    let row = sqlx::query("SELECT status, actual_micros FROM usage_reservations WHERE id = $1")
        .bind(reservation.id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("status"), "reserved");
    assert_eq!(row.get::<Option<i64>, _>("actual_micros"), None);
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "9.75"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn quarantine_fences_nonce_and_ack_clears_it_without_changing_ordinary_recovery() {
    let (directory, database, key, upstream_id, price) = fixture().await;
    let pool = AnyPool::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("generation.db").display()
    ))
    .await
    .unwrap();
    let reservation = reserve(&database, &key, &price).await;
    let job = database
        .create_generation_job(input(&key, upstream_id, reservation, &price))
        .await
        .unwrap();
    database
        .claim_generation_job("original")
        .await
        .unwrap()
        .unwrap();
    let nonce = Uuid::now_v7();
    database
        .mark_generation_submitting(job.job_id, "original", nonce)
        .await
        .unwrap();
    assert!(
        database
            .arm_generation_shutdown_quarantine(job.job_id, "other", nonce)
            .await
            .is_err()
    );
    assert!(
        database
            .arm_generation_shutdown_quarantine(job.job_id, "original", Uuid::now_v7())
            .await
            .is_err()
    );
    // Ordinary submitting rows remain claimable after lease expiry.
    sqlx::query("UPDATE generation_jobs SET lease_expires_at = 0 WHERE id = $1")
        .bind(job.job_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        database
            .claim_generation_job("recovered")
            .await
            .unwrap()
            .unwrap()
            .status,
        "submitting"
    );
    database
        .arm_generation_shutdown_quarantine(job.job_id, "recovered", nonce)
        .await
        .unwrap();
    database
        .mark_generation_submitted(job.job_id, "recovered", nonce, "confirmed-provider-job")
        .await
        .unwrap();
    let acknowledged = database
        .generation_job(key.key_id, job.job_id)
        .await
        .unwrap();
    assert_eq!(acknowledged.status, "running");
    assert_eq!(acknowledged.error_code, None);
    assert_eq!(
        acknowledged.upstream_job_id.as_deref(),
        Some("confirmed-provider-job")
    );
}

#[tokio::test]
async fn generic_retry_cannot_clear_an_armed_delivery_unknown_guard() {
    let (_directory, database, key, upstream_id, price) = fixture().await;
    let reservation = reserve(&database, &key, &price).await;
    let job = database
        .create_generation_job(input(&key, upstream_id, reservation, &price))
        .await
        .unwrap();
    database
        .claim_generation_job("normal-error")
        .await
        .unwrap()
        .unwrap();
    let nonce = Uuid::now_v7();
    database
        .mark_generation_submitting(job.job_id, "normal-error", nonce)
        .await
        .unwrap();
    database
        .arm_generation_shutdown_quarantine(job.job_id, "normal-error", nonce)
        .await
        .unwrap();
    assert!(
        database
            .reschedule_generation_job(job.job_id, "normal-error", 500, Some("upstream_retry"))
            .await
            .is_err()
    );
    let retried = database
        .generation_job(key.key_id, job.job_id)
        .await
        .unwrap();
    assert_eq!(retried.status, "submitting");
    assert_eq!(
        retried.error_code.as_deref(),
        Some("shutdown_delivery_unknown")
    );
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "9.75"
    );
}
