use super::*;
use std::sync::Arc;

const PEPPER: &[u8] = b"optional prepaid rate limits contract pepper";

async fn credential(
    database: &Database,
    policy: KeyPolicy,
    balance: Decimal,
) -> (AuthenticatedKey, ModelPrice) {
    let unique = Uuid::now_v7().to_string();
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: unique.clone(),
                principal_external_id: "member".into(),
                alias: unique.clone(),
                currency: "USD".into(),
                policy,
                initial_balance: balance,
                idempotency_key: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, PEPPER)
        .await
        .unwrap();
    let price = database
        .upsert_model_price(&unique, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    (key, price)
}

async fn reserve_at(
    database: &Database,
    key: &AuthenticatedKey,
    price: &ModelPrice,
    tokens: i64,
    now: i64,
) -> Result<UsageReservation, AppError> {
    let mut transaction = database.begin_write_transaction().await?;
    let reservation =
        reserve_usage_in_transaction(&mut transaction, key, price, 0, tokens, now).await?;
    transaction.commit().await?;
    Ok(reservation)
}

async fn window_count(database: &Database, key: &AuthenticatedKey) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM rate_limit_windows WHERE key_id = $1")
        .bind(key.key_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap()
}

async fn window_tokens(database: &Database, key: &AuthenticatedKey, now: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT tokens FROM rate_limit_windows WHERE key_id = $1 AND window_start = $2",
    )
    .bind(key.key_id.to_string())
    .bind(now / 60_000 * 60_000)
    .fetch_one(&database.pool)
    .await
    .unwrap()
}

async fn defaults_admit_trusted_codex_ceiling_without_rate_windows(database: &Database) {
    let (key, price) = credential(database, KeyPolicy::default(), Decimal::from(1000)).await;
    assert_eq!(key.policy.enforcement_mode, EnforcementMode::Prepaid);
    let now = unix_millis();
    let barrier = Arc::new(tokio::sync::Barrier::new(65));
    let mut tasks = Vec::new();
    for _ in 0..65 {
        let database = database.clone();
        let key = key.clone();
        let price = price.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            reserve_at(&database, &key, &price, 500_000, now).await
        }));
    }
    let mut reservations = Vec::new();
    for task in tasks {
        let reservation = task.await.unwrap().unwrap();
        assert_eq!(reservation.reserved_tokens, 500_000);
        assert_eq!(reservation.reserved_micros, 500_000);
        reservations.push(reservation);
    }
    let snapshot = database.key_limit_snapshot(key.key_id).await.unwrap();
    assert_eq!(snapshot.available_balance, "967.5");
    assert_eq!(snapshot.reserved_balance, "32.5");
    assert_eq!(snapshot.concurrency.active, 65);
    assert_eq!(window_count(database, &key).await, 0);
    let snapshot = serde_json::to_value(snapshot).unwrap();
    for rate in ["rpm", "tpm"] {
        for field in ["limit", "remaining", "reset_at"] {
            assert!(snapshot[rate][field].is_null());
        }
        assert_eq!(snapshot[rate]["used"], 0);
    }
    assert!(snapshot["concurrency"]["limit"].is_null());
    assert!(snapshot["concurrency"]["remaining"].is_null());
    let applied: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(tpm_window_applied), 0) AS BIGINT) FROM usage_reservations WHERE key_id = $1",
    )
    .bind(key.key_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(applied, 0);

    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut replays = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let reservation = reservations[0].clone();
        let barrier = barrier.clone();
        replays.push(tokio::spawn(async move {
            barrier.wait().await;
            database.settle_usage(&reservation, 20, 30).await
        }));
    }
    for replay in replays {
        assert_eq!(replay.await.unwrap().unwrap(), 50);
    }
    for reservation in &reservations[1..] {
        assert_eq!(database.settle_usage(reservation, 0, 0).await.unwrap(), 0);
    }
    let settled = database.key_limit_snapshot(key.key_id).await.unwrap();
    assert_eq!(settled.available_balance, "999.99995");
    assert_eq!(settled.reserved_balance, "0");
    assert_eq!(settled.concurrency.active, 0);
    assert_eq!(window_count(database, &key).await, 0);
    let ledger = database
        .list_account_ledger(key.account_id, 100)
        .await
        .unwrap();
    let actual: Vec<_> = ledger
        .iter()
        .filter(|entry| entry.kind == "usage" && entry.source == reservations[0].id.to_string())
        .collect();
    assert_eq!(actual.len(), 1);
    assert_eq!(actual[0].amount, "-0.00005");
    assert_eq!(
        ledger.iter().filter(|entry| entry.kind == "usage").count(),
        65
    );
}

async fn optional_rates_do_not_weaken_atomic_prepaid_balance(database: &Database) {
    let (key, price) = credential(database, KeyPolicy::default(), Decimal::ONE).await;
    let now = unix_millis();
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let key = key.clone();
        let price = price.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            reserve_at(&database, &key, &price, 500_000, now).await
        }));
    }
    let mut admitted = Vec::new();
    let mut denied = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(reservation) => admitted.push(reservation),
            Err(AppError::LimitExceeded {
                reason: LimitReason::BalanceExhausted,
                retry_after_seconds: None,
            }) => denied += 1,
            other => panic!("unexpected prepaid admission result: {other:?}"),
        }
    }
    assert_eq!(admitted.len(), 2);
    assert_eq!(denied, 6);
    let held = database.key_limit_snapshot(key.key_id).await.unwrap();
    assert_eq!(held.available_balance, "0");
    assert_eq!(held.reserved_balance, "1");
    assert_eq!(held.concurrency.active, 2);
    assert_eq!(window_count(database, &key).await, 0);
    for reservation in &admitted {
        assert_eq!(
            database
                .settle_usage(reservation, 0, 500_000)
                .await
                .unwrap(),
            500_000
        );
    }
    let settled = database.key_limit_snapshot(key.key_id).await.unwrap();
    assert_eq!(settled.available_balance, "0");
    assert_eq!(settled.reserved_balance, "0");
    assert!(matches!(
        reserve_at(database, &key, &price, 1, now).await,
        Err(AppError::LimitExceeded {
            reason: LimitReason::BalanceExhausted,
            ..
        })
    ));
    assert_eq!(window_count(database, &key).await, 0);
}

async fn explicit_optional_limits_keep_their_atomic_rejections(database: &Database) {
    for (policy, reason, owns_window) in [
        (
            KeyPolicy {
                requests_per_minute: Some(1),
                ..KeyPolicy::default()
            },
            LimitReason::RpmExhausted,
            true,
        ),
        (
            KeyPolicy {
                tokens_per_minute: Some(500_000),
                ..KeyPolicy::default()
            },
            LimitReason::TpmExhausted,
            true,
        ),
        (
            KeyPolicy {
                max_concurrency: Some(1),
                ..KeyPolicy::default()
            },
            LimitReason::ConcurrencyExhausted,
            false,
        ),
        (
            KeyPolicy {
                requests_per_minute: Some(3),
                tokens_per_minute: Some(750_000),
                max_concurrency: Some(2),
                ..KeyPolicy::default()
            },
            LimitReason::TpmExhausted,
            true,
        ),
    ] {
        let (key, price) = credential(database, policy, Decimal::from(1000)).await;
        let now = unix_millis();
        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let database = database.clone();
            let key = key.clone();
            let price = price.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                reserve_at(&database, &key, &price, 500_000, now).await
            }));
        }
        let mut admitted = Vec::new();
        let mut rejected = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(reservation) => admitted.push(reservation),
                Err(AppError::LimitExceeded {
                    reason: actual,
                    retry_after_seconds: Some(retry),
                }) => {
                    assert_eq!(actual, reason);
                    assert!((1..=60).contains(&retry));
                    rejected += 1;
                }
                other => panic!("explicit {reason:?} was not preserved: {other:?}"),
            }
        }
        assert_eq!(admitted.len(), 1);
        assert_eq!(rejected, 7);
        assert_eq!(window_count(database, &key).await, i64::from(owns_window));
        if owns_window {
            let counts: (i64, i64) = sqlx::query_as("SELECT requests, tokens FROM rate_limit_windows WHERE key_id = $1 AND window_start = $2")
                .bind(key.key_id.to_string()).bind(now / 60000 * 60000)
                .fetch_one(&database.pool).await.unwrap();
            assert_eq!(
                counts.0,
                i64::from(key.policy.requests_per_minute.is_some())
            );
            assert_eq!(
                counts.1,
                if key.policy.tokens_per_minute.is_some() {
                    500000
                } else {
                    0
                }
            );
        }
        assert_eq!(
            database
                .key_limit_snapshot(key.key_id)
                .await
                .unwrap()
                .reserved_balance,
            "0.5"
        );
        assert_eq!(
            database
                .settle_usage(&admitted[0], 25_000, 25_000)
                .await
                .unwrap(),
            50_000
        );
        if reason != LimitReason::RpmExhausted {
            let next = reserve_at(database, &key, &price, 450_000, now)
                .await
                .unwrap();
            assert_eq!(database.settle_usage(&next, 0, 0).await.unwrap(), 0);
        }
    }
}

async fn recovered_unset_reservations_never_mutate_a_later_rate_window(
    database: &Database,
    recovered: &Database,
) {
    let (key, price) = credential(database, KeyPolicy::default(), Decimal::from(1000)).await;
    let now = unix_millis();
    let reservation = reserve_at(database, &key, &price, 500_000, now)
        .await
        .unwrap();
    let orphan = reserve_at(database, &key, &price, 500_000, now)
        .await
        .unwrap();
    sqlx::query("UPDATE usage_reservations SET created_at = $1 WHERE id = $2")
        .bind(now.saturating_sub(31 * 60_000))
        .bind(orphan.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    let mut finite_key = key.clone();
    finite_key.policy.tokens_per_minute = Some(100_000);
    database
        .update_key_policy(key.key_id, finite_key.policy.clone())
        .await
        .unwrap();
    let finite = reserve_at(database, &finite_key, &price, 100_000, now)
        .await
        .unwrap();
    assert_eq!(window_tokens(database, &key, now).await, 100_000);
    let mut transaction = recovered.begin_write_transaction().await.unwrap();
    let resized = resize_usage_reservation_in_transaction(
        &mut transaction,
        &finite_key,
        &price,
        &reservation,
        0,
        650_000,
        now,
    )
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    assert_eq!(resized.reserved_tokens, 650_000);
    assert_eq!(resized.reserved_micros, 650_000);
    assert_eq!(window_tokens(database, &key, now).await, 100_000);
    assert_eq!(
        recovered.release_orphaned_reservations(100).await.unwrap(),
        1
    );
    assert_eq!(
        recovered.release_orphaned_reservations(100).await.unwrap(),
        0
    );
    assert_eq!(window_tokens(database, &key, now).await, 100_000);
    assert_eq!(
        recovered
            .settle_usage(&resized, 20_000, 30_000)
            .await
            .unwrap(),
        50_000
    );
    assert_eq!(
        recovered
            .settle_usage(&resized, 20_000, 30_000)
            .await
            .unwrap(),
        50_000
    );
    assert_eq!(window_tokens(database, &key, now).await, 100_000);

    let mut transaction = recovered.begin_write_transaction().await.unwrap();
    let cancelled = reserve_usage_in_transaction(&mut transaction, &key, &price, 0, 500_000, now)
        .await
        .unwrap();
    drop(transaction);
    let persisted: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations WHERE id = $1")
            .bind(cancelled.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(persisted, 0);
    assert_eq!(window_tokens(database, &key, now).await, 100_000);
    database
        .update_key_policy(key.key_id, KeyPolicy::default())
        .await
        .unwrap();
    assert_eq!(recovered.settle_usage(&finite, 0, 100).await.unwrap(), 100);
    assert_eq!(window_tokens(database, &key, now).await, 100);
    let settled = recovered.key_limit_snapshot(key.key_id).await.unwrap();
    assert_eq!(settled.available_balance, "999.9499");
    assert_eq!(settled.reserved_balance, "0");
    assert_eq!(settled.concurrency.active, 0);
    let ledger = recovered
        .list_account_ledger(key.account_id, 100)
        .await
        .unwrap();
    assert_eq!(
        ledger.iter().filter(|entry| entry.kind == "usage").count(),
        3
    );
}

async fn optional_rate_contract(database: &Database, recovered: &Database) {
    rate_window_upgrade_preserves_historic_holds(database).await;
    defaults_admit_trusted_codex_ceiling_without_rate_windows(database).await;
    optional_rates_do_not_weaken_atomic_prepaid_balance(database).await;
    explicit_optional_limits_keep_their_atomic_rejections(database).await;
    recovered_unset_reservations_never_mutate_a_later_rate_window(database, recovered).await;
    let (key, price) = credential(
        database,
        KeyPolicy {
            lifetime_budget: Some("0.1".into()),
            ..KeyPolicy::default()
        },
        Decimal::from(1000),
    )
    .await;
    assert!(matches!(
        database.reserve_usage(&key, &price, 0, 500000).await,
        Err(AppError::LimitExceeded {
            reason: LimitReason::LifetimeBudgetExhausted,
            ..
        })
    ));
    let limits = database.key_limit_snapshot(key.key_id).await.unwrap();
    assert_eq!(limits.available_balance, "1000");
    assert_eq!(limits.reserved_balance, "0");
    assert_eq!(limits.concurrency.active, 0);
    assert_eq!(window_count(database, &key).await, 0);
}

async fn rate_window_upgrade_preserves_historic_holds(database: &Database) {
    let (key, price) = credential(
        database,
        KeyPolicy {
            requests_per_minute: Some(60),
            tokens_per_minute: Some(100000),
            max_concurrency: Some(4),
            ..KeyPolicy::default()
        },
        Decimal::from(1000),
    )
    .await;
    let now = unix_millis();
    let reservation = reserve_at(database, &key, &price, 100000, now)
        .await
        .unwrap();
    let policy_before: String =
        sqlx::query_scalar("SELECT policy_json FROM key_records WHERE id = $1")
            .bind(key.key_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    sqlx::query("ALTER TABLE usage_reservations DROP COLUMN tpm_window_applied")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM schema_migrations WHERE version = 134")
        .execute(&database.pool)
        .await
        .unwrap();
    database.migrate().await.unwrap();
    database.migrate().await.unwrap();
    let migrated: (i64, i64, i64) = sqlx::query_as("SELECT tpm_window_applied, reserved_tokens, reserved_micros FROM usage_reservations WHERE id = $1")
        .bind(reservation.id.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(migrated, (1, 100000, 100000));
    let policy_after: String =
        sqlx::query_scalar("SELECT policy_json FROM key_records WHERE id = $1")
            .bind(key.key_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(policy_before, policy_after);
    assert_eq!(window_tokens(database, &key, now).await, 100000);
    assert_eq!(
        database.settle_usage(&reservation, 0, 123).await.unwrap(),
        123
    );
    assert_eq!(window_tokens(database, &key, now).await, 123);
    let limits = database.key_limit_snapshot(key.key_id).await.unwrap();
    assert_eq!(limits.rpm.limit, Some(60));
    assert_eq!(limits.tpm.limit, Some(100000));
    assert_eq!(limits.concurrency.limit, Some(4));
    assert_eq!(limits.available_balance, "999.999877");
    assert_eq!(limits.reserved_balance, "0");
}

#[tokio::test]
async fn sqlite_optional_rate_limits_preserve_prepaid_finance_and_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("optional-rates.db").display()
    );
    let database = Database::connect_with_max(&url, 8).await.unwrap();
    database.migrate().await.unwrap();
    let recovered = Database::connect_with_max(&url, 8).await.unwrap();
    optional_rate_contract(&database, &recovered).await;
    recovered.close().await;
    database.close().await;
}

#[tokio::test]
async fn postgres_optional_rate_limits_preserve_prepaid_finance_and_recovery() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let fixture = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    let database = Database::connect_with_max(fixture.url(), 8).await.unwrap();
    database.migrate().await.unwrap();
    let recovered = Database::connect_with_max(fixture.url(), 8).await.unwrap();
    optional_rate_contract(&database, &recovered).await;
    recovered.close().await;
    database.close().await;
    fixture.cleanup().await;
}
