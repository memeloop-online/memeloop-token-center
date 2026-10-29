use super::*;

async fn archive_upgrade_preserves_142(url: &str, migrations: &[Migration]) {
    sqlx::any::install_default_drivers();
    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .unwrap();
    let mut transaction = pool.begin().await.unwrap();
    sqlx::query("CREATE TEMP TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)")
        .execute(&mut *transaction).await.unwrap();
    sqlx::query("INSERT INTO schema_migrations VALUES (142, 'plaintext state instead of sealed envelopes', 123456)")
        .execute(&mut *transaction).await.unwrap();
    assert!(migrations.iter().all(|migration| migration.version != 142));
    for schema in [
        include_str!("../../../migrations/common/0071_response_archive_spool.sql"),
        include_str!("../../../migrations/common/0080_request_archive_spool.sql"),
        include_str!("../../../migrations/common/0106_archive_budget_reservations.sql"),
        include_str!("../../../migrations/common/0112_request_archive_gap_evidence.sql"),
    ] {
        let schema = schema.replace("CREATE TABLE ", "CREATE TEMP TABLE ");
        sqlx::raw_sql(sqlx::AssertSqlSafe(schema))
            .execute(&mut *transaction)
            .await
            .unwrap();
    }
    for purpose in ["request", "response"] {
        let insert = format!(
            "INSERT INTO {purpose}_archive_spools (request_id, tenant_id, reservation_id, state, chunk_count, byte_count, cipher_bytes, attempts, next_attempt_at, created_at, updated_at, expires_at, lease_owner, lease_token, lease_expires_at) VALUES ($1, 'tenant', 'reservation', 'uploading', 1, 4, 2048, 10, 0, 1, 2, 999999, 'writer', 'token', 333)"
        );
        sqlx::query(sqlx::AssertSqlSafe(insert))
            .bind(purpose)
            .execute(&mut *transaction)
            .await
            .unwrap();
        let insert = format!(
            "INSERT INTO {purpose}_archive_spool_chunks VALUES ($1, 0, 'original-ciphertext', 4)"
        );
        sqlx::query(sqlx::AssertSqlSafe(insert))
            .bind(purpose)
            .execute(&mut *transaction)
            .await
            .unwrap();
    }
    sqlx::query(
        "UPDATE response_archive_spool_budget SET cipher_bytes = 4096, request_cipher_bytes = 2048",
    )
    .execute(&mut *transaction)
    .await
    .unwrap();
    for _ in 0..2 {
        apply_migration_range(&mut transaction, migrations, 143, 143)
            .await
            .unwrap();
    }
    let marker = sqlx::query("SELECT name, applied_at FROM schema_migrations WHERE version = 142")
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
    assert_eq!(
        marker.try_get::<String, _>("name").unwrap(),
        "plaintext state instead of sealed envelopes"
    );
    assert_eq!(marker.try_get::<i64, _>("applied_at").unwrap(), 123456);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations WHERE version = 143")
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
    assert_eq!(count, 1);
    for purpose in ["request", "response"] {
        let select = format!(
            "SELECT s.lease_owner, s.lease_token, s.lease_expires_at, s.attempts, s.cipher_bytes, c.ciphertext FROM {purpose}_archive_spools s JOIN {purpose}_archive_spool_chunks c ON c.request_id = s.request_id WHERE s.request_id = $1"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(select))
            .bind(purpose)
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
        assert_eq!(row.try_get::<String, _>("lease_owner").unwrap(), "writer");
        assert_eq!(row.try_get::<String, _>("lease_token").unwrap(), "token");
        assert_eq!(row.try_get::<i64, _>("lease_expires_at").unwrap(), 333);
        assert_eq!(row.try_get::<i64, _>("attempts").unwrap(), 10);
        assert_eq!(row.try_get::<i64, _>("cipher_bytes").unwrap(), 2048);
        assert_eq!(
            row.try_get::<String, _>("ciphertext").unwrap(),
            "original-ciphertext"
        );
        let update = format!(
            "UPDATE {purpose}_archive_spools SET attempts = 11, last_error_code = 'upload_timeout', cipher_bytes = 268435457 WHERE request_id = $1"
        );
        sqlx::query(sqlx::AssertSqlSafe(update))
            .bind(purpose)
            .execute(&mut *transaction)
            .await
            .unwrap();
    }
    let budget =
        sqlx::query("SELECT cipher_bytes, request_cipher_bytes FROM response_archive_spool_budget")
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
    assert_eq!(budget.try_get::<i64, _>("cipher_bytes").unwrap(), 4096);
    assert_eq!(
        budget.try_get::<i64, _>("request_cipher_bytes").unwrap(),
        2048
    );
    sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = 536870914, request_cipher_bytes = 268435457")
        .execute(&mut *transaction).await.unwrap();
    transaction.rollback().await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn sqlite_archive_upgrade_preserves_plaintext_142_and_pending_bytes() {
    archive_upgrade_preserves_142("sqlite::memory:", SQLITE_MIGRATIONS).await;
}

#[tokio::test]
async fn postgres_archive_upgrade_preserves_plaintext_142_and_pending_bytes() {
    let url = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL CI fixture is required");
    archive_upgrade_preserves_142(&url, POSTGRES_MIGRATIONS).await;
}
