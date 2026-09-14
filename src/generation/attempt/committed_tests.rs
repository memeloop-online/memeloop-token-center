use super::*;
use crate::db::UpstreamAttemptAdmission;
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn committed_observation_survives_caller_cancellation_and_keeps_probe_heartbeat() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("committed-health.db").display()
    );
    let mut config = crate::config::Config::for_test(url.clone());
    config.upstream_health.probe_lease_millis = 100;
    config.upstream_health.probe_heartbeat_millis = 10;
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
    let gate = crate::group_routing::test_observe_gate::install(request);
    let guard =
        MediaAttemptGuard::new(&state, request, Uuid::now_v7(), account, 1, admission, None);
    // This is the exact helper used only after a successful job CAS. Cancel
    // its caller while the actual observed completion is demonstrably live.
    let caller = tokio::spawn(complete_committed(guard, MediaAttemptTerminal::Succeeded));
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(250)).await;
    let lease_until: i64 = sqlx::query_scalar(
        "SELECT probe_lease_until FROM upstream_account_health WHERE upstream_account_id=$1",
    )
    .bind(account.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        lease_until > unix_millis(),
        "committed completion must retain its own heartbeat after caller cancellation"
    );
    gate.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let failures: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upstream_account_health WHERE upstream_account_id=$1 AND consecutive_failures>0")
                .bind(account.to_string()).fetch_one(&pool).await.unwrap();
            if failures == 0 { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    pool.close().await;
}
