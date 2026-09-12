use super::super::super::*;
use super::{SettlementFixture, start};

async fn postgres_fixture(database_url: &str) -> SettlementFixture {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(database_url).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let pepper = b"PostgreSQL settlement feed test pepper longer than thirty-two bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("settlement-feed-postgres-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: format!("settlement-feed-postgres-{unique}"),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::from(100),
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
    let price = database
        .upsert_model_price(
            &format!("settlement-feed-postgres-{unique}"),
            "USD",
            Decimal::ONE,
            Decimal::ONE,
        )
        .await
        .unwrap();
    SettlementFixture {
        _directory: directory,
        database,
        account_id: issued.account_id,
        key,
        price,
    }
}

fn completed(request_id: Uuid, cost_micros: i64) -> FinishRequest {
    FinishRequest {
        request_id,
        status_code: 200,
        duration_ms: 1,
        input_tokens: 7,
        cached_input_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 3,
        service_tier: None,
        cost_micros,
        error_code: None,
        response_object: format!("gap://settlement-feed-postgres/{request_id}/response"),
    }
}

async fn wait_for_account_lock(
    observer: &Database,
    application_name: &str,
    backend_pid: i32,
    blocker_pid: i32,
) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND application_name = $2 AND state = 'active' AND wait_event_type = 'Lock' AND $3 = ANY(pg_blocking_pids(pid)))",
            )
            .bind(backend_pid)
            .bind(application_name)
            .bind(blocker_pid)
            .fetch_one(&observer.pool)
            .await
            .unwrap();
            if waiting {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("second settlement publisher did not wait for the account lock");
}

#[tokio::test]
async fn postgres_account_settlement_sequence_follows_commits_without_rollback_gaps() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!("MTC_TEST_POSTGRES_URL is unset; skipping PostgreSQL settlement feed lock test");
        return;
    };
    let fixture = postgres_fixture(&database_url).await;
    let second = Database::connect(&database_url).await.unwrap();
    let observer = Database::connect(&database_url).await.unwrap();

    let first_request = Uuid::now_v7();
    let second_request = Uuid::now_v7();
    let first_reservation = start(&fixture, first_request).await;
    let second_reservation = start(&fixture, second_request).await;
    let first_cost = fixture
        .database
        .settle_usage(&first_reservation, 7, 3)
        .await
        .unwrap();
    let second_cost = fixture
        .database
        .settle_usage(&second_reservation, 7, 3)
        .await
        .unwrap();

    let mut first_transaction = fixture.database.begin_write_transaction().await.unwrap();
    let first_backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *first_transaction)
        .await
        .unwrap();
    assert!(
        record_request_finished_in_transaction(
            &mut first_transaction,
            &completed(first_request, first_cost),
            unix_millis(),
            false,
        )
        .await
        .unwrap()
    );

    let application_name = format!("settlement-feed-publisher-{}", Uuid::now_v7());
    let second_application_name = application_name.clone();
    let (pid_sender, pid_receiver) = tokio::sync::oneshot::channel();
    let second_task = tokio::spawn(async move {
        let mut transaction = second.begin_write_transaction().await.unwrap();
        sqlx::query("SELECT set_config('application_name', $1, true)")
            .bind(&second_application_name)
            .execute(&mut *transaction)
            .await
            .unwrap();
        let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
        pid_sender.send(backend_pid).unwrap();
        assert!(
            record_request_finished_in_transaction(
                &mut transaction,
                &completed(second_request, second_cost),
                unix_millis(),
                false,
            )
            .await
            .unwrap()
        );
        transaction.commit().await.unwrap();
    });
    let second_backend_pid = pid_receiver.await.unwrap();
    wait_for_account_lock(
        &observer,
        &application_name,
        second_backend_pid,
        first_backend_pid,
    )
    .await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM account_settlement_feed WHERE account_id = $1"
        )
        .bind(fixture.account_id.to_string())
        .fetch_one(&observer.pool)
        .await
        .unwrap(),
        0,
        "the uncommitted first publisher must not be visible"
    );

    first_transaction.rollback().await.unwrap();
    second_task.await.unwrap();
    let committed = fixture
        .database
        .list_account_settlements(fixture.account_id, 1, None, None)
        .await
        .unwrap();
    assert_eq!(committed.items.len(), 1);
    assert_eq!(committed.items[0].request_id, second_request);
    assert_eq!(committed.items[0].settlement_sequence, 1);

    let third_request = Uuid::now_v7();
    let third_reservation = start(&fixture, third_request).await;
    let third_cost = fixture
        .database
        .settle_usage(&third_reservation, 7, 3)
        .await
        .unwrap();
    let mut third_transaction = fixture.database.begin_write_transaction().await.unwrap();
    assert!(
        record_request_finished_in_transaction(
            &mut third_transaction,
            &completed(third_request, third_cost),
            unix_millis(),
            false,
        )
        .await
        .unwrap()
    );
    third_transaction.commit().await.unwrap();

    let first_page = fixture
        .database
        .list_account_settlements(fixture.account_id, 1, None, None)
        .await
        .unwrap();
    assert_eq!(first_page.items.len(), 1);
    assert_eq!(first_page.items[0].request_id, second_request);
    let cursor = first_page
        .next_cursor
        .expect("subsequent settlement cursor");
    let resumed = fixture
        .database
        .list_account_settlements(
            fixture.account_id,
            1,
            Some((cursor.after_sequence, cursor.after_id)),
            None,
        )
        .await
        .unwrap();
    assert_eq!(resumed.items.len(), 1);
    assert_eq!(resumed.items[0].request_id, third_request);
    assert_eq!(resumed.items[0].settlement_sequence, 2);
}
