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
    let tenant = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, 0)")
        .bind(&tenant)
        .bind(&tenant)
        .execute(&database.pool)
        .await
        .unwrap();
    let group = database
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: tenant.clone(),
                name: "pre-126".into(),
            },
        )
        .await
        .unwrap();
    let (route, _) = database
        .create_routed_model_route(CreateRoutedModelRouteInput {
            tenant_external_id: tenant.clone(),
            public_model: "pre-126".into(),
            upstream_model: "pre-126".into(),
            protocol: "openai".into(),
            priority: 0,
            enabled: false,
            upstream_account_ids: vec![],
            included_provider_group_ids: vec![group.id],
            excluded_provider_group_ids: vec![],
            route_group_ids: vec![],
            route_group_names: vec![],
            granted_credential_ids: vec![],
            custom_model_confirmed: false,
        })
        .await
        .unwrap();
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
    assert!(versions.contains(&125));
    assert!(versions.contains(&126));
    for statement in [
        "DROP TABLE session_generated_titles",
        "DROP TABLE session_response_summaries",
        "DROP VIEW model_route_eligible_upstream_accounts",
        "DROP TABLE model_route_group_catalog_evidence",
        "ALTER TABLE model_route_included_provider_groups DROP COLUMN catalog_policy",
    ] {
        sqlx::query(statement)
            .execute(&database.pool)
            .await
            .unwrap();
    }
    let (_, original_view) =
        include_str!("../../../migrations/common/0046_route_catalog_policy.sql")
            .split_once("CREATE VIEW ")
            .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE VIEW {original_view}")))
        .execute(&database.pool)
        .await
        .unwrap();
    for statement in [
        "ALTER TABLE usage_reservations DROP COLUMN tpm_window_applied",
        "DROP TABLE proxy_operational_terminals",
        "DROP TABLE application_plugin_release_resolutions",
        "DROP TABLE plugin_installer_network_snapshots",
        "DROP TABLE plugin_installer_network_preferences",
        "DROP TABLE pending_proxy_settlements",
        "ALTER TABLE request_records DROP COLUMN proxy_owner_token",
        "ALTER TABLE request_records DROP COLUMN proxy_owner_expires_at",
        "ALTER TABLE request_records DROP COLUMN cache_known_input_tokens",
        "ALTER TABLE request_records DROP COLUMN cache_known_read_tokens",
        "ALTER TABLE request_stats_facts DROP COLUMN cache_known_input_tokens",
        "ALTER TABLE request_stats_facts DROP COLUMN cache_known_read_tokens",
        "ALTER TABLE request_daily_aggregates DROP COLUMN cache_known_input_tokens",
        "ALTER TABLE request_daily_aggregates DROP COLUMN cache_known_read_tokens",
        "ALTER TABLE request_daily_aggregates DROP COLUMN cache_eligible_requests",
        "ALTER TABLE request_daily_aggregates DROP COLUMN cache_unknown_requests",
        "ALTER TABLE request_daily_aggregates DROP COLUMN cache_reported_read_tokens",
        "ALTER TABLE request_daily_aggregates DROP COLUMN cache_reported_requests",
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
        "DROP TABLE upstream_service_probes",
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
    sqlx::query("ALTER TABLE upstream_accounts DROP COLUMN notes")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE request_egress_attempts")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE request_records DROP COLUMN in_flight_stage")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE conversation_media_parts")
        .execute(&database.pool)
        .await
        .unwrap();
    for statement in [
        "DROP INDEX IF EXISTS session_archive_unlinked_recent_idx",
        "DROP INDEX IF EXISTS request_egress_attempts_request_idx",
        "DROP INDEX IF EXISTS conversation_observations_compacted_probe_idx",
    ] {
        sqlx::query(statement)
            .execute(&database.pool)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM schema_migrations WHERE version >= 115")
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
    assert_eq!(
        tail,
        migrations
            .iter()
            .filter(|migration| migration.version >= 117)
            .map(|migration| migration.version)
            .collect::<Vec<_>>()
    );
    let policy: String = sqlx::query_scalar(
        "SELECT catalog_policy FROM model_route_included_provider_groups WHERE model_route_id = $1",
    )
    .bind(route.id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(policy, "required");
    let routing = database.route_routing(route.id, &tenant).await.unwrap();
    assert!(!routing.custom_model_confirmed);
    assert_eq!(routing.included_provider_group_ids, vec![group.id]);
    assert!(routing.candidate_upstream_account_ids.is_empty());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upstream_transport_proxy_selections")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("SELECT terminal_cause_code, proxy_owner_token, proxy_owner_expires_at FROM request_records LIMIT 1")
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
        assert_eq!(
            tail,
            vec![
                117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132,
                133, 134, 135, 136, 137, 138, 139, 140, 141, 143, 144, 145
            ]
        );
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
    let fixture = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    let database = Database::connect(fixture.url()).await.unwrap();
    upgrade_contract(&database).await;
    database.close().await;
    fixture.cleanup().await;
}
