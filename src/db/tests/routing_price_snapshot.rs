use super::super::*;

async fn snapshot_contract(database: &Database, metered: bool) {
    let unique = Uuid::now_v7().to_string();
    let pepper = b"immutable route price snapshot test";
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
    let request_id = Uuid::now_v7();
    let original = database
        .start_proxy_forwarding_request(
            StartProxyRequest {
                request_id,
                key: &key,
                price: &first_price,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                protocol: "openai",
                model: "public-alias",
                request_object: "gap://snapshot/request",
                upstream_account_id: Some(first_assignment.0),
                model_route_id: Some(first_assignment.1),
            },
            Some(&first_model),
        )
        .await
        .unwrap();
    let switched = database
        .switch_pending_proxy_candidate(SwitchProxyCandidateInput {
            request_id,
            tenant_id: key.tenant_id,
            key: &key,
            price: &second_price,
            reservation: &original,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
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
        database.finish_proxy_request(finish).await.unwrap(),
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
                upstream_model: &first_model,
                expected_assignment: second_assignment,
                next_assignment: first_assignment,
            })
            .await
            .is_err()
    );
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
    for metered in [false, true] {
        snapshot_contract(&database, metered).await;
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
    for metered in [false, true] {
        snapshot_contract(&database, metered).await;
    }
    database.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}
