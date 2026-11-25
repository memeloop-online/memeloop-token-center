use super::*;
use crate::db::{Database, test_support::PostgresTestDatabase};

#[tokio::test]
async fn postgres_capability_163_skips_hot_ddl_and_preserves_true_ledger() {
    let url =
        std::env::var("MTC_TEST_POSTGRES_URL").expect("PG capability contract requires PostgreSQL");
    let isolated = PostgresTestDatabase::create(&url).await;
    let db = Database::connect_with_max(isolated.url(), 6).await.unwrap();
    db.migrate().await.unwrap();
    let modern = db
        .verify_capabilities(CapabilityRole::Gateway)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(modern.track, SchemaTrack::Linear165);
    sqlx::raw_sql("ALTER TABLE request_history_baseline_runs DROP COLUMN read_model_version,DROP COLUMN activation_epoch,DROP COLUMN chunk_count,DROP COLUMN manifest_hash,DROP COLUMN sources_complete,DROP COLUMN parity_json,DROP COLUMN target_identity; DELETE FROM schema_migrations WHERE version=165;")
        .execute(&db.pool).await.unwrap();
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
    tokio::time::timeout(std::time::Duration::from_secs(5), db.migrate())
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
}
