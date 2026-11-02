use super::*;
use serde_json::json;

async fn cleanup_contract(url: &str, migrations: &[Migration]) {
    sqlx::any::install_default_drivers();
    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .unwrap();
    let mut transaction = pool.begin().await.unwrap();
    for statement in [
        "CREATE TEMP TABLE upstream_accounts (id TEXT PRIMARY KEY, config_json TEXT NOT NULL)",
        "CREATE TEMP TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
    ] {
        sqlx::query(statement)
            .execute(&mut *transaction)
            .await
            .unwrap();
    }
    let cases = [
        (
            json!({"transport_policy":{"failover_deadline_millis":300000,"connect_timeout_millis":5000,"read_timeout_millis":null,"request_timeout_millis":20000},"network_scope":"private","other":{"retained":true}}),
            json!({"transport_policy":{"connect_timeout_millis":5000,"read_timeout_millis":null,"request_timeout_millis":20000},"network_scope":"private","other":{"retained":true}}),
        ),
        (
            json!({"transport_policy":{"failover_deadline_millis":null}}),
            json!({"transport_policy":{}}),
        ),
        (
            json!({"transport_policy":{"candidate_attempts":2}}),
            json!({"transport_policy":{"candidate_attempts":2}}),
        ),
        (
            json!({"transport_policy":null}),
            json!({"transport_policy":null}),
        ),
        (json!({"other":"unchanged"}), json!({"other":"unchanged"})),
    ];
    for (index, (original, _)) in cases.iter().enumerate() {
        sqlx::query("INSERT INTO upstream_accounts (id, config_json) VALUES ($1, $2)")
            .bind(index.to_string())
            .bind(original.to_string())
            .execute(&mut *transaction)
            .await
            .unwrap();
    }
    apply_migration_range(&mut transaction, migrations, 144, 144)
        .await
        .unwrap();
    apply_migration_range(&mut transaction, migrations, 144, 144)
        .await
        .unwrap();
    for (index, (_, expected)) in cases.iter().enumerate() {
        let stored: String =
            sqlx::query_scalar("SELECT config_json FROM upstream_accounts WHERE id = $1")
                .bind(index.to_string())
                .fetch_one(&mut *transaction)
                .await
                .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&stored).unwrap(), *expected);
    }
    let versions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations WHERE version = 144")
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
    assert_eq!(versions, 1);
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn sqlite_transport_policy_upgrade_preserves_explicit_windows() {
    cleanup_contract("sqlite::memory:", SQLITE_MIGRATIONS).await;
}

#[tokio::test]
async fn postgres_transport_policy_upgrade_preserves_explicit_windows() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    cleanup_contract(&url, POSTGRES_MIGRATIONS).await;
}
