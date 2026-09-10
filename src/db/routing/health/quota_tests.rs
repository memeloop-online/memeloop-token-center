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
    sqlx::query("INSERT INTO upstream_accounts (id,tenant_id,name,driver,auth_kind,config_json,status,credential_generation,created_at,updated_at) VALUES ($1,$2,'quota fixture','http-json','none','{}','active',1,0,0)")
        .bind(account.to_string()).bind(tenant.to_string()).execute(&database.pool).await.unwrap();
    let long = unix_millis() + 3_600_000;
    let short = unix_millis() + 60_000;
    let (a, b) = tokio::join!(
        database.record_upstream_account_failure(
            account,
            1,
            UpstreamFailureKind::RateLimitedUntil {
                until: long,
                exhausted: true
            }
        ),
        peer.record_upstream_account_failure(
            account,
            1,
            UpstreamFailureKind::RateLimitedUntil {
                until: short,
                exhausted: false
            }
        ),
    );
    assert!(a.unwrap());
    assert!(b.unwrap());
    let deadline: i64 = sqlx::query_scalar(
        "SELECT cooldown_until FROM upstream_account_health WHERE upstream_account_id = $1",
    )
    .bind(account.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(
        deadline, long,
        "a shorter concurrent Retry-After cannot reopen exhausted quota"
    );
    assert_eq!(
        peer.claim_upstream_account_attempt(account, 1)
            .await
            .unwrap(),
        UpstreamAttemptAdmission::Unavailable
    );
    database
        .record_upstream_account_failure(account, 1, UpstreamFailureKind::Connection)
        .await
        .unwrap();
    let deadline: i64 = sqlx::query_scalar(
        "SELECT cooldown_until FROM upstream_account_health WHERE upstream_account_id = $1",
    )
    .bind(account.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(
        deadline, long,
        "ordinary failures must not shorten quota cooldown"
    );
    sqlx::query(
        "UPDATE upstream_account_health SET cooldown_until = 0 WHERE upstream_account_id = $1",
    )
    .bind(account.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    let (a, b) = tokio::join!(
        database.claim_upstream_account_attempt(account, 1),
        peer.claim_upstream_account_attempt(account, 1),
    );
    let admissions = [a.unwrap(), b.unwrap()];
    assert_eq!(
        admissions
            .iter()
            .filter(|admission| matches!(admission, UpstreamAttemptAdmission::Probe { .. }))
            .count(),
        1
    );
    let token = admissions
        .into_iter()
        .find_map(|admission| match admission {
            UpstreamAttemptAdmission::Probe { lease_token } => Some(lease_token),
            _ => None,
        })
        .unwrap();
    assert!(
        !peer
            .record_upstream_account_probe_failure(
                account,
                1,
                Uuid::now_v7(),
                UpstreamFailureKind::RateLimitedUntil {
                    until: long,
                    exhausted: true
                }
            )
            .await
            .unwrap()
    );
    assert!(
        !peer
            .record_upstream_account_failure(
                account,
                1,
                UpstreamFailureKind::RateLimitedUntil {
                    until: long,
                    exhausted: true
                }
            )
            .await
            .unwrap(),
        "stale ordinary work cannot overwrite the active probe"
    );
    assert!(
        database
            .record_upstream_account_probe_failure(
                account,
                1,
                token,
                UpstreamFailureKind::RateLimitedUntil {
                    until: long,
                    exhausted: true
                }
            )
            .await
            .unwrap()
    );
    assert!(
        !peer
            .record_upstream_account_probe_success(account, 1, token)
            .await
            .unwrap(),
        "a stale success cannot clear quota failure"
    );
    let row = sqlx::query("SELECT cooldown_until,last_failure_kind FROM upstream_account_health WHERE upstream_account_id = $1")
        .bind(account.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(row.get::<i64, _>("cooldown_until"), long);
    assert_eq!(row.get::<String, _>("last_failure_kind"), "quota_exhausted");
    sqlx::query("UPDATE upstream_accounts SET credential_generation = 2 WHERE id = $1")
        .bind(account.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        !peer
            .record_upstream_account_failure(
                account,
                1,
                UpstreamFailureKind::RateLimitedUntil {
                    until: long,
                    exhausted: true
                }
            )
            .await
            .unwrap()
    );
    assert_eq!(
        database
            .claim_upstream_account_attempt(account, 2)
            .await
            .unwrap(),
        UpstreamAttemptAdmission::Healthy
    );
}

#[tokio::test]
async fn sqlite_two_workers_preserve_quota_deadline_and_probe_generation_fences() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("quota.db").display()
    );
    let database = Database::connect(&url).await.unwrap();
    let peer = Database::connect(&url).await.unwrap();
    invariants(&database, &peer).await;
}

#[tokio::test]
async fn postgres_two_workers_preserve_quota_deadline_and_probe_generation_fences() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    let peer = Database::connect(&url).await.unwrap();
    invariants(&database, &peer).await;
}
