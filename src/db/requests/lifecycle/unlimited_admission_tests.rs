use super::*;
use crate::db::test_support::PostgresTestDatabase;
use rust_decimal::Decimal;

struct Fixture {
    isolated: PostgresTestDatabase,
    database: Database,
    key: AuthenticatedKey,
    price: ModelPrice,
}

impl Fixture {
    async fn postgres(mode: EnforcementMode) -> Option<Self> {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            assert!(
                std::env::var_os("CI").is_none(),
                "PostgreSQL is required in CI"
            );
            return None;
        };
        let isolated = PostgresTestDatabase::create(&url).await;
        let database = Database::connect_with_max(isolated.url(), 8)
            .await
            .unwrap()
            .with_terminal_projection_enabled(crate::config::DEFAULT_TERMINAL_PROJECTION_ENABLED);
        database.migrate().await.unwrap();
        let pepper = b"unlimited admission dedicated regression pepper";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "unlimited-admission".into(),
                    principal_external_id: "member".into(),
                    alias: "unlimited-admission".into(),
                    currency: "USD".into(),
                    policy: KeyPolicy {
                        enforcement_mode: mode,
                        ..KeyPolicy::default()
                    },
                    initial_balance: if mode.enforces_prepaid_limits() {
                        Decimal::ONE
                    } else {
                        Decimal::ZERO
                    },
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
        // Exercise the actual credential boundary, not a synthetic principal.
        assert!(matches!(
            database
                .authenticate_key("invalid-credential", pepper)
                .await,
            Err(AppError::Unauthorized)
        ));
        let price = database
            .upsert_model_price("unlimited-model", "USD", Decimal::ONE, Decimal::ONE)
            .await
            .unwrap();
        Some(Self {
            isolated,
            database,
            key,
            price,
        })
    }

    async fn admit(&self, request_id: Uuid) -> Result<UsageReservation, AppError> {
        self.database
            .start_proxy_forwarding_request_with_output_limit(
                StartProxyRequest {
                    request_id,
                    key: &self.key,
                    price: &self.price,
                    input_token_ceiling: 10,
                    output_token_ceiling: 20,
                    protocol: "openai",
                    model: "unlimited-model",
                    request_object: "gap://unlimited/request",
                    upstream_account_id: None,
                    model_route_id: None,
                },
                Some("unlimited-upstream"),
                Some(20),
            )
            .await
    }

    fn finish<'a>(
        &'a self,
        request_id: Uuid,
        reservation: &'a UsageReservation,
    ) -> FinishProxyRequest<'a> {
        FinishProxyRequest {
            terminal_cause: None,
            usage_basis: Some(crate::model::RequestUsageBasis::ProviderReported),
            first_output_ms: Some(1),
            generation_duration_ms: None,
            request_id,
            tenant_id: self.key.tenant_id,
            reservation,
            input_token_ceiling: 10,
            output_token_ceiling: 20,
            requested_service_tier: None,
            status_code: 200,
            duration_ms: 10,
            usage: TokenUsage {
                input_tokens: 3,
                output_tokens: 5,
                ..TokenUsage::default()
            },
            error_code: None,
            response_object: "gap://unlimited/response",
            routing_session_id: None,
            routing_terminal_observed_at: None,
            conversation: None,
        }
    }

    async fn cleanup(self) {
        self.database.close().await;
        self.isolated.cleanup().await;
    }
}

#[tokio::test]
async fn postgres_unlimited_admission_and_terminal_ignore_shared_financial_and_stats_locks() {
    let Some(fixture) = Fixture::postgres(EnforcementMode::MeteredUnlimited).await else {
        return;
    };
    let mut blocker = fixture.database.begin_write_transaction().await.unwrap();
    for statement in [
        "UPDATE key_budget_state SET updated_at = updated_at WHERE key_id = $1",
        "UPDATE credit_accounts SET updated_at = updated_at WHERE id = $1",
    ] {
        let id = if statement.contains("key_budget_state") {
            fixture.key.key_id
        } else {
            fixture.key.account_id
        };
        sqlx::query(statement)
            .bind(id.to_string())
            .execute(&mut *blocker)
            .await
            .unwrap();
    }
    crate::db::projection_lock::lock_request_stats_projection_rebuild_in_transaction(&mut blocker)
        .await
        .unwrap();

    // Both simultaneous admissions and terminal winners must finish while all
    // three shared gates remain held. The only holds are per-request owners.
    let first_id = Uuid::now_v7();
    let second_id = Uuid::now_v7();
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(fixture.admit(first_id), fixture.admit(second_id))
    })
    .await
    .expect("unlimited admission must not wait for financial/statistics locks");
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!((first.reserved_micros, second.reserved_micros), (0, 0));
    assert_eq!(first.enforcement_mode, EnforcementMode::MeteredUnlimited);
    assert!(
        fixture.admit(first_id).await.is_err(),
        "duplicate request must not admit twice"
    );
    let mut forged = fixture.finish(first_id, &first);
    forged.tenant_id = Uuid::now_v7();
    assert!(fixture.database.finish_proxy_request(forged).await.is_err());

    let (normal, cancelled) = tokio::time::timeout(Duration::from_secs(5), async {
        let mut cancellation = fixture.finish(second_id, &second);
        cancellation.status_code = 499;
        cancellation.usage_basis = Some(crate::model::RequestUsageBasis::NotObserved);
        cancellation.usage = TokenUsage::default();
        cancellation.error_code = Some("client_disconnected");
        tokio::join!(
            fixture
                .database
                .finish_proxy_request(fixture.finish(first_id, &first)),
            fixture.database.finish_proxy_request(cancellation)
        )
    })
    .await
    .expect("durable terminal commit must not wait for shared financial/statistics locks");
    assert!(matches!(
        normal.unwrap(),
        FinishProxyRequestResult::Finished { cost_micros: 8, .. }
    ));
    assert!(matches!(
        cancelled.unwrap(),
        FinishProxyRequestResult::Finished { cost_micros: 0, .. }
    ));
    assert!(matches!(
        fixture
            .database
            .finish_proxy_request(fixture.finish(first_id, &first))
            .await
            .unwrap(),
        FinishProxyRequestResult::AlreadyFinished { cost_micros: 8, .. }
    ));
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM ledger_entries WHERE kind = 'usage'), (SELECT COUNT(*) FROM terminal_projection_outbox WHERE projected_at IS NULL), (SELECT COUNT(*) FROM request_stats_facts)"
    ).fetch_one(&fixture.database.pool).await.unwrap();
    assert_eq!(counts, (2, 2, 0));
    let balances: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT a.available_micros, a.reserved_micros, k.reserved_micros, k.settled_lifetime_micros FROM credit_accounts a JOIN key_budget_state k ON k.key_id = $1 WHERE a.id = $2"
    ).bind(fixture.key.key_id.to_string()).bind(fixture.key.account_id.to_string())
        .fetch_one(&fixture.database.pool).await.unwrap();
    assert_eq!(balances, (0, 0, 0, 0));
    blocker.rollback().await.unwrap();

    // A dead worker's claim is recoverable; replay cannot debit or count again.
    let stale = Uuid::now_v7();
    assert_eq!(
        fixture
            .database
            .claim_terminal_projection_tasks(stale, 32)
            .await
            .unwrap()
            .len(),
        2
    );
    sqlx::query(
        "UPDATE terminal_projection_outbox SET lease_expires_at = 0 WHERE lease_owner = $1",
    )
    .bind(stale.to_string())
    .execute(&fixture.database.pool)
    .await
    .unwrap();
    let owner = Uuid::now_v7();
    let tasks = fixture
        .database
        .claim_terminal_projection_tasks(owner, 32)
        .await
        .unwrap();
    assert_eq!(tasks.len(), 2);
    for request_id in tasks {
        assert!(
            !fixture
                .database
                .project_claimed_terminal_projection_task(stale, request_id)
                .await
                .unwrap()
        );
        assert!(
            fixture
                .database
                .project_claimed_terminal_projection_task(owner, request_id)
                .await
                .unwrap()
        );
        assert!(
            !fixture
                .database
                .project_claimed_terminal_projection_task(owner, request_id)
                .await
                .unwrap()
        );
    }
    let totals: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM ledger_entries WHERE kind = 'usage'), (SELECT CAST(SUM(cost_micros) AS BIGINT) FROM request_stats_facts), (SELECT COUNT(*) FROM request_events WHERE event_kind = 'finished'), (SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1)"
    ).bind(fixture.key.account_id.to_string()).fetch_one(&fixture.database.pool).await.unwrap();
    assert_eq!(totals, (2, 8, 2, 8));
    fixture.cleanup().await;
}

#[tokio::test]
async fn postgres_unlimited_contract_skips_hypothetical_hold_and_preserves_mode_on_resize() {
    let Some(fixture) = Fixture::postgres(EnforcementMode::MeteredUnlimited).await else {
        return;
    };
    let mut price = fixture.price.clone();
    price.input_micros_per_million = i64::MAX;
    price.output_micros_per_million = i64::MAX;
    // The hypothetical sum overflows prepaid monetary arithmetic. Unlimited
    // needs no hold; final observed usage is still priced by the snapshot.
    let reservation = fixture
        .database
        .reserve_usage(&fixture.key, &price, 1_000_000, 1_000_000)
        .await
        .unwrap();
    assert_eq!(reservation.reserved_micros, 0);
    let mut changed_key = fixture.key.clone();
    changed_key.policy.enforcement_mode = EnforcementMode::Prepaid;
    let mut transaction = fixture.database.begin_write_transaction().await.unwrap();
    let resized = super::super::settlement::resize_usage_reservation_in_transaction(
        &mut transaction,
        &changed_key,
        &fixture.price,
        &reservation,
        10,
        20,
        unix_millis(),
    )
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    assert_eq!(resized.reserved_micros, 0);
    assert_eq!(resized.enforcement_mode, EnforcementMode::MeteredUnlimited);
    let mut blocker = fixture.database.begin_write_transaction().await.unwrap();
    crate::db::projection_lock::lock_request_stats_projection_rebuild_in_transaction(&mut blocker)
        .await
        .unwrap();
    let settled = tokio::time::timeout(
        Duration::from_secs(5),
        fixture.database.settle_usage(&resized, 3, 5),
    )
    .await
    .expect("split metered settlement must not acquire the statistics fence")
    .unwrap();
    assert_eq!(settled, 8);
    blocker.rollback().await.unwrap();
    let generation = GenerationPrice {
        id: Uuid::now_v7(),
        model: "metered-units".into(),
        currency: "USD".into(),
        billing_unit: "second".into(),
        price_per_unit: "1".into(),
        micros_per_unit: i64::MAX,
    };
    let mut transaction = fixture.database.begin_write_transaction().await.unwrap();
    let unit_contract = super::super::settlement::reserve_metered_usage_with_id_in_transaction(
        &mut transaction,
        &fixture.key,
        &generation,
        2,
        unix_millis(),
        Uuid::now_v7(),
    )
    .await
    .unwrap();
    assert_eq!(unit_contract.reservation.reserved_micros, 0);
    transaction.rollback().await.unwrap();
    fixture.cleanup().await;
}

#[tokio::test]
async fn postgres_prepaid_terminal_releases_finance_before_statistical_projection() {
    let Some(fixture) = Fixture::postgres(EnforcementMode::Prepaid).await else {
        return;
    };
    let request_id = Uuid::now_v7();
    let reservation = fixture.admit(request_id).await.unwrap();
    assert_eq!(reservation.reserved_micros, 30);
    let mut blocker = fixture.database.begin_write_transaction().await.unwrap();
    crate::db::projection_lock::lock_request_stats_projection_rebuild_in_transaction(&mut blocker)
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture
            .database
            .finish_proxy_request(fixture.finish(request_id, &reservation)),
    )
    .await
    .expect("financial commit must not wait on statistics")
    .unwrap();
    let next = tokio::time::timeout(Duration::from_secs(5), fixture.admit(Uuid::now_v7()))
        .await
        .expect("next prepaid admission must not wait on prior statistics")
        .unwrap();
    assert_eq!(next.reserved_micros, 30);
    let balances: (i64, i64, i64) = sqlx::query_as(
        "SELECT a.available_micros, a.reserved_micros, k.settled_lifetime_micros FROM credit_accounts a JOIN key_budget_state k ON k.key_id = $1 WHERE a.id = $2"
    ).bind(fixture.key.key_id.to_string()).bind(fixture.key.account_id.to_string())
        .fetch_one(&fixture.database.pool).await.unwrap();
    assert_eq!(balances, (999_962, 30, 8));
    blocker.rollback().await.unwrap();
    // Mode changes must preserve the already funded contract and release its
    // real hold at settlement, rather than changing it to zero/unlimited.
    let mut changed_key = fixture.key.clone();
    changed_key.policy.enforcement_mode = EnforcementMode::MeteredUnlimited;
    let mut transaction = fixture.database.begin_write_transaction().await.unwrap();
    let resized = super::super::settlement::resize_usage_reservation_in_transaction(
        &mut transaction,
        &changed_key,
        &fixture.price,
        &next,
        10,
        30,
        unix_millis(),
    )
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    assert_eq!(resized.enforcement_mode, EnforcementMode::Prepaid);
    assert_eq!(resized.reserved_micros, 40);
    assert_eq!(
        fixture.database.settle_usage(&resized, 3, 5).await.unwrap(),
        8
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn postgres_terminal_outbox_failure_rolls_back_unlimited_and_prepaid_finance() {
    for mode in [EnforcementMode::MeteredUnlimited, EnforcementMode::Prepaid] {
        let Some(fixture) = Fixture::postgres(mode).await else {
            return;
        };
        let request_id = Uuid::now_v7();
        let reservation = fixture.admit(request_id).await.unwrap();
        // Reject the durable handoff after terminal/ledger writes. A failure
        // must keep the request pending and the original contract recoverable.
        sqlx::raw_sql(
            "CREATE FUNCTION reject_terminal_snapshot() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN RAISE EXCEPTION 'injected terminal outbox failure'; END $$;
             CREATE TRIGGER reject_terminal_snapshot BEFORE INSERT ON terminal_projection_outbox
             FOR EACH ROW EXECUTE FUNCTION reject_terminal_snapshot();",
        )
        .execute(&fixture.database.pool)
        .await
        .unwrap();
        assert!(
            fixture
                .database
                .finish_proxy_request(fixture.finish(request_id, &reservation))
                .await
                .is_err()
        );
        let state: (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM request_records WHERE id = $1 AND completed_at IS NULL),
             (SELECT COUNT(*) FROM ledger_entries WHERE kind = 'usage'),
             (SELECT COUNT(*) FROM terminal_projection_outbox),
             (SELECT COUNT(*) FROM usage_reservations WHERE id = $2 AND status = 'reserved')",
        )
        .bind(request_id.to_string())
        .bind(reservation.id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
        assert_eq!(state, (1, 0, 0, 1));
        sqlx::query("DROP TRIGGER reject_terminal_snapshot ON terminal_projection_outbox")
            .execute(&fixture.database.pool)
            .await
            .unwrap();
        let (first, replay) = tokio::join!(
            fixture
                .database
                .finish_proxy_request(fixture.finish(request_id, &reservation)),
            fixture
                .database
                .finish_proxy_request(fixture.finish(request_id, &reservation))
        );
        let results = [first.unwrap(), replay.unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, FinishProxyRequestResult::Finished { cost_micros: 8, .. }))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(
                    r,
                    FinishProxyRequestResult::AlreadyFinished { cost_micros: 8, .. }
                ))
                .count(),
            1
        );
        let ledger: (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), CAST(-SUM(amount_micros) AS BIGINT) FROM ledger_entries WHERE kind = 'usage'",
        )
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
        assert_eq!(ledger, (1, 8));
        fixture.cleanup().await;
    }
}
