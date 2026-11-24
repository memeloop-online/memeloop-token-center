use super::*;

#[tokio::test]
async fn postgres164_nowait_preserves_frontend_and_resumes_partition_indexes() {
    let url = std::env::var("MTC_TEST_POSTGRES_URL").expect("real PG contract requires PostgreSQL");
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("online164_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut scoped = url::Url::parse(&url).unwrap();
    scoped
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let db = Database::connect_for_migration(scoped.as_str(), 5)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TABLE schema_migrations(version BIGINT PRIMARY KEY,name TEXT,applied_at BIGINT); INSERT INTO schema_migrations VALUES(163,'history_event_backfill_cursor',123); CREATE TABLE request_records(created_at BIGINT NOT NULL,id TEXT NOT NULL) PARTITION BY RANGE(created_at); CREATE TABLE request_records_default PARTITION OF request_records DEFAULT;")
        .execute(&db.pool).await.unwrap();
    for n in 0..24 {
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TABLE request_records_p{n} PARTITION OF request_records FOR VALUES FROM ({}) TO ({})",n*100,(n+1)*100))).execute(&db.pool).await.unwrap();
    }
    sqlx::query("INSERT INTO request_records VALUES(1,'before'),(9999,'default')")
        .execute(&db.pool)
        .await
        .unwrap();
    let mut holder = db.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE request_records_p0 IN ACCESS SHARE MODE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let mut c = db.pool.acquire().await.unwrap();
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        online164::prepare(&mut c),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(
        error.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("55P03")
    );
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        sqlx::query("INSERT INTO request_records VALUES(2,'while-held')")
            .execute(&db.pool)
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_records")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count, 3);
    })
    .await
    .expect("failed NOWAIT must not queue exclusive locks behind holder");
    let absent:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM pg_attribute WHERE attrelid='request_records'::regclass AND attname='started_event_pending')").fetch_one(&db.pool).await.unwrap();
    assert!(absent);
    holder.rollback().await.unwrap();
    online164::prepare(&mut c).await.unwrap();
    online164::prepare(&mut c).await.unwrap();
    let mut tx = c.begin().await.unwrap();
    apply_migration_range(&mut tx, POSTGRES_MIGRATIONS, 164, 164)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations WHERE version=164")
        .fetch_one(&mut *c)
        .await
        .unwrap();
    assert_eq!(count, 1);
    sqlx::query("INSERT INTO request_records(created_at,id,started_event_pending) VALUES(3,'pending',1),(10000,'default-pending',1)").execute(&mut *c).await.unwrap();
    let covered:i64=sqlx::query_scalar("SELECT COUNT(*) FROM pg_inherits WHERE inhparent='request_records_started_pending'::regclass").fetch_one(&mut *c).await.unwrap();
    assert_eq!(covered, 25);
    drop(c);
    db.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}
