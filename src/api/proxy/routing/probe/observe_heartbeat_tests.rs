use super::*;

const TEST_PROBE_LEASE_MILLIS: i64 = 1_000;
const TEST_PROBE_HEARTBEAT_MILLIS: i64 = 25;

#[tokio::test]
async fn customer_success_does_not_wait_on_observe_or_heal_the_breaker() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("observe-lease.db").display()
    );
    let mut config = crate::config::Config::for_test(url.clone());
    config.archive_backend = crate::config::ArchiveBackend::Filesystem;
    config.archive_path = Some(directory.path().join("archive").display().to_string());
    config.upstream_health.probe_lease_millis = TEST_PROBE_LEASE_MILLIS;
    config.upstream_health.probe_heartbeat_millis = TEST_PROBE_HEARTBEAT_MILLIS;
    let state = AppState::initialize(config).await.unwrap();
    let pool = sqlx::AnyPool::connect(&crate::db::Database::sqlite_database_url(&url))
        .await
        .unwrap();
    let tenant = Uuid::now_v7();
    let account = Uuid::now_v7();
    sqlx::query("INSERT INTO tenants (id,external_id,created_at) VALUES ($1,$1,0)")
        .bind(tenant.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO upstream_accounts (id,tenant_id,name,driver,auth_kind,config_json,status,credential_generation,created_at,updated_at) VALUES ($1,$2,'observe fixture','http-json','none','{}','active',1,0,0)")
        .bind(account.to_string()).bind(tenant.to_string()).execute(&pool).await.unwrap();
    state
        .db
        .record_upstream_account_failure(account, 1, UpstreamFailureKind::Connection)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE upstream_account_health SET cooldown_until = 0 WHERE upstream_account_id = $1",
    )
    .bind(account.to_string())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO upstream_service_probes (upstream_account_id, status, error_code, latency_ms, checked_at)
         VALUES ($1, 'unhealthy', 'upstream_unavailable', 1, 1)",
    )
    .bind(account.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let admission = state
        .db
        .claim_upstream_account_attempt_with_health_config(account, 1, state.config.upstream_health)
        .await
        .unwrap();
    assert!(matches!(admission, UpstreamAttemptAdmission::Probe { .. }));
    let request_id = Uuid::now_v7();
    let mut guard = UpstreamAttemptGuard::new(
        &state,
        request_id,
        Uuid::now_v7(),
        account,
        1,
        0,
        admission,
        None,
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        guard.complete(UpstreamAttemptTerminal::Succeeded),
    )
    .await
    .expect("customer success must not wait on a plugin observe hook");
    state.routing_persistence.drain_for_test().await;
    let failures: i64 = sqlx::query_scalar(
        "SELECT consecutive_failures FROM upstream_account_health WHERE upstream_account_id=$1",
    )
    .bind(account.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        failures > 0,
        "customer success is not an account health sample"
    );
    let status: String = sqlx::query_scalar(
        "SELECT status FROM upstream_service_probes WHERE upstream_account_id = $1",
    )
    .bind(account.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "unhealthy");
    let mut observed = UpstreamAttemptGuard::new(
        &state,
        Uuid::now_v7(),
        Uuid::now_v7(),
        account,
        1,
        0,
        UpstreamAttemptAdmission::Observed,
        None,
    );
    observed.complete(UpstreamAttemptTerminal::Succeeded).await;
    state.routing_persistence.drain_for_test().await;
    let status: String = sqlx::query_scalar(
        "SELECT status FROM upstream_service_probes WHERE upstream_account_id = $1",
    )
    .bind(account.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "unhealthy");
    let probe = state
        .db
        .upstream_service_probe(account)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(probe["checked_at"], 1);
    assert_eq!(probe["status"], "unknown");
    assert_eq!(probe["source"], "legacy_unverified");
    assert_eq!(
        probe["historical_result"]["error_code"],
        "upstream_unavailable"
    );
    assert_eq!(probe["historical_result"]["latency_ms"], 1);
    assert!(
        state
            .db
            .upstream_service_probes_due(0, 1, 32)
            .await
            .unwrap()
            .contains(&account)
    );
    pool.close().await;
}
