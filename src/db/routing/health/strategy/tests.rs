use super::*;

async fn invariants(database: &Database, peer: &Database) {
    database.migrate().await.unwrap();
    let tenant = Uuid::now_v7();
    let account = Uuid::now_v7();
    sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1,$1,0)")
        .bind(tenant.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO upstream_accounts (id,tenant_id,name,driver,auth_kind,config_json,status,credential_generation,created_at,updated_at) VALUES ($1,$2,'strategy fixture','http-json','none','{}','active',1,0,0)")
        .bind(account.to_string()).bind(tenant.to_string()).execute(&database.pool).await.unwrap();
    let health = UpstreamHealthConfig::DEFAULT;
    assert!(
        database
            .group_routing_health(Uuid::now_v7(), account, 1)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        database
            .group_routing_health(tenant, account, 2)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        database
            .claim_upstream_account_attempt_with_strategy(
                tenant,
                account,
                1,
                health,
                false,
                Some(0),
                false
            )
            .await
            .unwrap()
            .is_healthy()
    );

    for kind in [
        UpstreamFailureKind::Connection,
        UpstreamFailureKind::Unavailable,
        UpstreamFailureKind::InvalidResponse,
    ] {
        sqlx::query("DELETE FROM upstream_account_health WHERE upstream_account_id = $1")
            .bind(account.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        database
            .record_upstream_account_failure(account, 1, kind)
            .await
            .unwrap();
        assert!(
            database
                .claim_upstream_account_attempt_with_strategy(
                    tenant,
                    account,
                    1,
                    health,
                    false,
                    Some(0),
                    false
                )
                .await
                .unwrap()
                .is_unavailable()
        );
        assert!(
            database
                .claim_upstream_account_attempt_with_strategy(
                    tenant,
                    account,
                    1,
                    health,
                    true,
                    Some(u64::MAX),
                    false
                )
                .await
                .unwrap()
                .is_unavailable()
        );
        let (first, second) = tokio::join!(
            database.claim_upstream_account_attempt_with_strategy(
                tenant,
                account,
                1,
                health,
                true,
                Some(0),
                true
            ),
            peer.claim_upstream_account_attempt_with_strategy(
                tenant,
                account,
                1,
                health,
                true,
                Some(0),
                true
            ),
        );
        assert_eq!(
            [first.unwrap(), second.unwrap()]
                .iter()
                .filter(|a| matches!(a, UpstreamAttemptAdmission::Probe { .. }))
                .count(),
            1
        );
        assert!(
            database
                .claim_upstream_account_attempt_with_strategy(
                    tenant,
                    account,
                    1,
                    health,
                    true,
                    Some(0),
                    false
                )
                .await
                .unwrap()
                .is_unavailable(),
            "override must never steal an active lease"
        );
    }
    for kind in [
        UpstreamFailureKind::Authentication,
        UpstreamFailureKind::RateLimited,
        UpstreamFailureKind::RateLimitedUntil {
            until: unix_millis() + 3_600_000,
            exhausted: true,
        },
    ] {
        sqlx::query("DELETE FROM upstream_account_health WHERE upstream_account_id = $1")
            .bind(account.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        database
            .record_upstream_account_failure(account, 1, kind)
            .await
            .unwrap();
        database
            .record_upstream_account_failure(account, 1, UpstreamFailureKind::Connection)
            .await
            .unwrap();
        let snapshot = database
            .group_routing_health(tenant, account, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot.last_failure_kind,
            kind.as_str(),
            "transient failure cannot relabel an active hard cooldown"
        );
        assert!(
            database
                .claim_upstream_account_attempt_with_strategy(
                    tenant,
                    account,
                    1,
                    health,
                    true,
                    Some(0),
                    false
                )
                .await
                .unwrap()
                .is_unavailable()
        );
    }
    sqlx::query("UPDATE upstream_accounts SET status = 'disabled' WHERE id = $1")
        .bind(account.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        database
            .group_routing_health(tenant, account, 1)
            .await
            .unwrap()
            .is_none()
    );
}

async fn transient_signal_invariants(database: &Database) {
    database.migrate().await.unwrap();
    let tenant = Uuid::now_v7();
    let account = Uuid::now_v7();
    sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1,$1,0)")
        .bind(tenant.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO upstream_accounts (id,tenant_id,name,driver,auth_kind,config_json,status,credential_generation,created_at,updated_at) VALUES ($1,$2,'signal fixture','http-json','none','{}','active',3,0,0)")
        .bind(account.to_string())
        .bind(tenant.to_string())
        .execute(&database.pool)
        .await
        .unwrap();

    let failed = database
        .record_transient_health_sample(account, 3, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.sample_count, 1);
    assert_eq!(failed.ewma_micros, TRANSIENT_EWMA_SCALE);
    assert_eq!(failed.recovery_successes, 0);
    assert_eq!(failed.revision, 1);
    assert!(failed.sample_count >= 1 && failed.ewma_micros >= 900_000);

    let first_success = database
        .record_transient_health_sample(account, 3, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_success.sample_count, 2);
    assert_eq!(first_success.ewma_micros, 750_000);
    assert_eq!(first_success.recovery_successes, 1);
    assert_eq!(first_success.revision, 2);
    assert!(first_success.ewma_micros > 600_000 || first_success.recovery_successes < 2);

    let second_success = database
        .record_transient_health_sample(account, 3, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second_success.ewma_micros, 562_500);
    assert_eq!(second_success.recovery_successes, 2);
    assert!(second_success.ewma_micros <= 600_000 && second_success.recovery_successes >= 2);
    assert!(
        database
            .record_transient_health_sample(account, 2, true)
            .await
            .unwrap()
            .is_none(),
        "a stale credential generation cannot publish a signal"
    );
    sqlx::query("UPDATE upstream_accounts SET credential_generation = 4 WHERE id = $1")
        .bind(account.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    let rotated = database
        .record_transient_health_sample(account, 4, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rotated.sample_count, 1);
    assert_eq!(rotated.ewma_micros, 0);
    assert_eq!(rotated.recovery_successes, 1);
    assert_eq!(rotated.revision, 1);
    let older_completion = sqlx::query(RECORD_TRANSIENT_HEALTH_SAMPLE_SQL)
        .bind(account.to_string())
        .bind(4_i64)
        .bind(0_i64)
        .bind(rotated.last_observed_at.saturating_sub(1))
        .bind(1_i64)
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        older_completion
            .try_get::<i64, _>("last_observed_at")
            .unwrap(),
        rotated.last_observed_at,
        "same-generation completion order cannot move last_observed_at backwards"
    );
    let retained_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_account_transient_health_signals WHERE upstream_account_id = $1",
    )
    .bind(account.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(
        retained_rows, 1,
        "credential rotation replaces the bounded signal row"
    );
}

async fn postgres_blocked_old_generation_cannot_overwrite_rotated_signal(database: &Database) {
    database.migrate().await.unwrap();
    let tenant = Uuid::now_v7();
    let account = Uuid::now_v7();
    sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1,$1,0)")
        .bind(tenant.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO upstream_accounts (id,tenant_id,name,driver,auth_kind,config_json,status,credential_generation,created_at,updated_at) VALUES ($1,$2,'signal race fixture','http-json','none','{}','active',3,0,0)")
        .bind(account.to_string())
        .bind(tenant.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    database
        .record_transient_health_sample(account, 3, true)
        .await
        .unwrap()
        .unwrap();

    let mut rotation = database.pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE upstream_account_transient_health_signals
            SET revision = revision
          WHERE upstream_account_id = $1",
    )
    .bind(account.to_string())
    .execute(&mut *rotation)
    .await
    .unwrap();

    let mut old_connection = database.pool.acquire().await.unwrap();
    let old_pid: i64 = sqlx::query_scalar("SELECT CAST(pg_backend_pid() AS BIGINT)")
        .fetch_one(&mut *old_connection)
        .await
        .unwrap();
    let account_id = account.to_string();
    let old_sample = tokio::spawn(async move {
        sqlx::query(RECORD_TRANSIENT_HEALTH_SAMPLE_SQL)
            .bind(account_id)
            .bind(3_i64)
            .bind(TRANSIENT_EWMA_SCALE)
            .bind(300_i64)
            .bind(0_i64)
            .fetch_optional(&mut *old_connection)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "SELECT COALESCE(wait_event_type = 'Lock', FALSE)
                   FROM pg_stat_activity
                  WHERE pid = CAST($1 AS INTEGER)",
            )
            .bind(old_pid)
            .fetch_one(&database.pool)
            .await
            .unwrap();
            if blocked {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("old generation sample must reach the row-lock barrier");

    sqlx::query("UPDATE upstream_accounts SET credential_generation = 4 WHERE id = $1")
        .bind(account.to_string())
        .execute(&mut *rotation)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE upstream_account_transient_health_signals
            SET credential_generation = 4, sample_count = 1, ewma_micros = 0,
                last_observed_at = 400, recovery_successes = 1, revision = 1
          WHERE upstream_account_id = $1",
    )
    .bind(account.to_string())
    .execute(&mut *rotation)
    .await
    .unwrap();
    rotation.commit().await.unwrap();

    let stale_result = tokio::time::timeout(std::time::Duration::from_secs(5), old_sample)
        .await
        .expect("blocked old generation sample must finish")
        .unwrap()
        .unwrap();
    assert!(
        stale_result.is_none(),
        "a statement that observed generation 3 before blocking cannot overwrite generation 4"
    );
    let row = sqlx::query(
        "SELECT credential_generation, sample_count, ewma_micros,
                last_observed_at, recovery_successes, revision
           FROM upstream_account_transient_health_signals
          WHERE upstream_account_id = $1",
    )
    .bind(account.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("credential_generation").unwrap(), 4);
    assert_eq!(row.try_get::<i64, _>("sample_count").unwrap(), 1);
    assert_eq!(row.try_get::<i64, _>("last_observed_at").unwrap(), 400);
    assert_eq!(row.try_get::<i64, _>("revision").unwrap(), 1);
}

#[test]
fn override_is_transient_only_and_bounded() {
    let mut snapshot = GroupRoutingHealth {
        consecutive_failures: 1,
        last_failure_kind: "connection".into(),
        cooldown_until: 500,
        probe_lease_until: 0,
        updated_at: 100,
    };
    assert_eq!(snapshot.effective_cooldown_until(Some(u64::MAX)), 60_100);
    assert_eq!(snapshot.effective_cooldown_until(None), 500);
    assert_eq!(snapshot.effective_cooldown_until(Some(0)), 100);
    snapshot.last_failure_kind = "quota_exhausted".into();
    assert_eq!(snapshot.effective_cooldown_until(Some(0)), 500);
}

#[tokio::test]
async fn sqlite_strategy_admission_preserves_tenant_quota_and_cross_worker_lease() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("strategy.db").display()
    );
    let database = Database::connect(&url).await.unwrap();
    let peer = Database::connect(&url).await.unwrap();
    invariants(&database, &peer).await;
    transient_signal_invariants(&database).await;
}

#[tokio::test]
async fn postgres_strategy_admission_preserves_tenant_quota_and_cross_worker_lease() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    let peer = Database::connect(&url).await.unwrap();
    invariants(&database, &peer).await;
    transient_signal_invariants(&database).await;
    postgres_blocked_old_generation_cannot_overwrite_rotated_signal(&database).await;
}
