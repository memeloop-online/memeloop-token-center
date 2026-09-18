use super::*;

const TEST_PROBE_LEASE_MILLIS: i64 = 1_000;
const TEST_PROBE_HEARTBEAT_MILLIS: i64 = 25;

async fn wait_for_heartbeat_beyond_deadline(
    pool: &sqlx::AnyPool,
    account: Uuid,
    initial_deadline: i64,
) -> i64 {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while unix_millis() <= initial_deadline {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let post_deadline_lease = loop {
            let now = unix_millis();
            let lease_until: i64 = sqlx::query_scalar(
                "SELECT probe_lease_until FROM upstream_account_health WHERE upstream_account_id=$1",
            )
            .bind(account.to_string())
            .fetch_one(pool)
            .await
            .unwrap();
            if lease_until > now {
                break lease_until;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        // Require another persisted renewal after the original deadline. A
        // heartbeat that fires only once and then stops cannot satisfy this.
        loop {
            let lease_until: i64 = sqlx::query_scalar(
                "SELECT probe_lease_until FROM upstream_account_health WHERE upstream_account_id=$1",
            )
            .bind(account.to_string())
            .fetch_one(pool)
            .await
            .unwrap();
            if lease_until > post_deadline_lease && lease_until > unix_millis() {
                return lease_until;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("probe heartbeat must remain live beyond the original lease deadline")
}

#[tokio::test]
async fn terminal_observe_keeps_half_open_lease_alive_until_fenced_settlement() {
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
    let peer = crate::db::Database::connect(&url).await.unwrap();
    let pool = sqlx::AnyPool::connect(&url).await.unwrap();
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
    let admission = state
        .db
        .claim_upstream_account_attempt_with_health_config(account, 1, state.config.upstream_health)
        .await
        .unwrap();
    assert!(matches!(admission, UpstreamAttemptAdmission::Probe { .. }));
    let request_id = Uuid::now_v7();
    let gate = crate::group_routing::test_observe_gate::install(request_id);
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
    let completion = tokio::spawn(async move {
        guard.complete(UpstreamAttemptTerminal::Succeeded).await;
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    let initial_deadline: i64 = sqlx::query_scalar(
        "SELECT probe_lease_until FROM upstream_account_health WHERE upstream_account_id=$1",
    )
    .bind(account.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let renewed = wait_for_heartbeat_beyond_deadline(&pool, account, initial_deadline).await;
    assert!(renewed > unix_millis());
    let competing = peer
        .claim_upstream_account_attempt_with_health_config(account, 1, state.config.upstream_health)
        .await
        .unwrap();
    gate.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), completion)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(competing, UpstreamAttemptAdmission::Unavailable { .. }),
        "a slow terminal hook must not surrender the live probe lease"
    );
    assert!(
        peer.claim_upstream_account_attempt_with_health_config(
            account,
            1,
            state.config.upstream_health
        )
        .await
        .unwrap()
        .is_healthy()
    );
    pool.close().await;
}
