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
    assert!(versions.contains(&115));
    assert!(versions.contains(&118));
    assert!(versions.contains(&119));
    assert!(versions.contains(&120));
    assert!(versions.contains(&121));
    for statement in [
        "DROP TABLE conversation_semantic_payloads",
        "ALTER TABLE conversation_projection_outbox DROP COLUMN semantic_payload_snapshot_json",
        "DROP TABLE terminal_projection_outbox",
        "DROP TABLE observability_prune_boundaries",
        "ALTER TABLE conversation_projection_outbox DROP COLUMN key_snapshot_json",
        "ALTER TABLE conversation_projection_outbox DROP COLUMN statistics_outcome",
        "ALTER TABLE conversation_projection_outbox DROP COLUMN semantic_snapshot_json",
        "ALTER TABLE conversation_projection_outbox DROP COLUMN terminal_lease_owner",
        "ALTER TABLE conversation_projection_outbox DROP COLUMN terminal_lease_expires_at",
        "DROP TABLE transport_proxy_bindings",
        "DROP TABLE transport_proxy_groups",
        "DROP TABLE transport_proxy_management_audit",
        "DROP TABLE transport_proxy_management_lock",
    ] {
        sqlx::query(statement)
            .execute(&database.pool)
            .await
            .unwrap();
    }
    sqlx::query("DROP TABLE upstream_transport_proxy_selections")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE request_records DROP COLUMN terminal_cause_code")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE request_records DROP COLUMN upstream_model")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE upstream_credentials DROP COLUMN oauth_refresh_diagnostic_json")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM schema_migrations WHERE version IN (115, 116, 117, 118, 119, 120, 121)")
        .execute(&database.pool)
        .await
        .unwrap();
    let maximum: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_migrations")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(maximum, 113);
    let migrations = match database.backend {
        DatabaseBackend::PostgreSql => POSTGRES_MIGRATIONS,
        DatabaseBackend::Sqlite => SQLITE_MIGRATIONS,
    };
    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, migrations, 116, 116)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
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
            sql: include_str!("../../../migrations/common/0115_request_terminal_cause_code.sql"),
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
    let tail: Vec<i64> = sqlx::query_scalar(
        "SELECT version FROM schema_migrations WHERE version >= 117 ORDER BY version",
    )
    .fetch_all(&database.pool)
    .await
    .unwrap();
    assert_eq!(tail, vec![117, 118, 119, 120]);
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

#[test]
fn terminal_projection_migration_precedes_routing_snapshot_in_both_registries() {
    for migrations in [SQLITE_MIGRATIONS, POSTGRES_MIGRATIONS] {
        let tail: Vec<i64> = migrations
            .iter()
            .filter(|migration| migration.version >= 117)
            .map(|migration| migration.version)
            .collect();
        assert_eq!(tail, vec![117, 118, 119, 120]);
    }
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
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&administrative.pool)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&url).unwrap();
    isolated.set_path(&name);
    let database = Database::connect(isolated.as_str()).await.unwrap();
    upgrade_contract(&database).await;
    database.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE {name}")))
        .execute(&administrative.pool)
        .await
        .unwrap();
}
