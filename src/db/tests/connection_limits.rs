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
