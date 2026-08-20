use memeloop_token_center::db::{Database, unix_millis};
use rust_decimal::Decimal;
use sqlx::AnyPool;
use uuid::Uuid;

async fn exercise_imported_account_without_usage_state(database_url: &str, label: &str) {
    sqlx::any::install_default_drivers();
    let database = Database::connect(database_url)
        .await
        .expect("connect imported-account database");
    database.migrate().await.expect("migrate database");
    let pool = AnyPool::connect(database_url)
        .await
        .expect("connect imported-account fixture pool");

    let unique = Uuid::now_v7();
    let tenant_id = Uuid::now_v7();
    let principal_id = Uuid::now_v7();
    let account_id = Uuid::now_v7();
    let now = unix_millis();
    sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3)")
        .bind(tenant_id.to_string())
        .bind(format!("imported-ledger-{label}-{unique}"))
        .bind(now)
        .execute(&pool)
        .await
        .expect("insert imported tenant");
    sqlx::query(
        "INSERT INTO principals (id, tenant_id, external_id, created_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(principal_id.to_string())
    .bind(tenant_id.to_string())
    .bind("cpamp-import")
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert imported principal");
    sqlx::query(
        "INSERT INTO credit_accounts (id, tenant_id, principal_id, currency, available_micros, reserved_micros, created_at, updated_at) VALUES ($1, $2, $3, 'USD', 0, 0, $4, $4)",
    )
    .bind(account_id.to_string())
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert imported account without usage state");

    let initial_state: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM account_usage_state WHERE account_id = $1")
            .bind(account_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("count absent usage state");
    assert_eq!(initial_state, 0);

    let grant_key = format!("imported-ledger:{label}:{unique}:grant");
    for _ in 0..2 {
        assert_eq!(
            database
                .grant(account_id, Decimal::from(3), "cpamp-import", &grant_key)
                .await
                .expect("grant imported account"),
            "3"
        );
    }
    let after_grant: (i64, i64, i64) = sqlx::query_as(
        "SELECT a.available_micros, (SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = a.id), (SELECT COUNT(*) FROM ledger_entries WHERE account_id = a.id AND kind = 'grant') FROM credit_accounts a WHERE a.id = $1",
    )
    .bind(account_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("read repaired imported account after grant replay");
    assert_eq!(after_grant, (3_000_000, 0, 1));

    // Exercise reverse independently of grant's repair. A legacy process or a
    // partial pre-v22 import may leave the state absent again.
    sqlx::query("DELETE FROM account_usage_state WHERE account_id = $1")
        .bind(account_id.to_string())
        .execute(&pool)
        .await
        .expect("remove usage state before reversal");
    let reversal_key = format!("imported-ledger:{label}:{unique}:reversal");
    for _ in 0..2 {
        assert_eq!(
            database
                .reverse_grant(
                    account_id,
                    &grant_key,
                    "cpamp-import-reversal",
                    &reversal_key,
                )
                .await
                .expect("reverse imported-account grant"),
            "3"
        );
    }
    let after_reversal: (i64, i64, i64) = sqlx::query_as(
        "SELECT a.available_micros, (SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = a.id), (SELECT COUNT(*) FROM ledger_entries WHERE account_id = a.id AND kind = 'grant_reversal') FROM credit_accounts a WHERE a.id = $1",
    )
    .bind(account_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("read repaired imported account after reversal replay");
    assert_eq!(after_reversal, (0, 0, 1));
}

#[tokio::test]
async fn sqlite_imported_account_grant_replay_and_reverse_repair_usage_state() {
    let directory = tempfile::tempdir().expect("SQLite imported-account directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("imported-ledger.db").display()
    );
    exercise_imported_account_without_usage_state(&database_url, "sqlite").await;
}

#[tokio::test]
async fn postgres_imported_account_grant_replay_and_reverse_repair_usage_state() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!(
            "MTC_TEST_POSTGRES_URL is unset; skipping PostgreSQL imported-account ledger test"
        );
        return;
    };
    exercise_imported_account_without_usage_state(&database_url, "postgres").await;
}
