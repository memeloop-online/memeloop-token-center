//! Observe actual relation locks held by the production candidate transaction.
use super::*;

async fn locked_request_children(fixture: &PgFixture, pid: i32) -> Vec<String> {
    sqlx::query_scalar("SELECT DISTINCT c.relname FROM pg_locks l JOIN pg_class c ON c.oid=l.relation JOIN pg_namespace n ON n.oid=c.relnamespace WHERE l.pid=$1 AND l.granted AND n.nspname=$2 AND c.oid IN (SELECT relid FROM pg_partition_tree('request_records'::regclass) WHERE isleaf) ORDER BY c.relname")
        .bind(pid).bind(&fixture.schema).fetch_all(&fixture.db.pool).await.unwrap()
}

#[tokio::test]
async fn postgres_spool_candidate_prunes_unrelated_request_partition_locks() {
    for purpose in [
        BufferedArchivePurpose::Response,
        BufferedArchivePurpose::Request,
    ] {
        verify_partition_claim(purpose).await;
    }
}

async fn verify_partition_claim(purpose: BufferedArchivePurpose) {
    let Some(fixture) = PgFixture::new().await else {
        return;
    };
    // DDL is committed before either measured transaction and visible to the
    // independent observer. The seeded request at time 1 remains in DEFAULT.
    sqlx::raw_sql("CREATE TABLE request_records_unrelated_early PARTITION OF request_records FOR VALUES FROM (-200) TO (-100); CREATE TABLE request_records_unrelated_late PARTITION OF request_records FOR VALUES FROM (100) TO (200);")
        .execute(&fixture.db.pool).await.unwrap();
    fixture.capture().await;
    fixture
        .db
        .seal_response_archive_spool(fixture.id, 1, 1)
        .await
        .unwrap();
    if purpose == BufferedArchivePurpose::Request {
        sqlx::query("INSERT INTO request_archive_spools(request_id,tenant_id,reservation_id,state,chunk_count,byte_count,cipher_bytes,attempts,next_attempt_at,created_at,updated_at,expires_at) SELECT request_id,tenant_id,reservation_id,state,chunk_count,byte_count,cipher_bytes,attempts,next_attempt_at,created_at,updated_at,expires_at FROM response_archive_spools WHERE request_id=$1")
            .bind(fixture.id.request_id.to_string()).execute(&fixture.db.pool).await.unwrap();
    }
    let now: i64 = sqlx::query_scalar(
        "SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT)",
    )
    .fetch_one(&fixture.db.pool)
    .await
    .unwrap();
    let mut old = fixture.db.pool.begin().await.unwrap();
    let old_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *old)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "SELECT s.request_id FROM response_archive_spools s JOIN request_records r ON r.id=s.request_id AND r.tenant_id=s.tenant_id AND r.reservation_id=s.reservation_id WHERE s.request_id=$1 FOR UPDATE OF s")))
        .bind(fixture.id.request_id.to_string()).fetch_one(&mut *old).await.unwrap();
    let old_locks = locked_request_children(&fixture, old_pid).await;
    assert!(
        old_locks
            .iter()
            .any(|name| name == "request_records_unrelated_early")
    );
    assert!(
        old_locks
            .iter()
            .any(|name| name == "request_records_unrelated_late")
    );
    old.rollback().await.unwrap();
    let (mut tx, row) = fixture
        .db
        .partitioned_archive_spool_candidate(purpose, now)
        .await
        .unwrap()
        .expect("eligible spool");
    assert_eq!(
        row.try_get::<String, _>("request_id").unwrap(),
        fixture.id.request_id.to_string()
    );
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let locks = locked_request_children(&fixture, pid).await;
    assert!(!locks.is_empty());
    assert!(
        !locks
            .iter()
            .any(|name| name.starts_with("request_records_unrelated_")),
        "{locks:?}"
    );
    eprintln!(
        "SPOOL_PARTITION_LOCKS old={} new={}",
        old_locks.len(),
        locks.len()
    );
    assert!(
        fixture
            .db
            .partitioned_archive_spool_candidate(purpose, now)
            .await
            .unwrap()
            .is_none(),
        "locked task is skipped"
    );
    tx.rollback().await.unwrap();
    // A locator with the wrong key must not fall back to a parent-wide read.
    let key: String = sqlx::query_scalar("SELECT key_id FROM request_record_locators WHERE id=$1")
        .bind(fixture.id.request_id.to_string())
        .fetch_one(&fixture.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE request_record_locators SET key_id=$1 WHERE id=$2")
        .bind(Uuid::new_v4().to_string())
        .bind(fixture.id.request_id.to_string())
        .execute(&fixture.db.pool)
        .await
        .unwrap();
    assert!(
        fixture
            .db
            .partitioned_archive_spool_candidate(purpose, now)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE request_record_locators SET key_id=$1 WHERE id=$2")
        .bind(key)
        .bind(fixture.id.request_id.to_string())
        .execute(&fixture.db.pool)
        .await
        .unwrap();
    let (tx, _) = fixture
        .db
        .partitioned_archive_spool_candidate(purpose, now)
        .await
        .unwrap()
        .expect("rollback preserves eligibility for retry");
    tx.rollback().await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(spool_sql(
        purpose,
        "UPDATE response_archive_spools SET next_attempt_at=$1 WHERE request_id=$2",
    )))
    .bind(now + 60_000)
    .bind(fixture.id.request_id.to_string())
    .execute(&fixture.db.pool)
    .await
    .unwrap();
    assert!(
        fixture
            .db
            .partitioned_archive_spool_candidate(purpose, now)
            .await
            .unwrap()
            .is_none(),
        "future task remains ineligible"
    );
    fixture.finish().await;
}
