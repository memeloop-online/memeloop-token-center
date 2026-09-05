use super::*;

async fn fixture() -> Option<(Database, Uuid, Uuid)> {
    let database_url = std::env::var("MTC_TEST_POSTGRES_URL").ok()?;
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let tenant_id = Uuid::now_v7();
    let account_id = Uuid::now_v7();
    let now = unix_millis();
    sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3)")
        .bind(tenant_id.to_string())
        .bind(format!("breaker-fence-{tenant_id}"))
        .bind(now)
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO upstream_accounts (
             id, tenant_id, name, driver, auth_kind, config_json, status,
             credential_generation, created_at, updated_at
         ) VALUES ($1, $2, $3, 'http-json', 'none', '{}', 'active', 1, $4, $4)",
    )
    .bind(account_id.to_string())
    .bind(tenant_id.to_string())
    .bind(format!("breaker-fence-{account_id}"))
    .bind(now)
    .execute(&database.pool)
    .await
    .unwrap();
    Some((database, tenant_id, account_id))
}

#[tokio::test]
async fn failure_upsert_fences_generation_and_active_probe() {
    let Some((database, tenant_id, account_id)) = fixture().await else {
        return;
    };
    assert!(
        database
            .record_upstream_account_failure(account_id, 1, UpstreamFailureKind::Connection)
            .await
            .unwrap()
    );
    sqlx::query(
        "UPDATE upstream_account_health SET cooldown_until = 0, probe_lease_until = 0
         WHERE upstream_account_id = $1",
    )
    .bind(account_id.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    let UpstreamAttemptAdmission::Probe { lease_token } = database
        .claim_upstream_account_attempt(account_id, 1)
        .await
        .unwrap()
    else {
        panic!("half-open probe lease");
    };
    let (ordinary_failure, probe_renewal) = tokio::join!(
        database.record_upstream_account_failure(
            account_id,
            1,
            UpstreamFailureKind::InvalidResponse,
        ),
        database.renew_upstream_account_probe(account_id, 1, lease_token),
    );
    assert!(!ordinary_failure.unwrap());
    assert!(probe_renewal.unwrap());
    assert!(
        database
            .record_upstream_account_probe_success(account_id, 1, lease_token)
            .await
            .unwrap()
    );

    assert!(
        database
            .record_upstream_account_failure(account_id, 1, UpstreamFailureKind::Connection)
            .await
            .unwrap()
    );
    let mut rotation = database.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT upstream_account_id FROM upstream_account_health
         WHERE upstream_account_id = $1 FOR UPDATE",
    )
    .bind(account_id.to_string())
    .fetch_one(&mut *rotation)
    .await
    .unwrap();
    let stale_database = database.clone();
    let stale_failure = tokio::spawn(async move {
        stale_database
            .record_upstream_account_failure(account_id, 1, UpstreamFailureKind::InvalidResponse)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !stale_failure.is_finished(),
        "the old-generation terminal must wait on the health-row writer"
    );
    sqlx::query("UPDATE upstream_accounts SET credential_generation = 2 WHERE id = $1")
        .bind(account_id.to_string())
        .execute(&mut *rotation)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE upstream_account_health
         SET credential_generation = 2, probe_lease_until = 0, probe_lease_token = ''
         WHERE upstream_account_id = $1",
    )
    .bind(account_id.to_string())
    .execute(&mut *rotation)
    .await
    .unwrap();
    rotation.commit().await.unwrap();
    assert!(
        !tokio::time::timeout(std::time::Duration::from_secs(3), stale_failure)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        "the blocked old-generation UPSERT cannot downgrade newer health"
    );
    assert!(
        database
            .record_upstream_account_failure(account_id, 2, UpstreamFailureKind::Unavailable)
            .await
            .unwrap()
    );
    let generation: i64 = sqlx::query_scalar(
        "SELECT credential_generation FROM upstream_account_health
         WHERE upstream_account_id = $1",
    )
    .bind(account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(generation, 2);

    sqlx::query("DELETE FROM upstream_accounts WHERE id = $1")
        .bind(account_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM tenants WHERE id = $1")
        .bind(tenant_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
}
