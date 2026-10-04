use super::*;

const ROUTE_SWITCH_SQL_PR422_ABA79EFE: &str = r#"
CREATE TABLE ledger_resource_route_switches (
    operation_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    route_id TEXT NOT NULL,
    source_upstream_account_id TEXT NOT NULL,
    target_upstream_account_id TEXT NOT NULL,
    expected_route_updated_at BIGINT NOT NULL,
    expected_source_updated_at BIGINT NOT NULL,
    expected_target_updated_at BIGINT NOT NULL,
    expected_grant_revision BIGINT NOT NULL,
    before_snapshot_json TEXT NOT NULL,
    after_snapshot_json TEXT NOT NULL,
    status TEXT NOT NULL,
    actor_service_id TEXT,
    created_at BIGINT NOT NULL,
    applied_at BIGINT,
    rolled_back_at BIGINT,
    rollback_updated_at BIGINT,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE RESTRICT,
    CHECK (source_upstream_account_id <> target_upstream_account_id),
    CHECK (expected_route_updated_at >= 0),
    CHECK (expected_source_updated_at >= 0),
    CHECK (expected_target_updated_at >= 0),
    CHECK (expected_grant_revision >= 0),
    CHECK (status IN ('planned', 'applied', 'rolled_back'))
);

CREATE INDEX ledger_resource_route_switches_tenant_created_idx
    ON ledger_resource_route_switches (tenant_id, created_at DESC, operation_id);
"#;

async fn upgrade_contract(database: &Database) {
    database.migrate().await.unwrap();
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM schema_migrations ORDER BY version")
            .fetch_all(&database.pool)
            .await
            .unwrap();
    assert!(versions.contains(&116));
    assert!(!versions.contains(&114));
    assert!(!versions.contains(&115));
    sqlx::query("DROP TABLE upstream_transport_proxy_selections")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM schema_migrations WHERE version = 116")
        .execute(&database.pool)
        .await
        .unwrap();
    let maximum: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_migrations")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(maximum, 113);
    database.migrate().await.unwrap();
    let applied_at: i64 =
        sqlx::query_scalar("SELECT applied_at FROM schema_migrations WHERE version = 116")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    let late = [
        Migration {
            version: 114,
            name: "audited ledger resource route switches",
            sql: ROUTE_SWITCH_SQL_PR422_ABA79EFE,
        },
        Migration {
            version: 115,
            name: "recorded request terminal cause",
            sql: "ALTER TABLE request_records ADD COLUMN terminal_cause_code TEXT;",
        },
    ];
    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, &late, 114, 115)
        .await
        .unwrap();
    apply_migration_range(&mut transaction, &late, 114, 115)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    database.migrate().await.unwrap();
    let replayed_at: i64 =
        sqlx::query_scalar("SELECT applied_at FROM schema_migrations WHERE version = 116")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(applied_at, replayed_at);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upstream_transport_proxy_selections")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("SELECT terminal_cause_code FROM request_records LIMIT 1")
        .fetch_optional(&database.pool)
        .await
        .unwrap();
    sqlx::query("SELECT operation_id FROM ledger_resource_route_switches LIMIT 1")
        .fetch_optional(&database.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn sqlite_proxy_group_116_accepts_empty_113_and_later_114_115() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("proxy-migrations.db").display()
    );
    let database = Database::connect(&url).await.unwrap();
    upgrade_contract(&database).await;
}

#[tokio::test]
async fn postgres_proxy_group_116_accepts_empty_113_and_later_114_115() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let administrative = Database::connect(&url).await.unwrap();
    let name = format!("mtc_proxy_schema_{}", uuid::Uuid::now_v7().simple());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&administrative.pool)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&url).unwrap();
    isolated.set_path(&name);
    let database = Database::connect(isolated.as_str()).await.unwrap();
    upgrade_contract(&database).await;
    database.pool.close().await;
    sqlx::query(&format!("DROP DATABASE {name}"))
        .execute(&administrative.pool)
        .await
        .unwrap();
}
