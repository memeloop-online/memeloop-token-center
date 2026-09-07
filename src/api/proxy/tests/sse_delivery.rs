use super::*;

async fn assert_codex_terminal_rejection(
    label: &str,
    sse: &str,
    expected_error: &str,
    expected_prefix: Option<&str>,
) {
    let fixture = codex_route_fixture(label).await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "reject terminal", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .expect("a rejected Responses stream must end with a safe SSE error");
    let delivered = String::from_utf8(delivered.to_vec()).unwrap();
    assert_eq!(delivered.matches("event: error").count(), 1);
    assert_eq!(delivered.matches("upstream request failed").count(), 1);
    assert!(!delivered.contains("response.completed"));
    assert!(!delivered.contains("[DONE]"));
    if let Some(prefix) = expected_prefix {
        assert!(delivered.contains(prefix));
    }

    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(rows[0].error_code.as_deref(), Some(expected_error));
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
    upstream.verify().await;
}

#[tokio::test]
async fn codex_terminal_id_conflict_never_archives_or_settles_as_success() {
    assert_codex_terminal_rejection(
        "terminal-id-conflict",
        concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-a\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-b\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n",
            "data: [DONE]\n\n"
        ),
        "upstream_invalid_response",
        None,
    )
    .await;
}

#[tokio::test]
async fn codex_completed_without_id_never_archives_or_settles_as_success() {
    assert_codex_terminal_rejection(
        "terminal-id-missing",
        concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-missing\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n",
            "data: [DONE]\n\n"
        ),
        "upstream_incomplete_response",
        None,
    )
    .await;
}

#[tokio::test]
async fn bare_completed_event_never_leaks_a_later_terminal() {
    assert_codex_terminal_rejection(
        "bare-terminal-event",
        concat!(
            "event: response.completed\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-bare\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n",
            "data: [DONE]\n\n"
        ),
        "upstream_invalid_response",
        None,
    )
    .await;
}

#[tokio::test]
async fn terminal_followed_by_unterminated_data_never_leaks_terminal_or_archives_success() {
    assert_codex_terminal_rejection(
        "terminal-trailing-partial",
        concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-trailing\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-trailing\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n",
            "data: [DONE]\n\n",
            "data: truncated"
        ),
        "upstream_incomplete_response",
        Some("response.created"),
    )
    .await;
}

#[tokio::test]
async fn codex_crlf_terminal_releases_at_eof_and_archives_only_safe_comments() {
    let fixture = codex_route_fixture("terminal-crlf-safe-comment").await;
    let upstream = MockServer::start().await;
    let secret = "Authorization: provider-comment-secret";
    let sse = concat!(
        "event: response.completed\r\n",
        ": Authorization: provider-comment-secret\r\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-crlf\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":2,\"total_tokens\":5}}}\r\n\r\n",
        "data: [DONE]\r\n\r\n"
    );
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "safe comment", "stream": true}),
    )
    .await;
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let delivered = String::from_utf8(body.to_vec()).unwrap();
    assert!(delivered.contains(": heartbeat\r\n"));
    assert!(delivered.contains("response.completed"));
    assert!(delivered.contains("data: [DONE]\r\n\r\n"));
    assert!(!delivered.contains(secret));

    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-crlf")).await;
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
    upstream.verify().await;
}

#[tokio::test]
async fn codex_failed_then_bare_secret_event_never_reaches_delivery_or_archive() {
    let fixture = codex_route_fixture("failed-bare-secret-event").await;
    let upstream = MockServer::start().await;
    let sse = concat!(
        "event: response.failed\n",
        "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp-failed\",\"error\":{\"message\":\"provider-payload-secret\"}}}\n\n",
        "event: Authorization-Bearer-bare-event-secret\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "redact failure", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let delivered = String::from_utf8(body.to_vec()).unwrap();
    assert!(delivered.contains("upstream request failed"));
    assert!(delivered.contains("data: [DONE]"));
    assert!(!delivered.contains("provider-payload-secret"));
    assert!(!delivered.contains("bare-event-secret"));

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
    let archived = String::from_utf8(archived.to_vec()).unwrap();
    assert!(archived.contains("upstream request failed"));
    assert!(!archived.contains("provider-payload-secret"));
    assert!(!archived.contains("bare-event-secret"));
    upstream.verify().await;
}
