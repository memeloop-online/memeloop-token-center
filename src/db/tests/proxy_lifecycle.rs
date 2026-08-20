use super::super::*;

#[tokio::test]
async fn proxy_lifecycle_is_atomic_fault_safe_and_exactly_replayable() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("proxy-lifecycle-atomic.db").display()
    );
    let database = Database::connect_with_max(&database_url, 4).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"proxy lifecycle atomic test pepper value";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "proxy-atomic".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "proxy-atomic".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    tokens_per_minute: 100_000,
                    max_concurrency: 8,
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
    let price = database
        .upsert_model_price("atomic-model", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let request_digest = "a".repeat(64);
    let staging_request = format!("staging://blake3/{request_digest}");
    let archived_request = format!("staging/proxy/{request_id}/request.bin");
    let reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 100,
            output_token_ceiling: 100,
            protocol: "openai",
            model: "atomic-model",
            request_object: &staging_request,
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        database
            .attach_proxy_request_archive(
                request_id,
                key.tenant_id,
                reservation.id,
                &staging_request,
                &archived_request,
            )
            .await
            .unwrap(),
        AttachProxyArchiveResult::Attached
    );
    assert_eq!(
        database
            .attach_proxy_request_archive(
                request_id,
                key.tenant_id,
                reservation.id,
                &staging_request,
                &archived_request,
            )
            .await
            .unwrap(),
        AttachProxyArchiveResult::AlreadyAttached
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM usage_reservations WHERE id = $1")
            .bind(reservation.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        "reserved"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM conversation_observations WHERE request_id = $1"
        )
        .bind(request_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap(),
        0,
        "lineage must not exist before the terminal transaction"
    );

    assert!(matches!(
        database
            .start_proxy_request(StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 100,
                output_token_ceiling: 100,
                protocol: "openai",
                model: "atomic-model",
                request_object: "objects/blake3/duplicate",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await,
        Err(AppError::BadRequest(_))
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_reservations")
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1 AND status = 'reserved'"
        )
        .bind(key.key_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap(),
        1
    );

    // Test-only SQL safety boundary: `request_id` is a typed UUID generated in this test and its
    // Display representation cannot contain SQL syntax. SQLite does not allow a bind parameter
    // in a persisted trigger definition.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER proxy_terminal_fault BEFORE INSERT ON request_stats_facts WHEN NEW.request_id = '{}' BEGIN SELECT RAISE(ABORT, 'proxy terminal fault'); END",
        request_id
    )))
        .execute(&database.pool)
        .await
        .unwrap();
    let request_json = serde_json::json!({
        "model": "atomic-model",
        "input": [{"role": "user", "content": "atomic terminal"}]
    });
    let hints = ConversationHints {
        session_id: Some("atomic-session".to_owned()),
        ..ConversationHints::default()
    };
    let usage = TokenUsage {
        input_tokens: 10,
        output_tokens: 5,
        ..TokenUsage::default()
    };
    let finish = || FinishProxyRequest {
        request_id,
        tenant_id: key.tenant_id,
        reservation: &reservation,
        input_token_ceiling: 100,
        output_token_ceiling: 100,
        requested_service_tier: None,
        status_code: 200,
        duration_ms: 12,
        usage: usage.clone(),
        charge_contract_ceiling: false,
        error_code: None,
        response_object: "objects/blake3/atomic-response",
        conversation: Some(ProxyConversationInput {
            key: &key,
            request_json: &request_json,
            hints: &hints,
            client_name: Some("codex"),
            upstream_response_id: Some("resp-atomic"),
        }),
    };
    assert!(database.finish_proxy_request(finish()).await.is_err());
    let rollback = sqlx::query(
            "SELECT r.status AS reservation_status, q.completed_at, q.status_code, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = r.id) AS ledger_count, (SELECT COUNT(*) FROM request_stats_facts f WHERE f.request_id = q.id) AS fact_count, (SELECT COUNT(*) FROM conversation_observations o WHERE o.request_id = q.id) AS observation_count FROM usage_reservations r JOIN request_records q ON q.reservation_id = r.id WHERE r.id = $1",
        )
        .bind(reservation.id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(rollback.get::<String, _>("reservation_status"), "reserved");
    assert_eq!(rollback.get::<Option<i64>, _>("completed_at"), None);
    assert_eq!(rollback.get::<Option<i64>, _>("status_code"), None);
    for field in ["ledger_count", "fact_count", "observation_count"] {
        assert_eq!(rollback.get::<i64, _>(field), 0, "{field}");
    }
    sqlx::query("DROP TRIGGER proxy_terminal_fault")
        .execute(&database.pool)
        .await
        .unwrap();

    assert!(matches!(
        database.finish_proxy_request(finish()).await.unwrap(),
        FinishProxyRequestResult::Finished {
            usage_invalid: false,
            ..
        }
    ));
    assert!(matches!(
        database.finish_proxy_request(finish()).await.unwrap(),
        FinishProxyRequestResult::AlreadyFinished {
            status_code: 200,
            ..
        }
    ));
    let committed = sqlx::query(
            "SELECT r.status AS reservation_status, q.status_code, q.input_tokens, q.output_tokens, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = r.id) AS ledger_count, (SELECT COUNT(*) FROM request_stats_facts f WHERE f.request_id = q.id) AS fact_count, (SELECT COUNT(*) FROM request_daily_aggregates) AS aggregate_count, (SELECT COUNT(*) FROM request_events e WHERE e.request_id = q.id AND e.event_kind = 'finished') AS finished_events, (SELECT COUNT(*) FROM conversation_observations o WHERE o.request_id = q.id AND o.upstream_response_id = 'resp-atomic') AS observation_count FROM usage_reservations r JOIN request_records q ON q.reservation_id = r.id WHERE r.id = $1",
        )
        .bind(reservation.id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(committed.get::<String, _>("reservation_status"), "settled");
    assert_eq!(committed.get::<i64, _>("status_code"), 200);
    assert_eq!(committed.get::<i64, _>("input_tokens"), 10);
    assert_eq!(committed.get::<i64, _>("output_tokens"), 5);
    for field in [
        "ledger_count",
        "fact_count",
        "aggregate_count",
        "finished_events",
        "observation_count",
    ] {
        assert_eq!(committed.get::<i64, _>(field), 1, "{field}");
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1 AND status = 'reserved'"
        )
        .bind(key.key_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap(),
        0,
        "a successful terminal replay must release active concurrency exactly once"
    );

    let invalid_request_id = Uuid::now_v7();
    let invalid_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: invalid_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol: "openai",
            model: "atomic-model",
            request_object: "objects/blake3/invalid-usage-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        database
            .finish_proxy_request(FinishProxyRequest {
                request_id: invalid_request_id,
                tenant_id: key.tenant_id,
                reservation: &invalid_reservation,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                requested_service_tier: None,
                status_code: 200,
                duration_ms: 1,
                usage: TokenUsage {
                    input_tokens: 11,
                    output_tokens: 1,
                    ..TokenUsage::default()
                },
                charge_contract_ceiling: false,
                error_code: None,
                response_object: "objects/blake3/untrusted-invalid-usage-response",
                conversation: None,
            })
            .await
            .unwrap(),
        FinishProxyRequestResult::Finished {
            cost_micros: 0,
            usage_invalid: true
        }
    ));
    let invalid_terminal = sqlx::query(
            "SELECT status_code, error_code, input_tokens, output_tokens, cost_micros, response_object FROM request_records WHERE id = $1",
        )
        .bind(invalid_request_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(invalid_terminal.get::<i64, _>("status_code"), 502);
    assert_eq!(
        invalid_terminal.get::<String, _>("error_code"),
        "upstream_invalid_usage"
    );
    assert_eq!(invalid_terminal.get::<i64, _>("input_tokens"), 0);
    assert_eq!(invalid_terminal.get::<i64, _>("output_tokens"), 0);
    assert_eq!(invalid_terminal.get::<i64, _>("cost_micros"), 0);
    assert!(
        !invalid_terminal
            .get::<String, _>("response_object")
            .contains("untrusted-invalid-usage-response")
    );

    let expensive_price = database
        .upsert_model_price_tier(
            "full-contract-model",
            "USD",
            "default",
            Decimal::ONE,
            Decimal::ONE,
            Decimal::from(100),
            Decimal::ONE,
            false,
        )
        .await
        .unwrap();
    let delivered_request_id = Uuid::now_v7();
    let delivered_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: delivered_request_id,
            key: &key,
            price: &expensive_price,
            input_token_ceiling: 10,
            output_token_ceiling: 2,
            protocol: "openai",
            model: "full-contract-model",
            request_object: "objects/blake3/delivered-failure-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let delivered = database
        .finish_proxy_request(FinishProxyRequest {
            request_id: delivered_request_id,
            tenant_id: key.tenant_id,
            reservation: &delivered_reservation,
            input_token_ceiling: 10,
            output_token_ceiling: 2,
            requested_service_tier: None,
            status_code: 502,
            duration_ms: 1,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 2,
                ..TokenUsage::default()
            },
            charge_contract_ceiling: true,
            error_code: Some("upstream_incomplete_response"),
            response_object: "objects/blake3/delivered-failure-response",
            conversation: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        delivered,
        FinishProxyRequestResult::Finished {
            cost_micros,
            usage_invalid: false
        } if cost_micros == delivered_reservation.reserved_micros
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT actual_micros FROM usage_reservations WHERE id = $1")
            .bind(delivered_reservation.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        delivered_reservation.reserved_micros,
        "delivered failures must not release a higher cache-write reservation"
    );

    database
        .upsert_model_price_tier(
            "tier-contract-model",
            "USD",
            "default",
            Decimal::ONE,
            Decimal::ONE,
            Decimal::ONE,
            Decimal::ONE,
            false,
        )
        .await
        .unwrap();
    database
        .upsert_model_price_tier(
            "tier-contract-model",
            "USD",
            "flex",
            Decimal::from(2),
            Decimal::from(2),
            Decimal::from(2),
            Decimal::from(2),
            false,
        )
        .await
        .unwrap();
    let tiered_price = database
        .upsert_model_price_tier(
            "tier-contract-model",
            "USD",
            "priority",
            Decimal::from(100),
            Decimal::from(100),
            Decimal::from(100),
            Decimal::from(100),
            false,
        )
        .await
        .unwrap();
    let flex_request_id = Uuid::now_v7();
    let flex_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: flex_request_id,
            key: &key,
            price: &tiered_price,
            input_token_ceiling: 10,
            output_token_ceiling: 2,
            protocol: "openai",
            model: "tier-contract-model",
            request_object: "objects/blake3/flex-delivered-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    assert!(flex_reservation.reserved_micros > 24);
    assert!(matches!(
        database
            .finish_proxy_request(FinishProxyRequest {
                request_id: flex_request_id,
                tenant_id: key.tenant_id,
                reservation: &flex_reservation,
                input_token_ceiling: 10,
                output_token_ceiling: 2,
                requested_service_tier: Some("flex"),
                status_code: 502,
                duration_ms: 1,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 2,
                    ..TokenUsage::default()
                },
                charge_contract_ceiling: true,
                error_code: Some("upstream_incomplete_response"),
                response_object: "objects/blake3/flex-delivered-response",
                conversation: None,
            })
            .await
            .unwrap(),
        FinishProxyRequestResult::Finished {
            cost_micros: 24,
            usage_invalid: false,
        }
    ));
}

#[tokio::test]
async fn concurrent_proxy_terminal_owners_settle_and_link_once() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("proxy-terminal-race.db").display()
    );
    let database = Database::connect_with_max(&database_url, 8).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"proxy terminal race test pepper value";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "proxy-race".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "proxy-race".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    tokens_per_minute: 100_000,
                    max_concurrency: 8,
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
    let price = database
        .upsert_model_price("race-model", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 100,
            output_token_ceiling: 100,
            protocol: "openai",
            model: "race-model",
            request_object: "objects/blake3/race-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(6));
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let database = database.clone();
        let key = key.clone();
        let reservation = reservation.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            let request_json = serde_json::json!({
                "model": "race-model",
                "messages": [{"role": "user", "content": "race"}]
            });
            let hints = ConversationHints::default();
            barrier.wait().await;
            database
                .finish_proxy_request(FinishProxyRequest {
                    request_id,
                    tenant_id: key.tenant_id,
                    reservation: &reservation,
                    input_token_ceiling: 100,
                    output_token_ceiling: 100,
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
                    response_object: "objects/blake3/race-response",
                    conversation: Some(ProxyConversationInput {
                        key: &key,
                        request_json: &request_json,
                        hints: &hints,
                        client_name: None,
                        upstream_response_id: Some("resp-race"),
                    }),
                })
                .await
        }));
    }
    let mut winners = 0;
    let mut replays = 0;
    for task in tasks {
        match task.await.unwrap().unwrap() {
            FinishProxyRequestResult::Finished { .. } => winners += 1,
            FinishProxyRequestResult::AlreadyFinished { .. } => replays += 1,
        }
    }
    assert_eq!(winners, 1);
    assert_eq!(replays, 5);
    let counts = sqlx::query(
            "SELECT (SELECT COUNT(*) FROM ledger_entries WHERE source = $1) AS ledger_count, (SELECT COUNT(*) FROM request_stats_facts WHERE request_id = $2) AS fact_count, (SELECT COUNT(*) FROM conversation_observations WHERE request_id = $3) AS observation_count, (SELECT COUNT(*) FROM request_events WHERE request_id = $4 AND event_kind = 'finished') AS event_count",
        )
        .bind(reservation.id.to_string())
        .bind(request_id.to_string())
        .bind(request_id.to_string())
        .bind(request_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    for field in [
        "ledger_count",
        "fact_count",
        "observation_count",
        "event_count",
    ] {
        assert_eq!(counts.get::<i64, _>(field), 1, "{field}");
    }
}
