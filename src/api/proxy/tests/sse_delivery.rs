use super::*;

mod incomplete_usage;

pub(super) async fn gated_sse_upstream(
    body: Vec<u8>,
) -> (
    String,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (release_body, body_released) = tokio::sync::oneshot::channel();
    let accepted = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&chunk[..read]);
            if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let declared_body_bytes = std::str::from_utf8(&request[..header_end])
            .unwrap()
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then_some(value.trim())
            })
            .unwrap()
            .parse::<usize>()
            .unwrap();
        while request.len() < header_end + declared_body_bytes {
            let mut chunk = [0_u8; 64 * 1024];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&chunk[..read]);
        }
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        stream.flush().await.unwrap();
        body_released.await.unwrap();
        stream
            .write_all(format!("{:X}\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        stream.write_all(&body).await.unwrap();
        stream.write_all(b"\r\n0\r\n\r\n").await.unwrap();
        stream.shutdown().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "a downstream cancellation must not replay the upstream request"
        );
    });
    (endpoint, release_body, accepted)
}

#[tokio::test]
async fn large_source_backed_conversation_stream_releases_raw_bytes_before_terminal_projection() {
    const RETAINED_BYTES: usize = 8 * 1024 * 1024;

    let fixture = codex_route_fixture("large-conversation-stream").await;
    fixture
        .state
        .db
        .update_key_policy(
            fixture.key_id,
            KeyPolicy {
                tokens_per_minute: 32 * 1024 * 1024,
                ..KeyPolicy::default()
            },
        )
        .await
        .unwrap();
    fixture
        .state
        .db
        .grant(
            fixture.credit_account_id,
            Decimal::from(32),
            "large conversation projection fixture",
            "large-conversation-projection-balance",
        )
        .await
        .unwrap();
    let (endpoint, release_body, upstream) =
        gated_sse_upstream(completed_codex_sse("projected after EOF").into_bytes()).await;
    let response = send_codex_route_to_endpoint(
        &fixture,
        endpoint,
        "/v1/responses",
        json!({
            "model": fixture.model,
            "input": "small semantic input",
            "metadata": {"padding": "x".repeat(RETAINED_BYTES)},
            "stream": true
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let request_id = Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap())
        .expect("gateway request id");
    let finalization_gate = streaming::finalization_test_gate::install(request_id);

    let (held, _, _, _) = fixture.state.proxy_memory_budget.snapshot();
    assert!(
        held < RETAINED_BYTES,
        "source-backed raw conversation bytes are released while the SSE remains open"
    );

    let delivered = tokio::spawn(async move {
        to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap()
    });
    release_body.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), finalization_gate.entered.notified())
        .await
        .expect("stream must reach the deterministic finalization gate");
    let (held, _, _, _) = fixture.state.proxy_memory_budget.snapshot();
    assert!(
        held < RETAINED_BYTES,
        "SSE framing capacity is returned before database finalization"
    );
    finalization_gate.release.notify_one();
    let delivered = delivered.await.unwrap();
    assert!(String::from_utf8_lossy(&delivered).contains("projected after EOF"));
    upstream.await.unwrap();
    wait_for_request_settlement(&fixture, 1).await;

    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let cluster: Option<String> =
        sqlx::query_scalar("SELECT conversation_cluster_id FROM request_records WHERE id = $1")
            .bind(rows[0].request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    pool.close().await;
    assert!(
        cluster.is_some(),
        "terminal projection reparsed the raw request"
    );
}

#[tokio::test]
async fn dropping_downstream_body_records_client_cancelled_without_poisoning_upstream() {
    let fixture = codex_route_fixture("downstream-client-cancelled").await;
    let (endpoint, release_body, upstream) =
        gated_sse_upstream(completed_codex_sse("never consumed").into_bytes()).await;
    let response = send_codex_route_to_endpoint(
        &fixture,
        endpoint,
        "/v1/responses",
        json!({"model": fixture.model, "input": "disconnect", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    drop(response);
    release_body.send(()).unwrap();
    upstream.await.unwrap();
    wait_for_request_settlement(&fixture, 1).await;

    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(499));
    assert_eq!(rows[0].error_code.as_deref(), Some("client_cancelled"));
    assert_eq!(rows[0].cost, "0");
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (0, 0));
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let health_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_account_health
         WHERE upstream_account_id = $1 AND consecutive_failures > 0",
    )
    .bind(fixture.upstream_account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(
        health_rows, 0,
        "a downstream cancellation is not upstream health evidence"
    );
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
}

async fn assert_delivery_database_fault(stage: &str, target_state: &str) {
    let fixture = codex_route_fixture(&format!("delivery-{stage}-fault")).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let trigger = match target_state {
        "delivery_prepared" => {
            "CREATE TRIGGER fail_delivery_prepare BEFORE UPDATE OF error_code ON request_records \
             WHEN NEW.error_code = 'delivery_prepared' \
             BEGIN SELECT RAISE(ABORT, 'DELIVERY_TRIGGER_PRIVATE_CANARY'); END"
        }
        "delivery_started" => {
            "CREATE TRIGGER fail_delivery_confirm BEFORE UPDATE OF error_code ON request_records \
             WHEN NEW.error_code = 'delivery_started' \
             BEGIN SELECT RAISE(ABORT, 'DELIVERY_TRIGGER_PRIVATE_CANARY'); END"
        }
        _ => panic!("unsupported delivery fault target"),
    };
    sqlx::query(trigger).execute(&pool).await.unwrap();
    pool.close().await;

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("delivery state fault"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "delivery fault", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .expect("delivery state failure must close with a safe SSE terminal");
    assert!(String::from_utf8_lossy(&delivered).contains("upstream request failed"));

    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(500));
    assert_eq!(rows[0].error_code.as_deref(), Some("delivery_state"));
    assert_eq!(rows[0].cost, "0");
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (0, 0));
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let health_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_account_health
         WHERE upstream_account_id = $1 AND consecutive_failures > 0",
    )
    .bind(fixture.upstream_account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(
        health_rows, 0,
        "a local delivery database fault is not upstream health evidence"
    );
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
    upstream.verify().await;
}

#[tokio::test]
async fn delivery_prepare_database_fault_is_local_and_unbilled() {
    assert_delivery_database_fault("prepare", "delivery_prepared").await;
}

#[tokio::test]
async fn delivery_confirm_database_fault_is_local_and_unbilled() {
    assert_delivery_database_fault("confirm", "delivery_started").await;
}

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
    assert_eq!(delivered.matches("event: response.failed").count(), 1);
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
async fn codex_clean_eof_without_terminal_is_durable_and_explicit_downstream() {
    let fixture = codex_route_fixture("clean-eof-without-terminal").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "event: response.created\n",
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-clean-eof\"}}\n\n"
            ),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "clean eof", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .expect("a clean EOF without a terminal must stay valid SSE");
    let delivered = String::from_utf8(delivered.to_vec()).unwrap();
    assert!(delivered.contains("response.created"));
    assert_eq!(delivered.matches("event: response.failed").count(), 1);
    assert!(!delivered.contains("response.completed"));

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
        Some("upstream_eof_without_terminal")
    );
    assert_eq!(
        rows[0].upstream_account_id,
        Some(fixture.upstream_account_id)
    );
    assert_eq!(rows[0].route_id, Some(fixture.route_id));
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
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
        "upstream_response_terminal_conflict",
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
    drain_completed_response_archive(&fixture).await;
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
