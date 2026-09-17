use super::support::*;
use super::*;
use futures_util::StreamExt;

#[tokio::test]
async fn responses_done_then_trailing_partial_is_rejected_and_settled_once() {
    let completed = completed_response_with_usage(29, 7);
    let first_chunk = format!(
        concat!(
            "event: response.created\n",
            "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-trailing-partial\",\"error\":null}}}}\n\n",
            "event: response.completed\n",
            "data: {{\"type\":\"response.completed\",\"response\":{completed}}}\n\n",
            "data: [DONE]\n\n"
        ),
        completed = {
            let mut completed = completed;
            completed["id"] = json!("resp-trailing-partial");
            completed
        }
    );
    let (uri, upstream) = fragmented_sse_upstream(vec![
        first_chunk.into_bytes(),
        b"data: {\"type\":\"response.output_text.delta\"".to_vec(),
    ])
    .await;
    let fixture = response_usage_fixture_with_uri("responses-trailing-partial", uri, 0).await;
    let request = json!({
        "model": fixture.model,
        "input": "reject a partial Responses tail",
        "stream": true,
        "max_output_tokens": 16,
    });
    let response = send_response_usage_request(&fixture, &request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .expect("a truncated Responses stream must end with a safe SSE error");
    let delivered = String::from_utf8_lossy(&delivered);
    assert!(delivered.contains("response.created"));
    assert!(!delivered.contains("response.completed"));
    assert!(!delivered.contains("[DONE]"));
    assert_eq!(delivered.matches("event: response.failed").count(), 1);
    assert_eq!(delivered.matches("upstream request failed").count(), 1);
    upstream.await.unwrap();
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
    assert_eq!(rows[0].output_tokens, 0);
    assert_eq!(rows[0].cost, "0");
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    assert_eq!(
        refs.response_object.as_deref(),
        Some(format!("gap://{}/response", rows[0].request_id).as_str())
    );
}

#[tokio::test]
async fn chat_done_is_a_hard_same_chunk_terminal_for_delivery_archive_and_settlement() {
    let post_done = chat_chunk(
        "chatcmpl-done-hard-stop",
        json!([{
            "index": 0,
            "delta": {"content": "must-not-arrive"},
            "finish_reason": null,
        }]),
        None,
    );
    let (uri, upstream) = fragmented_sse_upstream(vec![
        [
            chat_content("chatcmpl-done-hard-stop"),
            chat_finish("chatcmpl-done-hard-stop"),
            chat_usage_only("chatcmpl-done-hard-stop", usage(29, 7, 36)),
            done().to_owned(),
            post_done,
        ]
        .concat()
        .into_bytes(),
    ])
    .await;
    let fixture = response_usage_fixture_with_uri("chat-done-hard-stop", uri, 0).await;
    let response = send_chat_usage_request(&fixture, &chat_request(&fixture.model)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("\"choices\":[]"));
    assert!(!body.contains("must-not-arrive"));
    upstream.await.unwrap();
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    assert_eq!(rows[0].cost, "0.000036");
    drain_completed_response_archive(&fixture).await;
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    let response_object = refs.response_object.expect("stream response archived");
    let archived = fixture.state.archive.get(&response_object).await.unwrap();
    assert!(!String::from_utf8_lossy(&archived).contains("must-not-arrive"));
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
}

#[tokio::test]
async fn moderation_only_chat_chunk_does_not_replace_terminal_usage() {
    let upstream = MockServer::start().await;
    let sse = [
        chat_moderation_only("chatcmpl-moderation"),
        chat_content("chatcmpl-moderation"),
        chat_finish("chatcmpl-moderation"),
        chat_usage_only("chatcmpl-moderation", usage(29, 7, 36)),
        done().to_owned(),
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("chat-moderation-only", &upstream, 0).await;
    let response = send_chat_usage_request(&fixture, &chat_request(&fixture.model)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("\"moderation\""));
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    assert_eq!(rows[0].cost, "0.000036");
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
}

#[tokio::test]
async fn fragmented_crlf_comment_frames_fail_without_starting_billable_delivery() {
    let (uri, upstream) = fragmented_sse_upstream(vec![
        b": pi".to_vec(),
        b"ng\r\n".to_vec(),
        b"\r\n: second\r\n\r\n".to_vec(),
    ])
    .await;
    let fixture = response_usage_fixture_with_uri("chat-comment-only", uri, 0).await;
    let response = send_chat_usage_request(&fixture, &chat_request(&fixture.model)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert_eq!(
        body.as_ref(),
        [
            b": heartbeat\r\n\r\n: heartbeat\r\n\r\n".as_slice(),
            b"data: {\"error\":{\"type\":\"upstream_error\",\"message\":\"upstream stream did not complete\"}}\n\n".as_slice(),
        ].concat(),
    );
    upstream.await.unwrap();
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
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let delivery_state: String =
        sqlx::query_scalar("SELECT error_code FROM request_records WHERE id = $1")
            .bind(rows[0].request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(delivery_state, "delivery_started");
    pool.close().await;
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
}

#[tokio::test]
async fn no_op_chat_preamble_and_terminal_disconnect_do_not_start_contract_billing() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                [
                    chat_role_only("chatcmpl-role-only"),
                    chat_terminal_noop("chatcmpl-role-only"),
                ]
                .concat(),
                "text/event-stream",
            ),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("chat-role-only-disconnect", &upstream, 0).await;
    let response = send_chat_usage_request(&fixture, &chat_request(&fixture.model)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("\"role\":\"assistant\""));
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
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let delivery_state: String =
        sqlx::query_scalar("SELECT error_code FROM request_records WHERE id = $1")
            .bind(rows[0].request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    pool.close().await;
    assert_ne!(delivery_state, "delivery_started");
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
}

#[tokio::test]
async fn strict_chat_consumes_the_lf_of_a_done_crlf_split_across_network_chunks() {
    let first = [
        chat_content("chatcmpl-split-done-crlf"),
        chat_finish("chatcmpl-split-done-crlf"),
        chat_usage_only("chatcmpl-split-done-crlf", usage(29, 7, 36)),
        "data: [DONE]\r\n\r".to_owned(),
    ]
    .concat();
    let (uri, upstream) = fragmented_sse_upstream(vec![first.into_bytes(), b"\n".to_vec()]).await;
    let fixture = response_usage_fixture_with_uri("chat-split-done-crlf", uri, 0).await;
    let response = send_chat_usage_request(&fixture, &chat_request(&fixture.model)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(body.ends_with(b"data: [DONE]\r\n\r\n"));
    upstream.await.unwrap();

    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    drain_completed_response_archive(&fixture).await;
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    let archived = fixture
        .state
        .archive
        .get(refs.response_object.as_deref().expect("response archive"))
        .await
        .unwrap();
    assert!(archived.ends_with(b"data: [DONE]\r\n\r\n"));
}

#[tokio::test]
async fn oversized_chat_event_errors_downstream_and_cannot_recover_into_done() {
    let mut oversized = b"data: ".to_vec();
    oversized.extend(vec![b'x'; MAX_RESPONSES_SSE_EVENT_BYTES + 1]);
    let recovered_terminal = [
        "\n\n".to_owned(),
        chat_content("chatcmpl-after-oversize"),
        chat_finish("chatcmpl-after-oversize"),
        chat_usage_only("chatcmpl-after-oversize", usage(29, 7, 36)),
        done().to_owned(),
    ]
    .concat();
    let (uri, upstream) =
        fragmented_sse_upstream(vec![oversized, recovered_terminal.into_bytes()]).await;
    let fixture = response_usage_fixture_with_uri("chat-oversized-event", uri, 0).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let credential_generation: i64 =
        sqlx::query_scalar("SELECT credential_generation FROM upstream_accounts WHERE id = $1")
            .bind(fixture.upstream_account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    fixture
        .state
        .db
        .record_upstream_account_failure(
            fixture.upstream_account_id,
            credential_generation,
            UpstreamFailureKind::Connection,
        )
        .await
        .unwrap();
    sqlx::query(
        "UPDATE upstream_account_health SET cooldown_until = 0
         WHERE upstream_account_id = $1 AND credential_generation = $2",
    )
    .bind(fixture.upstream_account_id.to_string())
    .bind(credential_generation)
    .execute(&pool)
    .await
    .unwrap();
    let health_before = sqlx::query(
        "SELECT consecutive_failures, cooldown_until, last_failure_kind
         FROM upstream_account_health
         WHERE upstream_account_id = $1 AND credential_generation = $2",
    )
    .bind(fixture.upstream_account_id.to_string())
    .bind(credential_generation)
    .fetch_one(&pool)
    .await
    .unwrap();
    let response = send_chat_usage_request(&fixture, &chat_request(&fixture.model)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let mut saw_error = false;
    while let Some(next) = body.next().await {
        if next.is_err() {
            saw_error = true;
        }
    }
    assert!(saw_error);
    upstream.await.unwrap();

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
        Some("upstream_response_event_too_large")
    );
    assert_eq!(rows[0].cost, "0");
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    assert_eq!(
        refs.response_object.as_deref(),
        Some(format!("gap://{}/response", rows[0].request_id).as_str())
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let probe_lease_until: i64 = sqlx::query_scalar(
                "SELECT probe_lease_until FROM upstream_account_health
                 WHERE upstream_account_id = $1 AND credential_generation = $2",
            )
            .bind(fixture.upstream_account_id.to_string())
            .bind(credential_generation)
            .fetch_one(&pool)
            .await
            .unwrap();
            if probe_lease_until == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("record_terminal must release the probe lease");
    let health_after = sqlx::query(
        "SELECT consecutive_failures, cooldown_until, last_failure_kind
         FROM upstream_account_health
         WHERE upstream_account_id = $1 AND credential_generation = $2",
    )
    .bind(fixture.upstream_account_id.to_string())
    .bind(credential_generation)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        health_after.get::<i64, _>("consecutive_failures"),
        health_before.get::<i64, _>("consecutive_failures"),
        "a local SSE event limit must not add an upstream failure"
    );
    assert_eq!(
        health_after.get::<i64, _>("cooldown_until"),
        health_before.get::<i64, _>("cooldown_until"),
        "a local SSE event limit must not install account cooldown"
    );
    assert_eq!(
        health_after.get::<String, _>("last_failure_kind"),
        health_before.get::<String, _>("last_failure_kind"),
        "a local SSE event limit must not overwrite prior upstream evidence"
    );
    pool.close().await;
}

#[tokio::test]
async fn total_chat_response_limit_does_not_poison_upstream_health() {
    let first = b": keepalive\n\n".to_vec();
    let second = chat_chunk(
        "chatcmpl-total-limit",
        json!([{
            "index": 0,
            "delta": {"content": "x".repeat(768)},
            "finish_reason": null,
        }]),
        None,
    )
    .into_bytes();
    let (uri, upstream) = fragmented_sse_upstream(vec![first, second]).await;
    let fixture = response_usage_fixture_with_uri("chat-total-limit", uri, 0).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let credential_generation: i64 =
        sqlx::query_scalar("SELECT credential_generation FROM upstream_accounts WHERE id = $1")
            .bind(fixture.upstream_account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    fixture
        .state
        .db
        .record_upstream_account_failure(
            fixture.upstream_account_id,
            credential_generation,
            UpstreamFailureKind::Connection,
        )
        .await
        .unwrap();
    sqlx::query(
        "UPDATE upstream_account_health SET cooldown_until = 0
         WHERE upstream_account_id = $1 AND credential_generation = $2",
    )
    .bind(fixture.upstream_account_id.to_string())
    .bind(credential_generation)
    .execute(&pool)
    .await
    .unwrap();
    let health_before = sqlx::query(
        "SELECT consecutive_failures, cooldown_until, last_failure_kind
         FROM upstream_account_health
         WHERE upstream_account_id = $1 AND credential_generation = $2",
    )
    .bind(fixture.upstream_account_id.to_string())
    .bind(credential_generation)
    .fetch_one(&pool)
    .await
    .unwrap();

    let response = streaming::with_test_response_body_limit(
        512,
        send_chat_usage_request(&fixture, &chat_request(&fixture.model)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let mut saw_error = false;
    while let Some(next) = body.next().await {
        if next.is_err() {
            saw_error = true;
        }
    }
    assert!(saw_error);
    upstream.await.unwrap();

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
        Some("upstream_response_too_large")
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let probe_lease_until: i64 = sqlx::query_scalar(
                "SELECT probe_lease_until FROM upstream_account_health
                 WHERE upstream_account_id = $1 AND credential_generation = $2",
            )
            .bind(fixture.upstream_account_id.to_string())
            .bind(credential_generation)
            .fetch_one(&pool)
            .await
            .unwrap();
            if probe_lease_until == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("record_terminal must release the probe lease");
    let health_after = sqlx::query(
        "SELECT consecutive_failures, cooldown_until, last_failure_kind
         FROM upstream_account_health
         WHERE upstream_account_id = $1 AND credential_generation = $2",
    )
    .bind(fixture.upstream_account_id.to_string())
    .bind(credential_generation)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        health_after.get::<i64, _>("consecutive_failures"),
        health_before.get::<i64, _>("consecutive_failures")
    );
    assert_eq!(
        health_after.get::<i64, _>("cooldown_until"),
        health_before.get::<i64, _>("cooldown_until")
    );
    assert_eq!(
        health_after.get::<String, _>("last_failure_kind"),
        health_before.get::<String, _>("last_failure_kind")
    );
    pool.close().await;
}

#[tokio::test]
async fn strict_chat_strips_secret_event_metadata_without_changing_settlement() {
    let upstream = MockServer::start().await;
    let secret = "Authorization-Bearer-event-secret";
    let event_prefix = format!("event: {secret}\ndata: ");
    let content = chat_content("chatcmpl-secret-event").replacen("data: ", &event_prefix, 1);
    let sse = [
        content,
        chat_finish("chatcmpl-secret-event"),
        chat_usage_only("chatcmpl-secret-event", usage(29, 7, 36)),
        done().to_owned(),
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("chat-secret-event", &upstream, 0).await;
    let response = send_chat_usage_request(&fixture, &chat_request(&fixture.model)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let delivered = String::from_utf8(body.to_vec()).unwrap();
    assert!(delivered.contains("\"content\":\"ok\""));
    assert!(!delivered.contains(secret));

    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    assert_eq!(rows[0].cost, "0.000036");
    drain_completed_response_archive(&fixture).await;
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    let archived = fixture
        .state
        .archive
        .get(refs.response_object.as_deref().expect("response archive"))
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&archived).contains(secret));
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
    upstream.verify().await;
}
