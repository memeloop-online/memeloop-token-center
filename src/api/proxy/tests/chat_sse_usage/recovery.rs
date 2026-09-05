use super::support::*;
use super::*;

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
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("response.completed"));
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
    assert_eq!(rows[0].output_tokens, 16);
    assert_ne!(rows[0].cost, "0");
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
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
    assert_eq!(body, Bytes::from_static(b": ping\r\n\r\n: second\r\n\r\n"));
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
