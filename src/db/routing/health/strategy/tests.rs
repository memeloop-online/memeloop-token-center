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
    assert_eq!(
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
            .unwrap(),
        UpstreamAttemptAdmission::Healthy
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
}

#[tokio::test]
async fn postgres_strategy_admission_preserves_tenant_quota_and_cross_worker_lease() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    let peer = Database::connect(&url).await.unwrap();
    invariants(&database, &peer).await;
}
