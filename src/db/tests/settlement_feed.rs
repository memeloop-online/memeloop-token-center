use super::super::*;

mod postgres;

struct SettlementFixture {
    _directory: tempfile::TempDir,
    database: Database,
    account_id: Uuid,
    key: AuthenticatedKey,
    price: ModelPrice,
}

async fn fixture(enforcement_mode: EnforcementMode) -> SettlementFixture {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("settlement-feed.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"settlement feed test pepper longer than thirty-two bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "settlement-feed".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "settlement-feed".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    enforcement_mode,
                    ..KeyPolicy::default()
                },
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
        .upsert_model_price("settlement-feed", "USD", Decimal::ONE, Decimal::ONE)
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

async fn start(fixture: &SettlementFixture, request_id: Uuid) -> UsageReservation {
    fixture
        .database
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &fixture.key,
            price: &fixture.price,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol: "openai",
            model: "settlement-feed",
            request_object: "gap://settlement-feed/request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap()
}

async fn finish(
    fixture: &SettlementFixture,
    request_id: Uuid,
    reservation: &UsageReservation,
) -> Result<FinishProxyRequestResult, AppError> {
    let response_object = format!("gap://settlement-feed/{request_id}/response");
    fixture
        .database
        .finish_proxy_request(FinishProxyRequest {
            request_id,
            tenant_id: fixture.key.tenant_id,
            reservation,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            requested_service_tier: None,
            status_code: 200,
            duration_ms: 1,
            usage: TokenUsage {
                input_tokens: 7,
                output_tokens: 3,
                ..TokenUsage::default()
            },
            charge_contract_ceiling: false,
            error_code: None,
            response_object: &response_object,
            conversation: None,
        })
        .await
}

async fn account_sequence(fixture: &SettlementFixture) -> i64 {
    sqlx::query_scalar("SELECT settlement_sequence FROM credit_accounts WHERE id = $1")
        .bind(fixture.account_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn settlement_feed_insert_failure_rolls_back_and_replay_is_idempotent() {
    let fixture = fixture(EnforcementMode::Prepaid).await;
    let request_id = Uuid::now_v7();
    let reservation = start(&fixture, request_id).await;

    // `request_id` is a test-generated UUID and SQLite trigger definitions cannot bind it.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER settlement_feed_fault BEFORE INSERT ON account_settlement_feed WHEN NEW.request_id = '{request_id}' BEGIN SELECT RAISE(ABORT, 'settlement feed fault'); END"
    )))
    .execute(&fixture.database.pool)
    .await
    .unwrap();
    assert!(finish(&fixture, request_id, &reservation).await.is_err());

    let terminal =
        sqlx::query("SELECT completed_at, status_code FROM request_records WHERE id = $1")
            .bind(request_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    assert_eq!(terminal.get::<Option<i64>, _>("completed_at"), None);
    assert_eq!(terminal.get::<Option<i64>, _>("status_code"), None);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM ledger_entries WHERE source = $1")
            .bind(reservation.id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(account_sequence(&fixture).await, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM account_settlement_feed")
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap(),
        0
    );

    sqlx::query("DROP TRIGGER settlement_feed_fault")
        .execute(&fixture.database.pool)
        .await
        .unwrap();
    assert!(matches!(
        finish(&fixture, request_id, &reservation).await.unwrap(),
        FinishProxyRequestResult::Finished { .. }
    ));
    assert!(matches!(
        finish(&fixture, request_id, &reservation).await.unwrap(),
        FinishProxyRequestResult::AlreadyFinished {
            status_code: 200,
            ..
        }
    ));
    assert_eq!(account_sequence(&fixture).await, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM ledger_entries WHERE source = $1")
            .bind(reservation.id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM account_settlement_feed")
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn settlement_feed_uses_commit_sequence_despite_out_of_order_ledger_times() {
    let fixture = fixture(EnforcementMode::Prepaid).await;
    let first_request = Uuid::now_v7();
    let second_request = Uuid::now_v7();
    let first_reservation = start(&fixture, first_request).await;
    let second_reservation = start(&fixture, second_request).await;

    // Make the first commit's ledger timestamp later than the second's before the
    // feed snapshots it, proving the feed order is commit sequence rather than time.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER settlement_feed_reorder_ledger_time AFTER INSERT ON ledger_entries WHEN NEW.kind = 'usage' AND NEW.source IN ('{}', '{}') BEGIN UPDATE ledger_entries SET created_at = CASE WHEN NEW.source = '{}' THEN 2000 ELSE 1000 END WHERE id = NEW.id; END",
        first_reservation.id, second_reservation.id, first_reservation.id,
    )))
    .execute(&fixture.database.pool)
    .await
    .unwrap();
    finish(&fixture, first_request, &first_reservation)
        .await
        .unwrap();
    finish(&fixture, second_request, &second_reservation)
        .await
        .unwrap();

    let page = fixture
        .database
        .list_account_settlements(fixture.account_id, 1, None, None)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].request_id, first_request);
    assert_eq!(page.items[0].settlement_sequence, 1);
    assert_eq!(page.items[0].settled_at, 2_000);
    let cursor = page.next_cursor.expect("second settlement cursor");
    assert_eq!(cursor.after_sequence, 1);
    assert_eq!(cursor.after_id, page.items[0].settlement_id);
    let next = fixture
        .database
        .list_account_settlements(
            fixture.account_id,
            1,
            Some((cursor.after_sequence, cursor.after_id)),
            None,
        )
        .await
        .unwrap();
    assert_eq!(next.items.len(), 1);
    assert_eq!(next.items[0].request_id, second_request);
    assert_eq!(next.items[0].settlement_sequence, 2);
    assert_eq!(next.items[0].settled_at, 1_000);
    assert_eq!(account_sequence(&fixture).await, 2);
    assert!(matches!(
        fixture
            .database
            .list_account_settlements(fixture.account_id, 1, Some((0, Uuid::now_v7())), None)
            .await,
        Err(AppError::BadRequest(_))
    ));
    assert!(matches!(
        fixture
            .database
            .list_account_settlements(fixture.account_id, 1, Some((1, Uuid::now_v7())), None)
            .await,
        Err(AppError::BadRequest(_))
    ));
}

#[tokio::test]
async fn settlement_feed_publishes_after_split_settlement_and_late_completion() {
    let fixture = fixture(EnforcementMode::Prepaid).await;
    let request_id = Uuid::now_v7();
    let reservation = fixture
        .database
        .reserve_usage(&fixture.key, &fixture.price, 10, 10)
        .await
        .unwrap();
    fixture
        .database
        .record_request_started(NewRequest {
            request_id,
            key_id: fixture.key.key_id,
            tenant_id: fixture.key.tenant_id,
            protocol: "openai".to_owned(),
            model: "settlement-feed".to_owned(),
            request_object: "gap://settlement-feed/split-request".to_owned(),
            reservation_id: reservation.id,
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let cost_micros = fixture
        .database
        .settle_usage(&reservation, 7, 3)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM ledger_entries WHERE source = $1")
            .bind(reservation.id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(account_sequence(&fixture).await, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM account_settlement_feed")
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap(),
        0,
        "a ledger-only settlement is not yet terminal-visible"
    );

    let completed = FinishRequest {
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
        response_object: "gap://settlement-feed/split-response".to_owned(),
    };
    fixture
        .database
        .record_request_finished(completed)
        .await
        .unwrap();
    assert_eq!(account_sequence(&fixture).await, 1);
    let published = fixture
        .database
        .list_account_settlements(
            fixture.account_id,
            1,
            None,
            Some((AccountSettlementKind::Text, request_id)),
        )
        .await
        .unwrap();
    assert_eq!(published.items.len(), 1);
    assert_eq!(published.items[0].request_id, request_id);
    assert_eq!(published.items[0].settlement_sequence, 1);

    fixture
        .database
        .record_request_finished(FinishRequest {
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
            response_object: "gap://settlement-feed/split-response".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(account_sequence(&fixture).await, 1);
}

#[tokio::test]
async fn metered_unlimited_terminal_does_not_publish_or_advance_sequence() {
    let fixture = fixture(EnforcementMode::MeteredUnlimited).await;
    let request_id = Uuid::now_v7();
    let reservation = start(&fixture, request_id).await;
    assert!(matches!(
        finish(&fixture, request_id, &reservation).await.unwrap(),
        FinishProxyRequestResult::Finished { .. }
    ));
    assert!(matches!(
        finish(&fixture, request_id, &reservation).await.unwrap(),
        FinishProxyRequestResult::AlreadyFinished {
            status_code: 200,
            ..
        }
    ));
    assert_eq!(account_sequence(&fixture).await, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM account_settlement_feed")
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap(),
        0
    );
}
