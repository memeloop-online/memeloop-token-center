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
