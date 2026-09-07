use std::time::Duration;

use sqlx::Row;

use super::super::Database;

async fn postgres_timeout(database: &Database, setting: &str) -> String {
    sqlx::query("SELECT current_setting($1) AS value")
        .bind(setting)
        .fetch_one(&database.pool)
        .await
        .unwrap()
        .try_get("value")
        .unwrap()
}

#[tokio::test]
async fn postgres_serve_and_migration_pools_have_bounded_session_timeouts() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!("MTC_TEST_POSTGRES_URL is unset; skipping PostgreSQL pool timeout contract");
        return;
    };

    let serve = Database::connect_with_max(&database_url, 1).await.unwrap();
    assert_eq!(postgres_timeout(&serve, "statement_timeout").await, "30s");
    assert_eq!(postgres_timeout(&serve, "lock_timeout").await, "10s");
    assert_eq!(
        postgres_timeout(&serve, "idle_in_transaction_session_timeout").await,
        "30s"
    );

    let migration = Database::connect_for_migration(&database_url, 1)
        .await
        .unwrap();
    assert_eq!(
        postgres_timeout(&migration, "statement_timeout").await,
        "15min"
    );
    assert_eq!(postgres_timeout(&migration, "lock_timeout").await, "1min");
    assert_eq!(
        postgres_timeout(&migration, "idle_in_transaction_session_timeout").await,
        "5min"
    );
}

#[tokio::test]
async fn sqlite_roles_use_wal_and_read_through_an_uncommitted_writer() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("role-concurrency.db").display()
    );
    let writer = Database::connect_with_max(&database_url, 2).await.unwrap();
    writer.migrate().await.unwrap();
    let reader = Database::connect_with_max(&database_url, 2).await.unwrap();

    for database in [&writer, &reader] {
        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&database.pool)
            .await
            .unwrap();
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    }

    let mut transaction = writer.begin_write_transaction().await.unwrap();
    sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3)")
        .bind(uuid::Uuid::now_v7().to_string())
        .bind("uncommitted-writer")
        .bind(1_i64)
        .execute(&mut *transaction)
        .await
        .unwrap();

    let visible_rows: i64 = tokio::time::timeout(
        Duration::from_secs(1),
        sqlx::query_scalar("SELECT COUNT(*) FROM tenants").fetch_one(&reader.pool),
    )
    .await
    .expect("WAL readers must not wait for an uncommitted writer")
    .unwrap();
    assert_eq!(visible_rows, 0);
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn two_connection_pool_keeps_all_control_pages_bounded_under_concurrent_reads() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory
            .path()
            .join("control-page-concurrency.db")
            .display()
    );
    // This matches the acceptance harness: it fans 16 concurrent requests
    // across the four global control lists while the lightweight SQLite pool
    // has only two connections.  Keep the fixture above one page so each
    // method must enforce its own hard limit before joins or aggregations.
    let database = Database::connect_with_max(&database_url, 2).await.unwrap();
    database.migrate().await.unwrap();
    let mut transaction = database.pool.begin().await.unwrap();
    for index in 1..=256_u128 {
        let tenant_id = uuid::Uuid::from_u128(index);
        let service_id = uuid::Uuid::from_u128((1_u128 << 64) | index);
        let service_credential_id = uuid::Uuid::from_u128((2_u128 << 64) | index);
        let upstream_id = uuid::Uuid::from_u128((3_u128 << 64) | index);
        let upstream_credential_id = uuid::Uuid::from_u128((4_u128 << 64) | index);
        let route_id = uuid::Uuid::from_u128((5_u128 << 64) | index);
        let created_at = index as i64;

        sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3)")
            .bind(tenant_id.to_string())
            .bind(format!("control-page-tenant-{index:03}"))
            .bind(created_at)
            .execute(&mut *transaction)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO service_principals (id, name, status, credential_generation, created_at, updated_at) VALUES ($1, $2, 'active', 1, $3, $3)",
        )
        .bind(service_id.to_string())
        .bind(format!("control-page-service-{index:03}"))
        .bind(created_at)
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO service_credentials (id, service_principal_id, generation, secret_hash, fingerprint, scopes_json, tenant_external_id, created_at) VALUES ($1, $2, 1, $3, $4, '[\"requests:read\"]', NULL, $5)",
        )
        .bind(service_credential_id.to_string())
        .bind(service_id.to_string())
        .bind(vec![0_u8])
        .bind(format!("control-page-fingerprint-{index:03}"))
        .bind(created_at)
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, created_at, updated_at) VALUES ($1, $2, $3, 'http-json', 'none', '{}', 'active', 1, $4, $4)",
        )
        .bind(upstream_id.to_string())
        .bind(tenant_id.to_string())
        .bind(format!("control-page-upstream-{index:03}"))
        .bind(created_at)
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, 1, 'opaque', NULL, $3)",
        )
        .bind(upstream_credential_id.to_string())
        .bind(upstream_id.to_string())
        .bind(created_at)
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO model_routes (id, tenant_id, public_model, upstream_account_id, upstream_model, protocol, priority, enabled, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, 'openai', 0, 1, $6, $6)",
        )
        .bind(route_id.to_string())
        .bind(tenant_id.to_string())
        .bind(format!("control-page-model-{index:03}"))
        .bind(upstream_id.to_string())
        .bind(format!("control-page-upstream-model-{index:03}"))
        .bind(created_at)
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO model_route_upstream_accounts (tenant_id, model_route_id, upstream_account_id, upstream_model, scheduling_weight, created_at, catalog_policy) VALUES ($1, $2, $3, $4, 100, $5, 'explicit_custom')",
        )
        .bind(tenant_id.to_string())
        .bind(route_id.to_string())
        .bind(upstream_id.to_string())
        .bind(format!("control-page-upstream-model-{index:03}"))
        .bind(created_at)
        .execute(&mut *transaction)
        .await
        .unwrap();
    }
    transaction.commit().await.unwrap();

    let tasks = (0..4)
        .flat_map(|_| {
            let tenants = database.clone();
            let service_tokens = database.clone();
            let upstreams = database.clone();
            let routes = database.clone();
            [
                tokio::spawn(async move {
                    tenants
                        .list_tenants_page(None, 1_000_000)
                        .await
                        .map(|page| page.len())
                }),
                tokio::spawn(async move {
                    service_tokens
                        .list_service_tokens_page(None, None, 1_000_000)
                        .await
                        .map(|page| page.len())
                }),
                tokio::spawn(async move {
                    upstreams
                        .list_upstream_accounts_page(None, None, None, 1_000_000)
                        .await
                        .map(|page| page.len())
                }),
                tokio::spawn(async move {
                    routes
                        .list_enriched_model_routes_page(None, None, None, 1_000_000)
                        .await
                        .map(|page| page.len())
                }),
            ]
        })
        .collect::<Vec<_>>();
    let pages = futures_util::future::try_join_all(tasks).await.unwrap();
    assert_eq!(pages.len(), 16);
    assert!(
        pages
            .into_iter()
            .all(|page| page.expect("control page read succeeds") == 100)
    );
}
