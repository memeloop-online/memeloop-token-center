use super::*;

#[test]
fn optional_openai_output_limits_are_not_injected_and_explicit_values_are_preserved() {
    for (protocol, fields) in [
        (
            Protocol::OpenAiChat,
            &["max_tokens", "max_completion_tokens"][..],
        ),
        (Protocol::OpenAiResponses, &["max_output_tokens"][..]),
    ] {
        let original = json!({"model":"fixture-model"});
        let mut request = original.clone();
        assert_eq!(
            inject_controlled_output_ceiling(protocol, &mut request).unwrap(),
            None
        );
        assert_eq!(request, original);
        for field in fields {
            for limit in [0, 17, 65_536, MAX_REPORTED_TOKENS] {
                let mut request = original.clone();
                request[*field] = json!(limit);
                let expected = request.clone();
                assert_eq!(
                    inject_controlled_output_ceiling(protocol, &mut request).unwrap(),
                    Some(limit)
                );
                assert_eq!(request, expected);
            }
            for invalid in [json!(-1), json!(1.5), json!("17"), Value::Null] {
                let mut request = original.clone();
                request[*field] = invalid;
                assert!(inject_controlled_output_ceiling(protocol, &mut request).is_err());
            }
        }
    }
    assert!(
        inject_controlled_output_ceiling(
            Protocol::OpenAiChat,
            &mut json!({"max_tokens":17,"max_completion_tokens":17}),
        )
        .is_err()
    );
}

#[test]
fn mandatory_anthropic_output_limit_keeps_its_existing_default_and_explicit_value() {
    let mut request = json!({"model":"fixture-model"});
    assert_eq!(
        inject_controlled_output_ceiling(Protocol::AnthropicMessages, &mut request).unwrap(),
        Some(4096)
    );
    assert_eq!(request["max_tokens"], 4096);
    request["max_tokens"] = json!(8192);
    let expected = request.clone();
    assert_eq!(
        inject_controlled_output_ceiling(Protocol::AnthropicMessages, &mut request).unwrap(),
        Some(8192)
    );
    assert_eq!(request, expected);
    for protocol in [Protocol::OpenAiEmbeddings, Protocol::AnthropicCountTokens] {
        let mut request = json!({"model":"fixture-model"});
        let expected = request.clone();
        assert_eq!(
            inject_controlled_output_ceiling(protocol, &mut request).unwrap(),
            Some(0)
        );
        assert_eq!(request, expected);
    }
}

fn ordinary_request(fixture: &CodexRouteFixture, chat: bool) -> Value {
    if chat {
        json!({"model":fixture.model,"messages":[{"role":"user","content":"hello"}],"stream":false})
    } else {
        json!({"model":fixture.model,"input":"hello","stream":false})
    }
}

fn ordinary_response(chat: bool, output_tokens: i64) -> Value {
    if chat {
        json!({
            "id":"resp-usage-contract","object":"chat.completion",
            "choices":[{"index":0,"message":{"role":"assistant","content":"uncapped response"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":output_tokens,"total_tokens":3 + output_tokens}
        })
    } else {
        completed_response_with_usage(3, output_tokens)
    }
}

async fn send_ordinary(fixture: &CodexRouteFixture, chat: bool, request: &Value) -> Response {
    if chat {
        send_chat_usage_request(fixture, request).await
    } else {
        send_response_usage_request(fixture, request).await
    }
}

fn ordinary_sse(chat: bool, model: &str, output_tokens: i64) -> String {
    if chat {
        let delta = json!({"id":"resp-usage-contract","object":"chat.completion.chunk","model":model,"choices":[{"index":0,"delta":{"role":"assistant","content":"uncapped response"},"finish_reason":null}]});
        let stop = json!({"id":"resp-usage-contract","object":"chat.completion.chunk","model":model,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
        let usage = json!({"id":"resp-usage-contract","object":"chat.completion.chunk","model":model,"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":output_tokens,"total_tokens":3 + output_tokens}});
        format!("data: {delta}\n\ndata: {stop}\n\ndata: {usage}\n\ndata: [DONE]\n\n")
    } else {
        let completed = completed_response_with_usage(3, output_tokens);
        format!(
            "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-usage-contract\"}}}}\n\ndata: {{\"type\":\"response.completed\",\"response\":{completed}}}\n\ndata: [DONE]\n\n"
        )
    }
}

#[tokio::test]
async fn ordinary_missing_metadata_delivers_and_settles_over_estimate_with_bounded_charges() {
    for driver in ["http-json", "new-api"] {
        for chat in [false, true] {
            for stream in [false, true] {
                for balance in [100_000_i64, 4_800] {
                    let upstream = MockServer::start().await;
                    let fixture = response_usage_fixture_with_uri_contract_and_driver(
                        "missing-output",
                        upstream.uri(),
                        0,
                        Some("openai-chat-usage-only"),
                        driver,
                    )
                    .await;
                    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
                    sqlx::query("UPDATE credit_accounts SET available_micros = $1 WHERE id = $2")
                        .bind(balance)
                        .bind(fixture.credit_account_id.to_string())
                        .execute(&pool)
                        .await
                        .unwrap();
                    let config: String = sqlx::query_scalar(
                        "SELECT config_json FROM upstream_accounts WHERE id = $1",
                    )
                    .bind(fixture.upstream_account_id.to_string())
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                    assert!(
                        serde_json::from_str::<Value>(&config)
                            .unwrap()
                            .get("reservation_token_bounds")
                            .is_none()
                    );
                    let mut request = ordinary_request(&fixture, chat);
                    request["stream"] = json!(stream);
                    if chat && stream {
                        request["stream_options"] = json!({"include_usage":true});
                    }
                    let expected_wire = request.clone();
                    let template = if stream {
                        ResponseTemplate::new(200).set_body_raw(
                            ordinary_sse(chat, &fixture.model, 5000),
                            "text/event-stream",
                        )
                    } else {
                        ResponseTemplate::new(200).set_body_json(ordinary_response(chat, 5000))
                    };
                    Mock::given(method("POST"))
                        .and(path(if chat {
                            "/v1/chat/completions"
                        } else {
                            "/v1/responses"
                        }))
                        .and(move |request: &wiremock::Request| {
                            request.body_json::<Value>().unwrap() == expected_wire
                        })
                        .respond_with(template)
                        .expect(1)
                        .mount(&upstream)
                        .await;
                    let response = send_ordinary(&fixture, chat, &request).await;
                    assert_eq!(
                        response.status(),
                        StatusCode::OK,
                        "{driver} chat={chat} stream={stream} balance={balance}"
                    );
                    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                        .await
                        .unwrap();
                    if stream {
                        assert_eq!(
                            String::from_utf8(delivered.to_vec()).unwrap(),
                            ordinary_sse(chat, &fixture.model, 5000),
                            "{driver} chat={chat} balance={balance}"
                        );
                    } else {
                        assert_eq!(
                            serde_json::from_slice::<Value>(&delivered).unwrap(),
                            ordinary_response(chat, 5000)
                        );
                    }
                    wait_for_request_settlement(&fixture, 1).await;
                    let rows = fixture
                        .state
                        .db
                        .list_requests(fixture.key_id, 10)
                        .await
                        .unwrap();
                    assert_eq!(rows.len(), 1);
                    assert_eq!(rows[0].status_code, Some(200));
                    assert_eq!(rows[0].error_code, None);
                    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 5000));
                    assert_eq!(
                        rows[0].cost.parse::<Decimal>().unwrap(),
                        Decimal::new(balance.min(5003), 6)
                    );
                    assert_exactly_once_side_effects(
                        &fixture,
                        rows[0].request_id,
                        if chat {
                            None
                        } else {
                            Some("resp-usage-contract")
                        },
                    )
                    .await;
                    let row = sqlx::query("SELECT reserved_tokens, reserved_micros, actual_micros, price_snapshot_json FROM usage_reservations")
                        .fetch_one(&pool).await.unwrap();
                    assert!((4096..4800).contains(&row.get::<i64, _>("reserved_tokens")));
                    assert!(row.get::<i64, _>("reserved_micros") < balance);
                    assert_eq!(row.get::<i64, _>("actual_micros"), balance.min(5003));
                    let snapshot: Value =
                        serde_json::from_str(&row.get::<String, _>("price_snapshot_json")).unwrap();
                    assert_eq!(snapshot.get("output_token_limit"), Some(&Value::Null));
                    let account: (i64, i64) = sqlx::query_as("SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1")
                        .bind(fixture.credit_account_id.to_string()).fetch_one(&pool).await.unwrap();
                    assert_eq!(account, (balance - balance.min(5003), 0));
                    pool.close().await;
                    upstream.verify().await;
                }
            }
        }
    }
}

#[tokio::test]
async fn ordinary_openai_wire_omits_unsolicited_limits_and_settles_above_4096_once() {
    for driver in ["http-json", "new-api"] {
        for chat in [false, true] {
            let upstream = MockServer::start().await;
            let fixture = fixture("ordinary-output", &upstream, driver).await;
            let request = ordinary_request(&fixture, chat);
            let expected_wire = request.clone();
            Mock::given(method("POST"))
                .and(path(if chat {
                    "/v1/chat/completions"
                } else {
                    "/v1/responses"
                }))
                .and(move |request: &wiremock::Request| {
                    request.body_json::<Value>().unwrap() == expected_wire
                })
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(ordinary_response(chat, 5000)),
                )
                .expect(1)
                .mount(&upstream)
                .await;
            let response = send_ordinary(&fixture, chat, &request).await;
            assert_eq!(response.status(), StatusCode::OK);
            let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&delivered).unwrap(),
                ordinary_response(chat, 5000)
            );
            wait_for_request_settlement(&fixture, 1).await;
            let rows = fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].status_code, Some(200));
            assert_eq!(rows[0].error_code, None);
            assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 5000));
            assert_eq!(
                rows[0].cost.parse::<Decimal>().unwrap(),
                Decimal::new(5003, 6)
            );
            assert_exactly_once_side_effects(
                &fixture,
                rows[0].request_id,
                if chat {
                    None
                } else {
                    Some("resp-usage-contract")
                },
            )
            .await;
            let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
            let reserved: i64 =
                sqlx::query_scalar("SELECT reserved_tokens FROM usage_reservations")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert!(reserved >= 65_536);
            pool.close().await;
            upstream.verify().await;
        }
    }
}

#[tokio::test]
async fn ordinary_missing_metadata_cancellation_before_headers_preserves_unknown_hold() {
    for stream in [false, true] {
        let upstream = MockServer::start().await;
        let fixture = response_usage_fixture_with_uri_contract_and_driver(
            "missing-output-cancel",
            upstream.uri(),
            0,
            None,
            "http-json",
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
            .expect(1)
            .mount(&upstream)
            .await;
        let mut body = ordinary_request(&fixture, false);
        body["stream"] = json!(stream);
        let mut request = Box::pin(send_ordinary(&fixture, false, &body));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    _ = &mut request => panic!("cancel fixture must not return headers"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {
                        if upstream.received_requests().await.unwrap().len() == 1 { break; }
                    }
                }
            }
        })
        .await
        .unwrap();
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        let snapshot: String = sqlx::query_scalar(
            "SELECT price_snapshot_json FROM usage_reservations WHERE status = 'reserved'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&snapshot)
                .unwrap()
                .get("output_token_limit"),
            Some(&Value::Null)
        );
        let held_account: (i64, i64) = sqlx::query_as(
            "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
        )
        .bind(fixture.credit_account_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        drop(request);
        cancellation::assert_single_unknown_receipt(&fixture, 499, Some("client_cancelled")).await;
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status_code, Some(499));
        assert_eq!(rows[0].error_code.as_deref(), Some("client_cancelled"));
        assert!(rows[0].usage.tokens.is_none());
        assert_eq!(rows[0].billing.cost, None);
        let terminal: (i64, i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = q.reservation_id), (SELECT COUNT(*) FROM request_events e WHERE e.request_id = q.id AND e.event_kind = 'finished') FROM request_records q WHERE q.id = $1")
            .bind(rows[0].request_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(terminal, (0, 0));
        let account: (i64, i64) = sqlx::query_as(
            "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
        )
        .bind(fixture.credit_account_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(account, held_account);
        assert_eq!(account.0, 1_000_000);
        assert!(account.1 > 0);
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
        pool.close().await;
    }
}

#[tokio::test]
async fn ordinary_openai_preserves_each_explicit_wire_limit_and_financial_constraint() {
    for (chat, field) in [
        (true, "max_tokens"),
        (true, "max_completion_tokens"),
        (false, "max_output_tokens"),
    ] {
        for reported in [17, 18] {
            let upstream = MockServer::start().await;
            let fixture = fixture("ordinary-explicit-output", &upstream, "http-json").await;
            let mut request = ordinary_request(&fixture, chat);
            request[field] = json!(17);
            let expected_wire = request.clone();
            Mock::given(method("POST"))
                .and(move |request: &wiremock::Request| {
                    request.body_json::<Value>().unwrap() == expected_wire
                })
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(ordinary_response(chat, reported)),
                )
                .expect(1)
                .mount(&upstream)
                .await;
            let response = send_ordinary(&fixture, chat, &request).await;
            assert_eq!(
                response.status(),
                if reported == 17 {
                    StatusCode::OK
                } else {
                    StatusCode::BAD_GATEWAY
                }
            );
            let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                .await
                .unwrap();
            if reported == 17 {
                wait_for_request_settlement(&fixture, 1).await;
            } else {
                cancellation::assert_single_unknown_receipt(
                    &fixture,
                    502,
                    Some("upstream_invalid_usage"),
                )
                .await;
            }
            let rows = fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            if reported == 17 {
                assert_eq!(rows[0].output_tokens, 17);
            } else {
                assert!(rows[0].usage.tokens.is_none());
                assert_eq!(rows[0].billing.cost, None);
            }
            assert_eq!(
                rows[0].error_code.as_deref(),
                if reported == 17 {
                    None
                } else {
                    Some("upstream_invalid_usage")
                }
            );
            upstream.verify().await;
        }
    }
}

#[tokio::test]
async fn ordinary_openai_trusted_reservations_require_funding_before_any_post() {
    for chat in [false, true] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&upstream)
            .await;
        let fixture = fixture("ordinary-unfunded-output", &upstream, "http-json").await;
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        sqlx::query("UPDATE credit_accounts SET available_micros = 10000 WHERE id = $1")
            .bind(fixture.credit_account_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        let response = send_ordinary(&fixture, chat, &ordinary_request(&fixture, chat)).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("balance_exhausted"));
        let reservations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(reservations, 0);
        pool.close().await;
        upstream.verify().await;
    }
}

async fn fixture(label: &str, upstream: &MockServer, driver: &str) -> CodexRouteFixture {
    let fixture = response_usage_fixture_with_uri_contract_and_driver(
        label,
        upstream.uri(),
        256,
        None,
        driver,
    )
    .await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let config = json!({
        "base_url": upstream.uri(),
        "network_scope": "public",
        "input_token_overhead_ceiling": 256,
        "reservation_token_bounds": {fixture.upstream_model.clone(): 65_536}
    });
    sqlx::query("UPDATE upstream_accounts SET config_json = $1 WHERE id = $2")
        .bind(config.to_string())
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    fixture
}

async fn send(fixture: &CodexRouteFixture, limit: Option<i64>) -> Response {
    let mut request = json!({
        "model": fixture.model,
        "input": "hello",
        "tools": [{"type": "function", "name": "spawn_agent",
            "parameters": {"type": "object", "properties": {}}}],
        "stream": true
    });
    if let Some(limit) = limit {
        request["max_output_tokens"] = json!(limit);
    }
    router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::USER_AGENT, "codex_cli_rs/0.155.0")
                .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
                .body(Body::from(serde_json::to_vec(&request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn passthrough_reserves_trusted_bound_and_settles_actual_usage_above_4096() {
    for driver in ["http-json", "new-api"] {
        let upstream = MockServer::start().await;
        let completed = completed_response_with_usage(3, 5_000);
        let sse = format!(
            "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-usage-contract\"}}}}\n\n\
             data: {{\"type\":\"response.completed\",\"response\":{completed}}}\n\n\
             data: [DONE]\n\n"
        );
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(|request: &wiremock::Request| {
                let value: Value = serde_json::from_slice(&request.body).unwrap();
                value.get("max_output_tokens").is_none()
            })
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;
        let fixture = fixture(driver, &upstream, driver).await;
        let response = send(&fixture, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let _body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        wait_for_request_settlement(&fixture, 1).await;
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows[0].status_code, Some(200));
        assert_eq!(rows[0].error_code, None);
        assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 5_000));
        assert_eq!(
            rows[0].cost.parse::<Decimal>().unwrap(),
            Decimal::new(5003, 6)
        );
        assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-usage-contract"))
            .await;
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        let reserved: i64 = sqlx::query_scalar("SELECT reserved_tokens FROM usage_reservations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(reserved >= 65_536);
        pool.close().await;
        upstream.verify().await;
    }
}

#[tokio::test]
async fn passthrough_explicit_limit_remains_a_settlement_constraint() {
    let upstream = MockServer::start().await;
    let completed = completed_response_with_usage(3, 5_000);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(json!({"max_output_tokens": 4096})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(
                "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-usage-contract\"}}}}\n\n\
                 data: {{\"type\":\"response.completed\",\"response\":{completed}}}\n\n\
                 data: [DONE]\n\n"
            ),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = fixture("explicit-limit", &upstream, "http-json").await;
    let response = send(&fixture, Some(4096)).await;
    let _body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    cancellation::assert_single_unknown_receipt(&fixture, 502, Some("upstream_invalid_usage"))
        .await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_invalid_usage")
    );
    assert!(rows[0].usage.tokens.is_none());
    upstream.verify().await;
}

#[test]
fn passthrough_bounds_fail_closed_on_invalid_present_metadata() {
    let bound = super::super::routing::passthrough_output_reservation_bound;
    assert_eq!(bound(&json!({}), &json!({}), "model").unwrap(), 4096);
    for config in [
        json!({"reservation_token_bounds": {"model": -1}}),
        json!({"reservation_token_bounds": {"model": 1_000_000_001_i64}}),
        json!({"reservation_token_bounds": {"model": null}}),
        json!({"reservation_token_bounds": {"model": "65536"}}),
        json!({"reservation_token_bounds": null}),
        json!({"reservation_token_bounds": []}),
    ] {
        assert!(bound(&json!({}), &config, "model").is_err());
    }
    for limit in [json!(-1), json!(1.5), json!("65536"), Value::Null] {
        assert!(bound(&json!({"max_output_tokens": limit}), &json!({}), "model").is_err());
    }
}

#[test]
fn optional_passthrough_bounds_only_change_listed_models_and_keep_native_codex_strict() {
    let bound = super::super::routing::passthrough_output_reservation_bound;
    let config = json!({"reservation_token_bounds": {"listed-model": 65536}});
    assert_eq!(bound(&json!({}), &config, "listed-model").unwrap(), 65536);
    assert_eq!(bound(&json!({}), &config, "unlisted-model").unwrap(), 4096);
    assert_eq!(
        bound(
            &json!({}),
            &json!({"reservation_token_bounds": {}}),
            "unlisted-model"
        )
        .unwrap(),
        4096
    );
    for model in ["listed-model", "unlisted-model"] {
        assert_eq!(
            bound(&json!({"max_output_tokens": 123}), &config, model).unwrap(),
            123
        );
    }
    // This fallback is restricted to generic pass-through accounts. Native
    // Codex must still reject a route without synchronized model metadata.
    assert!(codex_transport::trusted_reservation_token_bound(&config, "unlisted-model").is_err());
    assert_eq!(
        codex_transport::trusted_reservation_token_bound(&config, "listed-model").unwrap(),
        65536
    );
}

#[tokio::test]
async fn passthrough_bound_must_be_funded_before_dispatch() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let fixture = fixture("unfunded-bound", &upstream, "http-json").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    // Enough for the historical 4096-token bound, not the trusted 65536.
    sqlx::query("UPDATE credit_accounts SET available_micros = 10000 WHERE id = $1")
        .bind(fixture.credit_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let response = send(&fixture, None).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("balance_exhausted"));
    let reservations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reservations, 0);
    pool.close().await;
    upstream.verify().await;
}
