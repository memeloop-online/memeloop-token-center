use super::*;
use crate::db::{Database, test_support::PostgresTestDatabase};

#[tokio::test]
async fn postgres_capability_163_skips_hot_ddl_and_preserves_true_ledger() {
    let url =
        std::env::var("MTC_TEST_POSTGRES_URL").expect("PG capability contract requires PostgreSQL");
    let isolated = PostgresTestDatabase::create(&url).await;
    let db = Database::connect_with_max(isolated.url(), 6).await.unwrap();
    db.migrate().await.unwrap();
    db.convert_sealed_state(b"postgres-runtime-test-pepper")
        .await
        .unwrap();
    db.close().await;
    let db = Database::connect_with_max(isolated.url(), 6).await.unwrap();
    let mut receipt_mutation = db.pool.begin().await.unwrap();
    sqlx::query("UPDATE schema_migrations SET name='unrecognized conversion' WHERE version=142")
        .execute(&mut *receipt_mutation)
        .await
        .unwrap();
    assert!(
        super::catalog::inspect(&mut receipt_mutation, CapabilityRole::Gateway)
            .await
            .is_err()
    );
    receipt_mutation.rollback().await.unwrap();
    let modern = db
        .verify_capabilities(CapabilityRole::Gateway)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(modern.track, SchemaTrack::Linear166);
    for mutation in [
        "ALTER TABLE request_records ALTER COLUMN started_event_pending SET DEFAULT 2",
        "DROP INDEX request_records_started_pending",
        "DELETE FROM schema_migrations WHERE version=100",
    ] {
        let mut tx = db.pool.begin().await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(mutation))
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(
            super::catalog::inspect(&mut tx, CapabilityRole::Gateway)
                .await
                .is_err(),
            "mutated catalog must be rejected: {mutation}"
        );
        tx.rollback().await.unwrap();
    }

    sqlx::raw_sql("DROP TABLE upstream_oauth_rejected_generations; DELETE FROM schema_migrations WHERE version=166; ALTER TABLE request_history_baseline_runs DROP COLUMN read_model_version,DROP COLUMN activation_epoch,DROP COLUMN chunk_count,DROP COLUMN manifest_hash,DROP COLUMN sources_complete,DROP COLUMN parity_json,DROP COLUMN target_identity; DELETE FROM schema_migrations WHERE version=165;")
        .execute(&db.pool).await.unwrap();
    let db = Database::connect_with_max(isolated.url(), 6).await.unwrap();
    assert_eq!(
        db.verify_capabilities(CapabilityRole::Worker)
            .await
            .unwrap()
            .unwrap()
            .track,
        SchemaTrack::Linear164
    );
    sqlx::raw_sql("DROP INDEX request_records_started_pending; ALTER TABLE request_records DROP COLUMN started_event_pending; DELETE FROM schema_migrations WHERE version=164;")
        .execute(&db.pool).await.unwrap();
    let db = Database::connect_with_max(isolated.url(), 6).await.unwrap();
    let report = db
        .verify_capabilities(CapabilityRole::Gateway)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.track, SchemaTrack::Legacy163);
    let relation:String=sqlx::query_scalar("SELECT relid::regclass::text FROM pg_partition_tree('request_records') WHERE isleaf LIMIT 1").fetch_one(&db.pool).await.unwrap();
    let mut holder = db.pool.begin().await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "LOCK TABLE {relation} IN ROW EXCLUSIVE MODE"
    )))
    .execute(&mut *holder)
    .await
    .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        db.validate_legacy163_startup(CapabilityRole::Gateway),
    )
    .await
    .expect("legacy planner must never attempt recursive exclusive DDL")
    .unwrap();
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM schema_migrations ORDER BY version")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(versions.last(), Some(&163));
    let marker:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_attribute WHERE attrelid='request_records'::regclass AND attname='started_event_pending' AND NOT attisdropped)").fetch_one(&db.pool).await.unwrap();
    assert!(!marker);
    db.publish_pending_proxy_started_events().await.unwrap();
    holder.rollback().await.unwrap();
    // Unknown partial objects cannot be silently promoted into capabilities.
    sqlx::query("ALTER TABLE request_records ADD COLUMN started_event_pending BIGINT")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        db.verify_capabilities(CapabilityRole::Gateway)
            .await
            .is_err()
    );
    let rows = sqlx::query("SELECT version,name FROM schema_migrations WHERE version>=164")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert!(rows.is_empty());
    sqlx::query("ALTER TABLE request_records DROP COLUMN started_event_pending")
        .execute(&db.pool)
        .await
        .unwrap();
    // Genuine interrupted online164 state: column committed, no ledger164.
    sqlx::query(
        "ALTER TABLE request_records ADD COLUMN started_event_pending BIGINT NOT NULL DEFAULT 0",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let db = Database::connect_with_max(isolated.url(), 6).await.unwrap();
    db.migrate().await.unwrap();
    assert_eq!(
        db.verify_capabilities(CapabilityRole::Worker)
            .await
            .unwrap()
            .unwrap()
            .track,
        SchemaTrack::Linear166
    );
}

#[tokio::test]
async fn postgres_capability_reader_permissions_do_not_authorize_gateway_writes() {
    let url =
        std::env::var("MTC_TEST_POSTGRES_URL").expect("PG capability contract requires PostgreSQL");
    let isolated = PostgresTestDatabase::create(&url).await;
    let db = Database::connect_with_max(isolated.url(), 6).await.unwrap();
    db.migrate().await.unwrap();
    let role = format!("capability_reader_{}", uuid::Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE ROLE {role}")))
        .execute(&db.pool)
        .await
        .unwrap();
    let schema: String = sqlx::query_scalar("SELECT current_schema()::text")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("GRANT USAGE ON SCHEMA {schema} TO {role}; GRANT SELECT ON ALL TABLES IN SCHEMA {schema} TO {role}"))).execute(&db.pool).await.unwrap();
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("SET LOCAL ROLE {role}")))
        .execute(&mut *tx)
        .await
        .unwrap();
    let reader = super::catalog::inspect(&mut tx, CapabilityRole::HistoryReader)
        .await
        .unwrap()
        .unwrap();
    reader.require("history.baseline_receipt.v1").unwrap();
    assert!(
        super::catalog::inspect(&mut tx, CapabilityRole::Gateway)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    for capability_role in [
        CapabilityRole::Gateway,
        CapabilityRole::Worker,
        CapabilityRole::Control,
        CapabilityRole::Maintenance,
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "REVOKE ALL ON ALL TABLES IN SCHEMA {schema} FROM {role}"
        )))
        .execute(&db.pool)
        .await
        .unwrap();
        // Ledger identity is always read, independent of lifecycle operations.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "GRANT SELECT ON schema_migrations TO {role}"
        )))
        .execute(&db.pool)
        .await
        .unwrap();
        let requirements = super::permissions::requirements_at(capability_role, 166);
        for requirement in &requirements {
            let operations = requirement.operations.join(",");
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "GRANT {operations} ON {} TO {role}",
                requirement.table
            )))
            .execute(&db.pool)
            .await
            .unwrap();
        }
        let mut tx = db.pool.begin().await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("SET LOCAL ROLE {role}")))
            .execute(&mut *tx)
            .await
            .unwrap();
        super::catalog::inspect(&mut tx, capability_role)
            .await
            .unwrap()
            .unwrap();
        tx.rollback().await.unwrap();
        let required = &requirements[0];
        let operation = required.operations[0];
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "REVOKE {operation} ON {} FROM {role}",
            required.table
        )))
        .execute(&db.pool)
        .await
        .unwrap();
        let mut tx = db.pool.begin().await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("SET LOCAL ROLE {role}")))
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(
            super::catalog::inspect(&mut tx, capability_role)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
    }
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP OWNED BY {role}; DROP ROLE {role}"
    )))
    .execute(&db.pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn postgres_capability_startup_rejects_absent_and_below_floor_ledger() {
    let url = std::env::var("MTC_TEST_POSTGRES_URL").expect("PG contract requires PostgreSQL");
    let isolated = PostgresTestDatabase::create(&url).await;
    let db = Database::connect_with_max(isolated.url(), 2).await.unwrap();
    assert!(
        db.verify_startup_capabilities(CapabilityRole::Gateway)
            .await
            .is_err()
    );
    sqlx::raw_sql("CREATE TABLE schema_migrations(version BIGINT PRIMARY KEY,name TEXT,applied_at BIGINT); INSERT INTO schema_migrations VALUES(162,'old',0)").execute(&db.pool).await.unwrap();
    assert!(
        db.verify_startup_capabilities(CapabilityRole::Gateway)
            .await
            .is_err()
    );
}
