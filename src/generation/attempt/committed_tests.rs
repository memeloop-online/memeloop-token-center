use super::*;
use crate::db::UpstreamAttemptAdmission;
use std::time::Duration;
use uuid::Uuid;

const TEST_PROBE_LEASE_MILLIS: i64 = 1_000;
const TEST_PROBE_HEARTBEAT_MILLIS: i64 = 25;

#[tokio::test]
async fn committed_customer_success_does_not_heal_the_breaker() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("committed-health.db").display()
    );
    let mut config = crate::config::Config::for_test(url.clone());
    config.upstream_health.probe_lease_millis = TEST_PROBE_LEASE_MILLIS;
    config.upstream_health.probe_heartbeat_millis = TEST_PROBE_HEARTBEAT_MILLIS;
    let state = AppState::initialize(config).await.unwrap();
    let pool = sqlx::AnyPool::connect(&url).await.unwrap();
    let tenant = Uuid::now_v7();
    let account = Uuid::now_v7();
    sqlx::query("INSERT INTO tenants (id,external_id,created_at) VALUES ($1,$1,0)")
        .bind(tenant.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO upstream_accounts (id,tenant_id,name,driver,auth_kind,config_json,status,credential_generation,created_at,updated_at) VALUES ($1,$2,'committed fixture','http-json','none','{}','active',1,0,0)")
        .bind(account.to_string()).bind(tenant.to_string()).execute(&pool).await.unwrap();
    state
        .db
        .record_upstream_account_failure(account, 1, UpstreamFailureKind::Connection)
        .await
        .unwrap();
    sqlx::query("UPDATE upstream_account_health SET cooldown_until=0 WHERE upstream_account_id=$1")
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
    let request = Uuid::now_v7();
    let guard = MediaAttemptGuard::new(
        &state,
        request,
        Uuid::now_v7(),
        account,
        1,
        0,
        admission,
        None,
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        complete_committed(guard, MediaAttemptTerminal::Succeeded),
    )
    .await
    .expect("committed customer success must not wait on a plugin observe hook");
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
        "a committed customer success is not an account health sample"
    );
    pool.close().await;
}
