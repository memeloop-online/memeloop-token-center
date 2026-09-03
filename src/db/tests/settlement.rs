use super::super::*;
use crate::conversation::ConversationHints;

#[test]
fn responses_auto_tier_alias_settles_against_the_admitted_default_contract() {
    let usage = TokenUsage {
        input_tokens: 10_008,
        output_tokens: 13,
        service_tier: Some("auto".to_owned()),
        ..TokenUsage::default()
    };

    let omitted = normalize_proxy_usage(&usage, 44_471, 4_096, None).unwrap();
    assert_eq!(omitted.service_tier, None);

    let default = normalize_proxy_usage(&usage, 44_471, 4_096, Some("default")).unwrap();
    assert_eq!(default.service_tier.as_deref(), Some("default"));

    assert!(normalize_proxy_usage(&usage, 44_471, 4_096, Some("flex")).is_err());
    let explicit_auto = normalize_proxy_usage(&usage, 44_471, 4_096, Some("auto")).unwrap();
    assert_eq!(explicit_auto.service_tier.as_deref(), Some("auto"));
}

#[tokio::test]
async fn rate_window_cleanup_is_composite_keyed_and_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("rate-window-cleanup.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let old = unix_millis().saturating_sub(3 * 86_400_000);
    sqlx::query("INSERT INTO rate_limit_windows (key_id, window_start, requests, tokens) VALUES ('b', $1, 1, 1), ('a', $2, 1, 1), ('c', $3, 1, 1), ('current', $4, 1, 1)")
            .bind(old)
            .bind(old)
            .bind(old.saturating_add(1))
            .bind(unix_millis())
            .execute(&database.pool)
            .await
            .unwrap();
    assert_eq!(database.delete_expired_rate_windows(2).await.unwrap(), 2);
    let remaining =
        sqlx::query("SELECT key_id FROM rate_limit_windows ORDER BY window_start, key_id")
            .fetch_all(&database.pool)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.get::<String, _>("key_id"))
            .collect::<Vec<_>>();
    assert_eq!(remaining, vec!["c", "current"]);
}

#[tokio::test]
async fn concurrent_budget_reservations_and_settlement_replays_are_exact() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("budget-concurrency.db").display()
    );
    let database = Database::connect_with_max(&database_url, 8).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"a budget test pepper longer than thirty-two bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "budget-concurrency".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "budget-concurrency".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    tokens_per_minute: 10_000,
                    daily_budget: Some("0.001".to_owned()),
                    weekly_budget: Some("0.001".to_owned()),
                    lifetime_budget: Some("0.001".to_owned()),
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ONE,
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
        .upsert_model_price("budget-concurrency", "USD", Decimal::ZERO, Decimal::ONE)
        .await
        .unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let database = database.clone();
        let key = key.clone();
        let price = price.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database.reserve_usage(&key, &price, 0, 600).await
        }));
    }
    let mut reservation = None;
    let mut rejected = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(value) => reservation = Some(value),
            Err(AppError::LimitExceeded { .. }) => rejected += 1,
            result => panic!("unexpected reservation result: {result:?}"),
        }
    }
    assert_eq!(rejected, 1);
    let reservation = reservation.unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let reservation = reservation.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database.settle_usage(&reservation, 0, 700).await
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap(), 700);
    }
    let state = sqlx::query(
        "SELECT settled_lifetime_micros, reserved_micros FROM key_budget_state WHERE key_id = $1",
    )
    .bind(issued.key_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(state.get::<i64, _>("settled_lifetime_micros"), 700);
    assert_eq!(state.get::<i64, _>("reserved_micros"), 0);
    let ledger_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS count FROM ledger_entries WHERE key_id = $1 AND kind = 'usage'",
    )
    .bind(issued.key_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap()
    .get("count");
    assert_eq!(ledger_count, 1);
}

#[tokio::test]
async fn metered_usage_projection_is_exactly_once_and_skips_prepaid_hot_rows() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("metered-projection.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"a metered projection test pepper longer than thirty-two bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "metered-projection".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "metered-projection".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    enforcement_mode: EnforcementMode::MeteredUnlimited,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ONE,
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
        .upsert_model_price("metered-projection", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 1,
            output_token_ceiling: 1,
            protocol: "openai",
            model: "metered-projection",
            request_object: "gap://metered-projection/request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let request_json = serde_json::json!({"input": [{"role": "user", "content": "metered"}]});
    let hints = ConversationHints {
        session_id: Some("metered-projection-session".to_owned()),
        ..ConversationHints::default()
    };
    database
        .finish_proxy_request(FinishProxyRequest {
            request_id,
            tenant_id: key.tenant_id,
            reservation: &reservation,
            input_token_ceiling: 1,
            output_token_ceiling: 1,
            requested_service_tier: None,
            status_code: 200,
            duration_ms: 12,
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                ..TokenUsage::default()
            },
            charge_contract_ceiling: false,
            error_code: None,
            response_object: "gap://metered-projection/response",
            conversation: Some(ProxyConversationInput {
                key: &key,
                request_json: &request_json,
                hints: &hints,
                client_name: None,
                upstream_response_id: None,
            }),
        })
        .await
        .unwrap();

    // Project conversation first to force the cross-queue order in which the
    // session projector has already materialized this fact before the metered
    // statistics projector acknowledges its own task.
    let conversation_projector = Uuid::now_v7();
    let conversation_tasks = database
        .claim_conversation_projection_tasks(conversation_projector, 32)
        .await
        .unwrap();
    assert_eq!(conversation_tasks.len(), 1);
    assert!(
        database
            .project_claimed_conversation_projection_task(conversation_projector, request_id)
            .await
            .unwrap()
    );

    let projector = Uuid::now_v7();
    let tasks = database
        .claim_metered_usage_projection_tasks(projector, 32)
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].reservation_id, reservation.id);
    assert!(
        database
            .project_claimed_metered_usage_projection_task(projector, reservation.id)
            .await
            .unwrap()
    );
    assert!(
        !database
            .project_claimed_metered_usage_projection_task(projector, reservation.id)
            .await
            .unwrap()
    );
    assert!(
        database
            .claim_metered_usage_projection_tasks(Uuid::now_v7(), 32)
            .await
            .unwrap()
            .is_empty()
    );

    let projection: (i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT COUNT(*) FROM metered_usage_projection_outbox WHERE reservation_id = $1 AND projected_at IS NOT NULL),
            (SELECT COALESCE(SUM(requests), 0) FROM usage_daily_aggregates WHERE key_id = $2),
            (SELECT COALESCE(SUM(requests), 0) FROM request_daily_aggregates WHERE key_id = $3),
            (SELECT COALESCE(SUM(requests), 0) FROM usage_analysis_hourly WHERE key_id = $4 AND source_kind = 'request'),
            (SELECT COALESCE(SUM(requests), 0) FROM usage_analysis_daily WHERE key_id = $5 AND source_kind = 'request'),
            (SELECT COALESCE(SUM(requests), 0) FROM session_usage_totals WHERE key_id = $6),
            (SELECT settled_lifetime_micros FROM key_budget_state WHERE key_id = $7),
            (SELECT available_micros FROM credit_accounts WHERE id = $8)",
    )
    .bind(reservation.id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(issued.account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(projection, (1, 1, 1, 1, 1, 1, 0, 1_000_000));
}

#[tokio::test]
async fn maintenance_releases_old_unlinked_reservations() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("orphan-reservation.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"a downstream key pepper longer than thirty-two bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "tenant".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "orphan-test".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ONE,
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
        .upsert_model_price("orphan-model", "USD", Decimal::ZERO, Decimal::ONE)
        .await
        .unwrap();
    let reservation = database
        .reserve_usage(&key, &price, 0, 1_000)
        .await
        .unwrap();
    assert_eq!(reservation.reserved_micros, 1_000);
    let reserved_account =
        sqlx::query("SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1")
            .bind(issued.account_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(reserved_account.get::<i64, _>("available_micros"), 999_000);
    assert_eq!(reserved_account.get::<i64, _>("reserved_micros"), 1_000);
    sqlx::query("UPDATE usage_reservations SET created_at = $1 WHERE id = $2")
        .bind(unix_millis().saturating_sub(31 * 60 * 1_000))
        .bind(reservation.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();

    assert_eq!(
        database.release_orphaned_reservations(100).await.unwrap(),
        1
    );
    let reservation_row =
        sqlx::query("SELECT status, actual_micros FROM usage_reservations WHERE id = $1")
            .bind(reservation.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(reservation_row.get::<String, _>("status"), "settled");
    assert_eq!(reservation_row.get::<i64, _>("actual_micros"), 0);
    let account_row =
        sqlx::query("SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1")
            .bind(issued.account_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(account_row.get::<i64, _>("available_micros"), 1_000_000);
    assert_eq!(account_row.get::<i64, _>("reserved_micros"), 0);

    let linked_reservation = database
        .reserve_usage(&key, &price, 0, 1_000)
        .await
        .unwrap();
    let linked_request_id = Uuid::now_v7();
    database
        .record_request_started(NewRequest {
            request_id: linked_request_id,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: "openai".to_owned(),
            model: "orphan-model".to_owned(),
            request_object: format!("gap://{linked_request_id}/request"),
            reservation_id: linked_reservation.id,
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    sqlx::query("UPDATE usage_reservations SET created_at = $1 WHERE id = $2")
        .bind(unix_millis().saturating_sub(31 * 60 * 1_000))
        .bind(linked_reservation.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        database.release_orphaned_reservations(100).await.unwrap(),
        1
    );
    let expired_request = sqlx::query(
        "SELECT status_code, error_code, completed_at FROM request_records WHERE id = $1",
    )
    .bind(linked_request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(expired_request.get::<i64, _>("status_code"), 504);
    assert_eq!(
        expired_request.get::<String, _>("error_code"),
        "request_expired"
    );
    assert!(
        expired_request
            .get::<Option<i64>, _>("completed_at")
            .is_some()
    );

    let overage_reservation = database
        .reserve_usage(&key, &price, 0, 1_000)
        .await
        .unwrap();
    assert_eq!(
        database
            .settle_usage(&overage_reservation, 0, 2_000)
            .await
            .unwrap(),
        2_000
    );
    let overage_account =
        sqlx::query("SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1")
            .bind(issued.account_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(overage_account.get::<i64, _>("available_micros"), 998_000);
    assert_eq!(overage_account.get::<i64, _>("reserved_micros"), 0);

    let capped_reservation = database
        .reserve_usage(&key, &price, 0, 1_000)
        .await
        .unwrap();
    assert!(matches!(
        database
            .settle_usage(&capped_reservation, 0, 2_000_000_000)
            .await,
        Err(AppError::BadRequest(_))
    ));
    assert_eq!(
        database
            .settle_usage(&capped_reservation, 0, 1_000_000_000)
            .await
            .unwrap(),
        998_000
    );
    let capped_account =
        sqlx::query("SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1")
            .bind(issued.account_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(capped_account.get::<i64, _>("available_micros"), 0);
    assert_eq!(capped_account.get::<i64, _>("reserved_micros"), 0);
}

#[tokio::test]
async fn settlement_cannot_cross_a_hard_lifetime_budget() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("settlement-budget.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"a downstream key pepper longer than thirty-two bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "budget-tenant".to_owned(),
                principal_external_id: "budget-member".to_owned(),
                alias: "hard-budget".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    allowed_models: vec!["budget-model".to_owned()],
                    lifetime_budget: Some("0.0015".to_owned()),
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ONE,
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
        .upsert_model_price("budget-model", "USD", Decimal::ZERO, Decimal::ONE)
        .await
        .unwrap();
    let reservation = database
        .reserve_usage(&key, &price, 0, 1_000)
        .await
        .unwrap();

    assert_eq!(
        database.settle_usage(&reservation, 0, 2_000).await.unwrap(),
        1_500
    );
    assert!(matches!(
        database.reserve_usage(&key, &price, 0, 1).await,
        Err(AppError::LimitExceeded { .. })
    ));
    let account =
        sqlx::query("SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1")
            .bind(issued.account_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(account.get::<i64, _>("available_micros"), 998_500);
    assert_eq!(account.get::<i64, _>("reserved_micros"), 0);
}

#[tokio::test]
async fn settlement_uses_cache_and_service_tier_price_snapshots() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("tier-pricing.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"a downstream key pepper longer than thirty-two bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "tier-tenant".to_owned(),
                principal_external_id: "tier-member".to_owned(),
                alias: "tier-pricing".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    tokens_per_minute: 2_000_000,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::from(10),
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
    database
        .upsert_model_price_tier(
            "tier-model",
            "USD",
            "default",
            Decimal::ONE,
            Decimal::new(1, 1),
            Decimal::TWO,
            Decimal::from(3),
            false,
        )
        .await
        .unwrap();
    let price = database
        .upsert_model_price_tier(
            "tier-model",
            "USD",
            "priority",
            Decimal::from(5),
            Decimal::from(5),
            Decimal::from(5),
            Decimal::from(6),
            false,
        )
        .await
        .unwrap();
    let reservation = database
        .reserve_usage(&key, &price, 300_000, 100_000)
        .await
        .unwrap();
    assert_eq!(reservation.reserved_micros, 2_100_000);
    assert_eq!(reservation.price_tiers.len(), 2);
    let snapshot: String =
        sqlx::query("SELECT price_snapshot_json FROM usage_reservations WHERE id = $1")
            .bind(reservation.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap()
            .try_get("price_snapshot_json")
            .unwrap();
    assert!(snapshot.contains("cached_input_micros_per_million"));

    let cost = database
        .settle_token_usage(
            &reservation,
            &TokenUsage {
                input_tokens: 100_000,
                cached_input_tokens: 100_000,
                cache_write_tokens: 100_000,
                output_tokens: 100_000,
                service_tier: Some("default".to_owned()),
            },
        )
        .await
        .unwrap();
    assert_eq!(cost, 610_000);

    let conservative = database
        .reserve_usage(&key, &price, 300_000, 100_000)
        .await
        .unwrap();
    let cost = database
        .settle_token_usage(
            &conservative,
            &TokenUsage {
                input_tokens: 100_000,
                cached_input_tokens: 100_000,
                cache_write_tokens: 100_000,
                output_tokens: 100_000,
                service_tier: Some("flex".to_owned()),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        cost, 2_100_000,
        "unknown response tiers use snapshot maxima"
    );
}
