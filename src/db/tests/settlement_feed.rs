use super::super::*;
use crate::model::RequestUsageBasis;

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
    finish_with_usage_basis(
        fixture,
        request_id,
        reservation,
        RequestUsageBasis::ProviderReported,
        7,
        3,
    )
    .await
}

async fn finish_with_usage_basis(
    fixture: &SettlementFixture,
    request_id: Uuid,
    reservation: &UsageReservation,
    usage_basis: RequestUsageBasis,
    input_tokens: i64,
    output_tokens: i64,
) -> Result<FinishProxyRequestResult, AppError> {
    finish_with_usage_basis_and_error(
        fixture,
        request_id,
        reservation,
        usage_basis,
        input_tokens,
        output_tokens,
        None,
    )
    .await
}

async fn finish_with_usage_basis_and_error(
    fixture: &SettlementFixture,
    request_id: Uuid,
    reservation: &UsageReservation,
    usage_basis: RequestUsageBasis,
    input_tokens: i64,
    output_tokens: i64,
    error_code: Option<&str>,
) -> Result<FinishProxyRequestResult, AppError> {
    finish_with_terminal_evidence(
        fixture,
        request_id,
        reservation,
        Some(usage_basis),
        200,
        input_tokens,
        output_tokens,
        error_code,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn finish_with_terminal_evidence(
    fixture: &SettlementFixture,
    request_id: Uuid,
    reservation: &UsageReservation,
    usage_basis: Option<RequestUsageBasis>,
    status_code: i64,
    input_tokens: i64,
    output_tokens: i64,
    error_code: Option<&str>,
) -> Result<FinishProxyRequestResult, AppError> {
    let response_object = format!("gap://settlement-feed/{request_id}/response");
    fixture
        .database
        .finish_proxy_request(FinishProxyRequest {
            usage_basis,
            first_output_ms: None,
            generation_duration_ms: None,
            request_id,
            tenant_id: fixture.key.tenant_id,
            reservation,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            requested_service_tier: None,
            status_code,
            duration_ms: 1,
            usage: TokenUsage {
                input_tokens,
                output_tokens,
                ..TokenUsage::default()
            },
            error_code,
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
    let before = fixture
        .database
        .list_requests(fixture.key.key_id, 10)
        .await
        .unwrap();
    assert_eq!(before[0].usage_basis, None);

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
    // The terminal record, list/event projection and immutable ledger feed
    // agree on provenance after the same transaction; replay cannot duplicate it.
    let requests = fixture
        .database
        .list_requests(fixture.key.key_id, 10)
        .await
        .unwrap();
    assert_eq!(
        requests[0].usage_basis,
        Some(RequestUsageBasis::ProviderReported)
    );
    assert_eq!(
        serde_json::to_value(&requests[0]).unwrap()["usage_basis"],
        "provider_reported"
    );
    let events = fixture
        .database
        .request_events_after("settlement-feed", 0, None, 10)
        .await
        .unwrap();
    assert!(events.iter().any(|event| event.request_id == request_id
        && event.usage_basis == Some(RequestUsageBasis::ProviderReported)));
    let feed = fixture
        .database
        .list_account_settlements(fixture.account_id, 10, None, None)
        .await
        .unwrap();
    assert_eq!(feed.items.len(), 1);
    assert_eq!(
        feed.items[0].usage_basis,
        Some(RequestUsageBasis::ProviderReported)
    );
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
async fn two_xx_error_code_uses_zero_in_request_and_event_projections() {
    let fixture = fixture(EnforcementMode::Prepaid).await;
    let request_id = Uuid::now_v7();
    let reservation = start(&fixture, request_id).await;
    finish_with_usage_basis_and_error(
        &fixture,
        request_id,
        &reservation,
        RequestUsageBasis::NotObserved,
        7,
        3,
        Some("client_cancelled"),
    )
    .await
    .expect("finish 2xx client-cancelled request");

    let requests = fixture
        .database
        .list_requests(fixture.key.key_id, 10)
        .await
        .expect("request projection");
    let request = requests
        .iter()
        .find(|request| request.request_id == request_id)
        .expect("request row");
    assert_eq!(request.status_code, Some(200));
    assert_eq!(request.error_code.as_deref(), Some("client_cancelled"));
    assert_eq!(request.cost, "0");
    assert_eq!(request.billing.cost.as_deref(), Some("0"));

    let events = fixture
        .database
        .request_events_after("settlement-feed", 0, None, 10)
        .await
        .expect("event projection");
    let event = events
        .iter()
        .find(|event| event.request_id == request_id)
        .expect("finished event");
    assert_eq!(event.status_code, Some(200));
    assert_eq!(event.error_code.as_deref(), Some("client_cancelled"));
    assert_eq!(event.cost, "0");
    assert_eq!(event.billing.cost.as_deref(), Some("0"));
}

#[tokio::test]
async fn terminal_cost_policy_flows_into_request_aggregates_without_rewriting_settlement() {
    let fixture = fixture(EnforcementMode::Prepaid).await;
    let cases = [
        ("success unknown", None, 200, None, true),
        (
            "failed provider reported",
            Some(RequestUsageBasis::ProviderReported),
            503,
            None,
            true,
        ),
        (
            "failed provider estimated",
            Some(RequestUsageBasis::ProviderEstimated),
            502,
            None,
            false,
        ),
        (
            "failed contract ceiling",
            Some(RequestUsageBasis::ContractCeiling),
            503,
            None,
            false,
        ),
        (
            "failed not observed",
            Some(RequestUsageBasis::NotObserved),
            499,
            None,
            false,
        ),
        (
            "2xx terminal error without provenance",
            None,
            200,
            Some("upstream_incomplete_response"),
            false,
        ),
    ];
    let mut expected_aggregate_cost = 0_i64;

    for (label, usage_basis, status_code, error_code, keeps_cost) in cases {
        let request_id = Uuid::now_v7();
        let reservation = start(&fixture, request_id).await;
        finish_with_terminal_evidence(
            &fixture,
            request_id,
            &reservation,
            usage_basis,
            status_code,
            7,
            3,
            error_code,
        )
        .await
        .unwrap_or_else(|error| panic!("{label}: {error}"));

        let stored_cost: i64 =
            sqlx::query_scalar("SELECT cost_micros FROM request_records WHERE id = $1")
                .bind(request_id.to_string())
                .fetch_one(&fixture.database.pool)
                .await
                .unwrap();
        assert!(
            stored_cost > 0,
            "{label}: settlement amount remains auditable"
        );
        let projected_cost: i64 =
            sqlx::query_scalar("SELECT cost_micros FROM request_stats_facts WHERE request_id = $1")
                .bind(request_id.to_string())
                .fetch_one(&fixture.database.pool)
                .await
                .unwrap();
        let expected = if keeps_cost { stored_cost } else { 0 };
        assert_eq!(projected_cost, expected, "{label}");
        let projected_status: String = sqlx::query_scalar(
            "SELECT status_class FROM request_stats_facts WHERE request_id = $1",
        )
        .bind(request_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
        let expected_status = if status_code >= 200 && status_code < 400 && error_code.is_none() {
            "success"
        } else {
            "failure"
        };
        assert_eq!(projected_status, expected_status, "{label}");
        expected_aggregate_cost += expected;
    }

    for table in [
        "usage_daily_aggregates",
        "request_daily_aggregates",
        "usage_analysis_hourly",
        "usage_analysis_daily",
        "session_usage_totals",
        "session_usage_hourly",
        "session_usage_daily",
    ] {
        let aggregate_cost: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COALESCE(SUM(cost_micros), 0) FROM {table} WHERE key_id = $1"
        )))
        .bind(fixture.key.key_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
        assert_eq!(aggregate_cost, expected_aggregate_cost, "{table}");
    }
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
        first_output_ms: None,
        generation_duration_ms: None,
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
            first_output_ms: None,
            generation_duration_ms: None,
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

#[tokio::test]
async fn correction_preview_only_returns_contract_ceiling_with_stable_cursor() {
    let fixture = fixture(EnforcementMode::Prepaid).await;
    let first_request = Uuid::now_v7();
    let null_request = Uuid::now_v7();
    let second_request = Uuid::now_v7();
    let first_reservation = start(&fixture, first_request).await;
    let null_reservation = start(&fixture, null_request).await;
    let second_reservation = start(&fixture, second_request).await;
    finish_with_usage_basis(
        &fixture,
        first_request,
        &first_reservation,
        RequestUsageBasis::ContractCeiling,
        10,
        10,
    )
    .await
    .unwrap();
    finish_with_usage_basis(
        &fixture,
        null_request,
        &null_reservation,
        RequestUsageBasis::ProviderReported,
        10,
        10,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE request_records SET usage_basis = NULL WHERE id = $1")
        .bind(null_request.to_string())
        .execute(&fixture.database.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE account_settlement_feed SET usage_basis = NULL WHERE request_id = $1")
        .bind(null_request.to_string())
        .execute(&fixture.database.pool)
        .await
        .unwrap();
    finish_with_usage_basis(
        &fixture,
        second_request,
        &second_reservation,
        RequestUsageBasis::ContractCeiling,
        10,
        10,
    )
    .await
    .unwrap();

    let now = unix_millis();
    let first_page = fixture
        .database
        .list_settlement_correction_previews(
            fixture.account_id,
            now.saturating_sub(10_000),
            now.saturating_add(10_000),
            1,
            None,
        )
        .await
        .unwrap();
    assert_eq!(first_page.items.len(), 1);
    let first = &first_page.items[0];
    assert_eq!(
        first.original.usage_basis,
        RequestUsageBasis::ContractCeiling
    );
    assert_eq!(first.evidence.archive_state, RequestArchiveState::Gap);
    assert!(!first.evidence.response_available);
    assert_eq!(first.evidence.provider_usage, "not_evaluated");
    assert_eq!(
        first.invariants.settlement_feed,
        SettlementCorrectionFeedState::Matched
    );
    assert_eq!(
        first.review_state,
        SettlementCorrectionReviewState::ReadyForEvidence
    );
    assert!(first.usage_ledger_entry_id.is_some());
    assert_eq!(first.pending_correction.corrected_cost, None);
    assert_eq!(first.pending_correction.corrected_usage_basis, None);

    let cursor = first_page.next_cursor.expect("second correction candidate");
    let second_page = fixture
        .database
        .list_settlement_correction_previews(
            fixture.account_id,
            now.saturating_sub(10_000),
            now.saturating_add(10_000),
            1,
            Some((cursor.after_created_at, cursor.after_request_id)),
        )
        .await
        .unwrap();
    assert_eq!(second_page.items.len(), 1);
    assert_ne!(second_page.items[0].request_id, first.request_id);
    assert_ne!(second_page.items[0].request_id, null_request);
    assert!(second_page.next_cursor.is_none());
    assert!(matches!(
        fixture
            .database
            .list_settlement_correction_previews(
                fixture.account_id,
                now.saturating_sub(10_000),
                now.saturating_add(10_000),
                1,
                Some((cursor.after_created_at, null_request)),
            )
            .await,
        Err(AppError::BadRequest(_))
    ));
}

#[tokio::test]
async fn correction_preview_distinguishes_metered_and_invariant_mismatch() {
    let metered = fixture(EnforcementMode::MeteredUnlimited).await;
    let metered_request = Uuid::now_v7();
    let metered_reservation = start(&metered, metered_request).await;
    finish_with_usage_basis(
        &metered,
        metered_request,
        &metered_reservation,
        RequestUsageBasis::ContractCeiling,
        10,
        10,
    )
    .await
    .unwrap();
    let now = unix_millis();
    let metered_page = metered
        .database
        .list_settlement_correction_previews(
            metered.account_id,
            now.saturating_sub(10_000),
            now.saturating_add(10_000),
            10,
            None,
        )
        .await
        .unwrap();
    assert_eq!(metered_page.items.len(), 1);
    assert_eq!(
        metered_page.items[0].invariants.settlement_feed,
        SettlementCorrectionFeedState::NotApplicableMetered
    );
    assert_eq!(
        metered_page.items[0].review_state,
        SettlementCorrectionReviewState::ReadyForEvidence
    );

    let prepaid = fixture(EnforcementMode::Prepaid).await;
    let prepaid_request = Uuid::now_v7();
    let prepaid_reservation = start(&prepaid, prepaid_request).await;
    finish_with_usage_basis(
        &prepaid,
        prepaid_request,
        &prepaid_reservation,
        RequestUsageBasis::ContractCeiling,
        10,
        10,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE usage_reservations SET actual_micros = actual_micros + 1 WHERE id = $1")
        .bind(prepaid_reservation.id.to_string())
        .execute(&prepaid.database.pool)
        .await
        .unwrap();
    let mismatch_page = prepaid
        .database
        .list_settlement_correction_previews(
            prepaid.account_id,
            now.saturating_sub(10_000),
            now.saturating_add(10_000),
            10,
            None,
        )
        .await
        .unwrap();
    assert_eq!(mismatch_page.items.len(), 1);
    assert!(
        !mismatch_page.items[0]
            .invariants
            .reservation_actual_matches_cost
    );
    assert_eq!(
        mismatch_page.items[0].review_state,
        SettlementCorrectionReviewState::InvariantMismatch
    );
}

#[tokio::test]
async fn correction_preview_preserves_negative_tokens_but_never_marks_them_ready() {
    let fixture = fixture(EnforcementMode::Prepaid).await;
    let request_id = Uuid::now_v7();
    let reservation = start(&fixture, request_id).await;
    finish_with_usage_basis(
        &fixture,
        request_id,
        &reservation,
        RequestUsageBasis::ContractCeiling,
        10,
        10,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE request_records SET input_tokens = -1, output_tokens = 21 WHERE id = $1")
        .bind(request_id.to_string())
        .execute(&fixture.database.pool)
        .await
        .unwrap();

    let now = unix_millis();
    let page = fixture
        .database
        .list_settlement_correction_previews(
            fixture.account_id,
            now.saturating_sub(10_000),
            now.saturating_add(10_000),
            10,
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    let preview = &page.items[0];
    assert_eq!(preview.original.input_tokens, -1);
    assert_eq!(preview.original.output_tokens, 21);
    assert!(!preview.invariants.token_counts_non_negative);
    assert!(!preview.invariants.token_ceiling_matches_reservation);
    assert_eq!(
        preview.review_state,
        SettlementCorrectionReviewState::InvariantMismatch
    );
    assert_eq!(
        serde_json::to_value(preview).unwrap()["original"]["input_tokens"],
        -1
    );
}
