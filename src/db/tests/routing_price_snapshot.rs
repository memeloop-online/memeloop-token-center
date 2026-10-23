use super::super::*;

async fn single_checkout_price_snapshot(database: &Database) {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    let model = Uuid::now_v7().to_string();
    let base = database
        .upsert_model_price(&model, "USD", Decimal::new(3, 0), Decimal::new(7, 0))
        .await
        .unwrap();
    for (tier, input, cached, write, output) in [("priority", 11, 2, 4, 20), ("batch", 8, 1, 2, 9)]
    {
        database
            .upsert_model_price_tier(
                &model,
                "USD",
                tier,
                Decimal::new(input, 0),
                Decimal::new(cached, 0),
                Decimal::new(write, 0),
                Decimal::new(output, 0),
                false,
            )
            .await
            .unwrap();
    }
    database
        .upsert_model_price(&model, "EUR", Decimal::TEN, Decimal::TEN)
        .await
        .unwrap();

    let checkouts = Arc::new(AtomicUsize::new(0));
    let delayed_return = Arc::new(AtomicBool::new(false));
    let returning = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let checkout_count = checkouts.clone();
    let return_enabled = delayed_return.clone();
    let return_entered = returning.clone();
    let return_release = release.clone();
    let mut observed = database.clone();
    observed.pool = database
        .pool
        .options()
        .clone()
        .max_connections(1)
        .before_acquire(move |_connection, _metadata| {
            let checkouts = checkout_count.clone();
            Box::pin(async move {
                checkouts.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            })
        })
        .after_release(move |_connection, _metadata| {
            let enabled = return_enabled.clone();
            let entered = return_entered.clone();
            let release = return_release.clone();
            Box::pin(async move {
                if enabled.load(Ordering::SeqCst) {
                    entered.notify_one();
                    release.acquire().await.unwrap().forget();
                }
                Ok(true)
            })
        })
        .connect_lazy_with(database.pool.connect_options().as_ref().clone());
    let mut warmed = observed.pool.acquire().await.unwrap();
    warmed.return_to_pool().await;
    checkouts.store(0, Ordering::SeqCst);
    delayed_return.store(true, Ordering::SeqCst);
    let result =
        tokio::time::timeout(Duration::from_secs(6), observed.model_price(&model, "usd")).await;
    let return_observed = tokio::time::timeout(Duration::from_secs(5), returning.notified()).await;
    delayed_return.store(false, Ordering::SeqCst);
    release.add_permits(1);
    observed.pool.close().await;
    return_observed.expect("the fixture must stall the price query's connection return");
    let price = result
        .expect("base and tiers must not require a second pool checkout")
        .unwrap();
    assert_eq!(checkouts.load(Ordering::SeqCst), 1);
    assert_eq!(price.id, base.id);
    assert_eq!(price.input_micros_per_million, 3_000_000);
    assert_eq!(price.output_micros_per_million, 7_000_000);
    assert_eq!(
        price
            .tiers
            .iter()
            .map(|tier| tier.service_tier.as_str())
            .collect::<Vec<_>>(),
        ["default", "batch", "priority"]
    );
    let priority = &price.tiers[2];
    assert_eq!(priority.input_micros_per_million, 11_000_000);
    assert_eq!(priority.cached_input_micros_per_million, 2_000_000);
    assert_eq!(priority.cache_write_micros_per_million, 4_000_000);
    assert_eq!(priority.output_micros_per_million, 20_000_000);
    assert_eq!(priority.source, "manual");

    database
        .upsert_model_price(&model, "USD", Decimal::TEN, Decimal::TEN)
        .await
        .unwrap();
    assert_eq!(price.input_micros_per_million, 3_000_000);
    let updated = database.model_price(&model, "USD").await.unwrap();
    assert_eq!(updated.id, price.id);
    assert_eq!(updated.input_micros_per_million, 10_000_000);
    assert_eq!(updated.tiers[0].input_micros_per_million, 10_000_000);
    assert_eq!(updated.tiers[2].input_micros_per_million, 11_000_000);

    sqlx::query("DELETE FROM model_price_tiers WHERE model = $1 AND currency = 'USD'")
        .bind(&model)
        .execute(&database.pool)
        .await
        .unwrap();
    let legacy = database.model_price(&model, "usd").await.unwrap();
    assert_eq!(legacy.id, price.id);
    assert_eq!(legacy.input_micros_per_million, 10_000_000);
    assert!(legacy.tiers.is_empty());
    assert!(matches!(
        database.model_price(&model, "CNY").await,
        Err(AppError::UnpricedModel)
    ));
    assert!(matches!(
        database.model_price("missing-price-fixture", "USD").await,
        Err(AppError::UnpricedModel)
    ));
}

async fn optional_output_financial_contract(
    database: &Database,
    recovered_database: &Database,
    balance: i64,
    budget: Option<i64>,
    cancelled: bool,
) {
    let unique = Uuid::now_v7().to_string();
    let pepper = b"optional output validity financial contract pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: unique.clone(),
                principal_external_id: "member".into(),
                alias: unique.clone(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    max_concurrency: 2,
                    lifetime_budget: budget.map(|amount| Decimal::new(amount, 6).to_string()),
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::new(balance, 6),
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
        .upsert_model_price(&unique, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let reservation = database
        .start_proxy_forwarding_request_with_output_limit(
            StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 10,
                output_token_ceiling: 4096,
                protocol: "openai",
                model: &unique,
                request_object: &format!("gap://{request_id}/request"),
                upstream_account_id: None,
                model_route_id: None,
            },
            Some(&unique),
            None,
        )
        .await
        .unwrap();
    let other_hold = database.reserve_usage(&key, &price, 0, 4000).await.unwrap();
    assert_eq!(reservation.reserved_micros, 4106);
    assert_eq!(other_hold.reserved_micros, 4000);
    database
        .prepare_proxy_delivery(request_id, key.tenant_id, &reservation, 10, 4096, None)
        .await
        .unwrap();
    database
        .mark_proxy_delivery_started(request_id, key.tenant_id, &reservation)
        .await
        .unwrap();
    let durable: (i64, i64, String) = sqlx::query_as("SELECT q.input_tokens, q.output_tokens, r.price_snapshot_json FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.id = $1")
        .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!((durable.0, durable.1), (10, 4096));
    let snapshot: serde_json::Value = serde_json::from_str(&durable.2).unwrap();
    assert_eq!(
        snapshot.get("output_token_limit"),
        Some(&serde_json::Value::Null)
    );
    database
        .upsert_model_price(&unique, "USD", Decimal::TEN, Decimal::TEN)
        .await
        .unwrap();
    let mut stale_reservation = reservation.clone();
    stale_reservation.output_micros_per_million = 99_000_000;
    let finish = FinishProxyRequest {
        terminal_cause: None,
        usage_basis: Some(crate::model::RequestUsageBasis::ProviderReported),
        first_output_ms: Some(1),
        generation_duration_ms: None,
        request_id,
        tenant_id: key.tenant_id,
        reservation: &stale_reservation,
        input_token_ceiling: 10,
        output_token_ceiling: 4096,
        requested_service_tier: None,
        status_code: if cancelled { 499 } else { 200 },
        duration_ms: 2,
        usage: TokenUsage {
            input_tokens: 3,
            output_tokens: 5000,
            ..TokenUsage::default()
        },
        error_code: cancelled.then_some("client_cancelled"),
        response_object: "gap://optional-output/response",
        routing_session_id: None,
        routing_terminal_observed_at: None,
        conversation: None,
    };
    let (first, second) = tokio::join!(
        recovered_database.finish_proxy_request(finish.clone()),
        recovered_database.finish_proxy_request(finish)
    );
    let results = [first.unwrap(), second.unwrap()];
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                FinishProxyRequestResult::Finished {
                    usage_invalid: false,
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, FinishProxyRequestResult::AlreadyFinished { .. }))
            .count(),
        1
    );
    let charged = 5003_i64
        .min(balance - 4000)
        .min(budget.unwrap_or(i64::MAX) - 4000);
    let row = sqlx::query("SELECT q.status_code, q.input_tokens, q.output_tokens, q.cost_micros, q.error_code, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = q.reservation_id) AS ledgers FROM request_records q WHERE q.id = $1")
        .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(
        row.get::<i64, _>("status_code"),
        if cancelled { 499 } else { 200 }
    );
    assert_eq!(row.get::<i64, _>("input_tokens"), 3);
    assert_eq!(row.get::<i64, _>("output_tokens"), 5000);
    assert_eq!(row.get::<i64, _>("cost_micros"), charged);
    assert_eq!(
        row.get::<Option<String>, _>("error_code").as_deref(),
        cancelled.then_some("client_cancelled")
    );
    assert_eq!(row.get::<i64, _>("ledgers"), 1);
    let account: (i64, i64) = sqlx::query_as(
        "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
    )
    .bind(key.account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(account, (balance - charged - 4000, 4000));
    let budget_hold: i64 =
        sqlx::query_scalar("SELECT reserved_micros FROM key_budget_state WHERE key_id = $1")
            .bind(key.key_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(budget_hold, 4000);
    assert_eq!(
        database.settle_usage(&other_hold, 0, 4000).await.unwrap(),
        4000
    );
    let account: (i64, i64) = sqlx::query_as(
        "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
    )
    .bind(key.account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(account, (balance - charged - 4000, 0));
}

#[tokio::test]
async fn optional_output_actual_usage_preserves_other_holds_budgets_and_once_finish() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("output-validity.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let recovered_database = Database::connect(&database_url).await.unwrap();
    for (balance, budget) in [(20_000, None), (9_000, None), (20_000, Some(9_000))] {
        for cancelled in [false, true] {
            optional_output_financial_contract(
                &database,
                &recovered_database,
                balance,
                budget,
                cancelled,
            )
            .await;
        }
    }
}

async fn snapshot_contract(database: &Database, metered: bool) {
    let unique = Uuid::now_v7().to_string();
    let pepper = b"immutable route price snapshot test pepper over 32 bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: unique.clone(),
                principal_external_id: "member".into(),
                alias: unique.clone(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    enforcement_mode: if metered {
                        EnforcementMode::MeteredUnlimited
                    } else {
                        EnforcementMode::Prepaid
                    },
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
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
    let first_model = format!("{unique}-first");
    let second_model = format!("{unique}-second");
    let first_price = database
        .upsert_model_price(&first_model, "USD", Decimal::ONE, Decimal::from(3))
        .await
        .unwrap();
    let second_price = database
        .upsert_model_price(&second_model, "USD", Decimal::from(3), Decimal::ONE)
        .await
        .unwrap();
    let first_assignment = (Uuid::now_v7(), Uuid::now_v7());
    let second_assignment = (Uuid::now_v7(), Uuid::now_v7());
    let older_request_id = Uuid::now_v7();
    let older_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: older_request_id,
            key: &key,
            price: &first_price,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol: "openai",
            model: "public-alias",
            request_object: "gap://legacy-price/request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let original = database
        .start_proxy_request_with_deferred_archive(
            StartProxyRequest {
                request_id,
                key: &key,
                price: &first_price,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                protocol: "openai",
                model: "public-alias",
                request_object: &format!("gap://{request_id}/request"),
                upstream_account_id: Some(first_assignment.0),
                model_route_id: Some(first_assignment.1),
            },
            &bytes::Bytes::from_static(b"{\"input\":\"snapshot\"}"),
            pepper,
            false,
            Some(&first_model),
            Some(10),
        )
        .await
        .unwrap()
        .reservation;
    database.drain_gateway_persistence_for_test().await;
    let switched = database
        .switch_pending_proxy_candidate(SwitchProxyCandidateInput {
            request_id,
            tenant_id: key.tenant_id,
            key: &key,
            price: &second_price,
            reservation: &original,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            output_token_limit: Some(10),
            upstream_model: &second_model,
            expected_assignment: first_assignment,
            next_assignment: second_assignment,
        })
        .await
        .unwrap();
    assert_eq!(original.id, switched.id);
    assert_eq!(original.reserved_micros, switched.reserved_micros);
    assert_eq!(original.reserved_tokens, switched.reserved_tokens);
    assert_eq!(
        switched.input_micros_per_million,
        second_price.input_micros_per_million
    );
    assert_eq!(
        switched.output_micros_per_million,
        second_price.output_micros_per_million
    );
    let row = sqlx::query("SELECT request.upstream_model, reservation.price_id, reservation.price_snapshot_json FROM request_records request JOIN usage_reservations reservation ON reservation.id = request.reservation_id WHERE request.id = $1")
        .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("upstream_model"), second_model);
    assert_eq!(
        row.get::<String, _>("price_id"),
        second_price.id.to_string()
    );
    let durable: ModelPrice =
        serde_json::from_str(&row.get::<String, _>("price_snapshot_json")).unwrap();
    assert_eq!(
        serde_json::to_value(durable).unwrap(),
        serde_json::to_value(&second_price).unwrap()
    );
    assert!(
        database
            .switch_pending_proxy_candidate(SwitchProxyCandidateInput {
                request_id,
                tenant_id: key.tenant_id,
                key: &key,
                price: &first_price,
                reservation: &original,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                output_token_limit: Some(10),
                upstream_model: &first_model,
                expected_assignment: first_assignment,
                next_assignment: first_assignment,
            })
            .await
            .is_err()
    );
    database
        .upsert_model_price(&second_model, "USD", Decimal::from(99), Decimal::from(99))
        .await
        .unwrap();
    let retained = database
        .switch_pending_proxy_candidate(SwitchProxyCandidateInput {
            request_id,
            tenant_id: key.tenant_id,
            key: &key,
            price: &first_price,
            reservation: &switched,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            output_token_limit: Some(10),
            upstream_model: &second_model,
            expected_assignment: second_assignment,
            next_assignment: second_assignment,
        })
        .await
        .unwrap();
    assert_eq!(
        retained.input_micros_per_million,
        second_price.input_micros_per_million
    );
    assert!(
        database
            .switch_pending_proxy_candidate(SwitchProxyCandidateInput {
                request_id,
                tenant_id: key.tenant_id,
                key: &key,
                price: &first_price,
                reservation: &retained,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                output_token_limit: Some(10),
                upstream_model: &first_model,
                expected_assignment: second_assignment,
                next_assignment: second_assignment,
            })
            .await
            .is_err()
    );
    let finish = FinishProxyRequest {
        terminal_cause: None,
        usage_basis: Some(crate::model::RequestUsageBasis::ProviderReported),
        first_output_ms: Some(1),
        generation_duration_ms: None,
        request_id,
        tenant_id: key.tenant_id,
        reservation: &original,
        input_token_ceiling: 10,
        output_token_ceiling: 10,
        requested_service_tier: None,
        status_code: 200,
        duration_ms: 10,
        usage: TokenUsage {
            input_tokens: 1,
            output_tokens: 9,
            ..TokenUsage::default()
        },
        error_code: None,
        response_object: "gap://snapshot/response",
        routing_session_id: None,
        routing_terminal_observed_at: None,
        conversation: None,
    };
    database.finish_proxy_request(finish.clone()).await.unwrap();
    assert!(matches!(
        database.finish_proxy_request(finish.clone()).await.unwrap(),
        FinishProxyRequestResult::AlreadyFinished { .. }
    ));
    let stored: i64 =
        sqlx::query_scalar("SELECT actual_micros FROM usage_reservations WHERE id = $1")
            .bind(original.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(stored, 12);
    let ledger: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger_entries WHERE kind = 'usage' AND source = $1",
    )
    .bind(original.id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(ledger, 1);
    if database.terminal_projection_enabled {
        let receipt_cost: i64 = sqlx::query_scalar("SELECT cost_micros FROM terminal_projection_outbox WHERE request_id = $1 AND projected_at IS NULL")
            .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
        assert_eq!(receipt_cost, 12);
        let owner = Uuid::now_v7();
        assert_eq!(
            database
                .claim_terminal_projection_tasks(owner, 32)
                .await
                .unwrap(),
            vec![request_id]
        );
        assert!(
            database
                .project_claimed_terminal_projection_task(owner, request_id)
                .await
                .unwrap()
        );
        assert!(
            !database
                .project_claimed_terminal_projection_task(owner, request_id)
                .await
                .unwrap()
        );
    } else if metered {
        let owner = Uuid::now_v7();
        let tasks = database
            .claim_metered_usage_projection_tasks(owner, 32)
            .await
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(
            database
                .project_claimed_metered_usage_projection_task(owner, original.id)
                .await
                .unwrap()
        );
    }
    let lifetime: i64 = sqlx::query_scalar(
        "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1",
    )
    .bind(key.account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(lifetime, 12);
    let projected_cost: i64 =
        sqlx::query_scalar("SELECT cost_micros FROM request_stats_facts WHERE request_id = $1")
            .bind(request_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(projected_cost, 12);
    let views = database.list_requests(key.key_id, 10).await.unwrap();
    assert_eq!(views[0].model, "public-alias");
    assert_eq!(
        views[0].upstream_model.as_deref(),
        Some(second_model.as_str())
    );
    let detail = database
        .request_archive_refs(key.key_id, request_id)
        .await
        .unwrap();
    assert_eq!(
        detail.view.upstream_model.as_deref(),
        Some(second_model.as_str())
    );
    let events = database
        .request_events_after(&unique, 0, None, 10)
        .await
        .unwrap();
    assert!(events.iter().any(|event| event.request_id == request_id
        && event.upstream_model.as_deref() == Some(second_model.as_str())));
    assert!(
        database
            .switch_pending_proxy_candidate(SwitchProxyCandidateInput {
                request_id,
                tenant_id: key.tenant_id,
                key: &key,
                price: &first_price,
                reservation: &switched,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                output_token_limit: Some(10),
                upstream_model: &first_model,
                expected_assignment: second_assignment,
                next_assignment: first_assignment,
            })
            .await
            .is_err()
    );
    database
        .upsert_model_price(&first_model, "USD", Decimal::from(77), Decimal::from(77))
        .await
        .unwrap();
    let mut older_finish = finish;
    older_finish.request_id = older_request_id;
    older_finish.reservation = &older_reservation;
    database.finish_proxy_request(older_finish).await.unwrap();
    let older_cost: i64 =
        sqlx::query_scalar("SELECT actual_micros FROM usage_reservations WHERE id = $1")
            .bind(older_reservation.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(older_cost, 28);
    if database.terminal_projection_enabled {
        let owner = Uuid::now_v7();
        assert_eq!(
            database
                .claim_terminal_projection_tasks(owner, 32)
                .await
                .unwrap(),
            vec![older_request_id]
        );
        assert!(
            database
                .project_claimed_terminal_projection_task(owner, older_request_id)
                .await
                .unwrap()
        );
    } else if metered {
        let owner = Uuid::now_v7();
        assert_eq!(
            database
                .claim_metered_usage_projection_tasks(owner, 32)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            database
                .project_claimed_metered_usage_projection_task(owner, older_reservation.id)
                .await
                .unwrap()
        );
    }
    let older_view = database
        .request_archive_refs(key.key_id, older_request_id)
        .await
        .unwrap();
    assert_eq!(older_view.view.upstream_model, None);
    assert_eq!(older_view.view.cost, "0.000028");
    let total_usage: i64 = sqlx::query_scalar(
        "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1",
    )
    .bind(key.account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(total_usage, 40);
}

#[tokio::test]
async fn sqlite_same_amount_failover_replaces_price_snapshot_not_admission() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("snapshot.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    single_checkout_price_snapshot(&database).await;
    for deferred in [false, true] {
        for metered in [false, true] {
            snapshot_contract(
                &database.clone().with_terminal_projection_enabled(deferred),
                metered,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn postgres_same_amount_failover_replaces_price_snapshot_not_admission() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    let schema = format!("route_snapshot_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&database_url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect_with_max(isolated.as_str(), 4)
        .await
        .unwrap();
    database.migrate().await.unwrap();
    single_checkout_price_snapshot(&database).await;
    for deferred in [false, true] {
        for metered in [false, true] {
            snapshot_contract(
                &database.clone().with_terminal_projection_enabled(deferred),
                metered,
            )
            .await;
        }
    }
    let recovered_database = Database::connect_with_max(isolated.as_str(), 4)
        .await
        .unwrap();
    for (balance, budget) in [(20_000, None), (9_000, None), (20_000, Some(9_000))] {
        for cancelled in [false, true] {
            optional_output_financial_contract(
                &database,
                &recovered_database,
                balance,
                budget,
                cancelled,
            )
            .await;
        }
    }
    recovered_database.close().await;
    database.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}
