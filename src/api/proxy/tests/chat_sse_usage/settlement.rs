use super::support::*;
use super::*;

#[tokio::test]
async fn chat_usage_only_terminal_is_archived_and_settled_once() {
    let upstream = MockServer::start().await;
    let sse = [
        chat_content("chatcmpl-usage"),
        chat_finish("chatcmpl-usage"),
        chat_usage_only("chatcmpl-usage", usage(29, 7, 36)),
        done().to_owned(),
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({
            "stream": true,
            "stream_options": {"include_usage": true},
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("chat-terminal-usage", &upstream, 0).await;
    let request = chat_request(&fixture.model);
    let response = send_chat_usage_request(&fixture, &request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("\"choices\":[]"));
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (29, 7));
    assert_eq!(rows[0].cost, "0.000036");
    assert_eq!(rows[0].error_code, None);

    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let stored = sqlx::query(
        "SELECT q.cached_input_tokens, q.cache_write_tokens, r.actual_micros, r.price_snapshot_json, c.available_micros, t.cached_input_micros_per_million, t.cache_write_micros_per_million, t.cache_price_estimated FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id JOIN key_records k ON k.id = q.key_id JOIN credit_accounts c ON c.id = k.account_id JOIN model_price_tiers t ON t.model = q.model AND t.currency = 'USD' AND t.service_tier = 'default' WHERE q.id = $1",
    )
    .bind(rows[0].request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored.get::<i64, _>("cached_input_tokens"), 11);
    assert_eq!(stored.get::<i64, _>("cache_write_tokens"), 0);
    assert_eq!(stored.get::<i64, _>("actual_micros"), 36);
    assert_eq!(stored.get::<i64, _>("available_micros"), 999_964);
    assert_eq!(
        (
            stored.get::<i64, _>("cached_input_micros_per_million"),
            stored.get::<i64, _>("cache_write_micros_per_million"),
            stored.get::<i64, _>("cache_price_estimated"),
        ),
        (1_000_000, 1_000_000, 1),
        "default-tier cache pricing must use the explicit conservative fallback",
    );
    let snapshot: Value =
        serde_json::from_str(&stored.get::<String, _>("price_snapshot_json")).unwrap();
    assert_eq!(
        snapshot.pointer("/tiers/0/cached_input_micros_per_million"),
        Some(&json!(1_000_000)),
    );
    assert_eq!(
        snapshot.pointer("/tiers/0/cache_write_micros_per_million"),
        Some(&json!(1_000_000)),
    );
    pool.close().await;

    drain_completed_response_archive(&fixture).await;
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    let response_object = refs.response_object.expect("stream response archived");
    let archived = fixture.state.archive.get(&response_object).await.unwrap();
    assert!(String::from_utf8_lossy(&archived).contains("\"choices\":[]"));
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
}

#[tokio::test]
async fn chat_usage_reported_priority_tier_and_standard_nullable_details_settle_exactly() {
    let upstream = MockServer::start().await;
    let standard_usage = json!({
        "prompt_tokens": 29,
        "completion_tokens": 7,
        "total_tokens": 36,
        "prompt_tokens_details": {
            "cached_tokens": 11,
            "cache_write_tokens": 2,
            "audio_tokens": null,
            "image_tokens": null,
            "text_tokens": 18,
        },
        "completion_tokens_details": {
            "accepted_prediction_tokens": null,
            "audio_tokens": 0,
            "reasoning_tokens": null,
            "rejected_prediction_tokens": null,
            "text_tokens": 7,
        },
    });
    let sse = [
        chat_content("chatcmpl-priority"),
        chat_finish("chatcmpl-priority"),
        chat_usage_only_with_service_tier("chatcmpl-priority", standard_usage, "priority"),
        done().to_owned(),
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("chat-priority-tier", &upstream, 0).await;
    fixture
        .state
        .db
        .upsert_model_price_tier(
            &fixture.model,
            "USD",
            "priority",
            Decimal::from(2),
            Decimal::from(3),
            Decimal::from(3),
            Decimal::from(5),
            false,
        )
        .await
        .unwrap();
    let mut request = chat_request(&fixture.model);
    // `auto` may be served by a higher actual tier. Settlement must price the
    // tier reported by the terminal Chat chunk rather than discard it or price
    // this as the default tier.
    request["service_tier"] = json!("auto");
    let response = send_chat_usage_request(&fixture, &request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
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
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (29, 7));
    assert_eq!(rows[0].cost, "0.000106");
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let (actual_micros, service_tier, available_micros, cache_write_tokens): (i64, String, i64, i64) = sqlx::query_as(
        "SELECT r.actual_micros, q.service_tier, c.available_micros, q.cache_write_tokens FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id JOIN key_records k ON k.id = q.key_id JOIN credit_accounts c ON c.id = k.account_id WHERE q.id = $1",
    )
    .bind(rows[0].request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(actual_micros, 106);
    assert_eq!(service_tier, "priority");
    assert_eq!(available_micros, 999_894);
    assert_eq!(cache_write_tokens, 2);
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
}

#[tokio::test]
async fn strict_chat_success_with_a_non_sse_body_fails_closed_before_delivery() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-buffered",
            "object": "chat.completion",
            "choices": [{"message": {"content": "must-not-forward"}}],
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("chat-non-sse-success", &upstream, 0).await;
    let response = send_chat_usage_request(&fixture, &chat_request(&fixture.model)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("must-not-forward"));
    upstream.verify().await;
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_invalid_response")
    );
    assert_eq!(rows[0].cost, "0");
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
}

#[tokio::test]
async fn strict_chat_logprobs_and_named_failure_start_delivery_and_charge_once() {
    let upstream = MockServer::start().await;
    let sse = [
        chat_chunk(
            "chatcmpl-logprobs",
            json!([{
                "index": 0,
                "delta": {"role": "assistant", "content": null},
                "finish_reason": null,
                "logprobs": {
                    "content": [{
                        "token": "visible",
                        "logprob": -0.01,
                        "bytes": [118, 105, 115, 105, 98, 108, 101],
                        "top_logprobs": [],
                    }],
                },
            }]),
            None,
        ),
        concat!(
            "event: response.failed\n",
            "data: {\"error\":{\"message\":\"must-fail\"}}\n\n"
        )
        .to_owned(),
        done().to_owned(),
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("chat-logprobs-named-failure", &upstream, 0).await;
    let response = send_chat_usage_request(&fixture, &chat_request(&fixture.model)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("logprobs"));
    assert!(body.contains("response.failed"));
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_failed_response")
    );
    assert_eq!(rows[0].output_tokens, 16);
    assert_ne!(rows[0].cost, "0");
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
}

#[tokio::test]
async fn strict_chat_empty_named_event_fails_without_contract_charge() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("event: message\n\n", "text/event-stream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("chat-empty-named-event", &upstream, 0).await;
    let response = send_chat_usage_request(&fixture, &chat_request(&fixture.model)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert_eq!(
        body.as_ref(),
        [
            b"event: message\n\n".as_slice(),
            b"data: {\"error\":{\"type\":\"upstream_error\",\"message\":\"upstream stream did not complete\"}}\n\n".as_slice(),
        ].concat(),
    );
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_incomplete_response")
    );
    assert_eq!(rows[0].cost, "0");
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (0, 0));
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
}

#[tokio::test]
async fn strict_chat_empty_logprobs_preamble_fails_without_contract_charge() {
    let upstream = MockServer::start().await;
    let sse = chat_chunk(
        "chatcmpl-empty-logprobs",
        json!([{
            "index": 0,
            "delta": {"role": "assistant", "content": null},
            "finish_reason": null,
            "logprobs": {"content": []},
        }]),
        None,
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("chat-empty-logprobs", &upstream, 0).await;
    let response = send_chat_usage_request(&fixture, &chat_request(&fixture.model)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("\"content\":[]"));
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(rows[0].cost, "0");
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (0, 0));
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
}

#[tokio::test]
async fn strict_chat_usage_failures_charge_the_delivered_contract_once() {
    let mut cases = vec![
        (
            "missing-usage",
            [
                chat_content("chatcmpl-missing-usage"),
                chat_finish("chatcmpl-missing-usage"),
                done().to_owned(),
            ]
            .concat(),
        ),
        (
            "negative",
            [
                chat_content("chatcmpl-negative"),
                chat_finish("chatcmpl-negative"),
                chat_usage_only("chatcmpl-negative", usage(-1, 7, 6)),
                done().to_owned(),
            ]
            .concat(),
        ),
        (
            "overflow",
            [
                chat_content("chatcmpl-overflow"),
                chat_finish("chatcmpl-overflow"),
                chat_usage_only("chatcmpl-overflow", usage(1_000_000_001, 7, 1_000_000_008)),
                done().to_owned(),
            ]
            .concat(),
        ),
        (
            "inconsistent",
            [
                chat_content("chatcmpl-inconsistent"),
                chat_finish("chatcmpl-inconsistent"),
                chat_usage_only("chatcmpl-inconsistent", usage(29, 7, 37)),
                done().to_owned(),
            ]
            .concat(),
        ),
        (
            "usage-before-finish",
            [
                chat_content("chatcmpl-order"),
                chat_usage_only("chatcmpl-order", usage(29, 7, 36)),
                chat_finish("chatcmpl-order"),
                done().to_owned(),
            ]
            .concat(),
        ),
        (
            "missing-done",
            [
                chat_content("chatcmpl-missing-done"),
                chat_finish("chatcmpl-missing-done"),
                chat_usage_only("chatcmpl-missing-done", usage(29, 7, 36)),
            ]
            .concat(),
        ),
    ];

    for (label, sse) in cases.drain(..) {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;
        let fixture = response_usage_fixture(&format!("chat-invalid-{label}"), &upstream, 0).await;
        let response = send_chat_usage_request(&fixture, &chat_request(&fixture.model)).await;
        assert_eq!(response.status(), StatusCode::OK, "{label}");
        let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&body).contains("\"content\":\"ok\""),
            "{label} must exercise settlement after a delivered stream chunk",
        );
        wait_for_request_settlement(&fixture, 1).await;
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{label}");
        assert_eq!(rows[0].status_code, Some(502), "{label}");
        assert!(rows[0].error_code.is_some(), "{label}");
        assert_eq!(rows[0].output_tokens, 16, "{label}");
        assert!(rows[0].input_tokens > 0, "{label}");
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        let actual_micros: i64 = sqlx::query_scalar(
            "SELECT r.actual_micros FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.id = $1",
        )
        .bind(rows[0].request_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        pool.close().await;
        assert_eq!(
            actual_micros,
            rows[0].input_tokens + rows[0].output_tokens,
            "{label} must settle the priced contract ceiling exactly once",
        );
        assert!(actual_micros > 0, "{label}");
        assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
    }
}
