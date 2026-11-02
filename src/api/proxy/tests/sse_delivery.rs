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
        let sent = async {
            stream
                .write_all(format!("{:X}\r\n", body.len()).as_bytes())
                .await?;
            stream.write_all(&body).await?;
            stream.write_all(b"\r\n0\r\n\r\n").await?;
            stream.shutdown().await
        }
        .await;
        if let Err(error) = sent {
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::NotConnected
                ),
                "unexpected provider socket failure: {error}"
            );
        }
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
    let retirement = fixture
        .state
        .pending_terminals
        .hold_retirement_for_test()
        .await;
    let spool_reads = fixture.state.responses_request_spool.read_count_for_test();
    fixture
        .state
        .db
        .update_key_policy(
            fixture.key_id,
            KeyPolicy {
                tokens_per_minute: Some(32 * 1024 * 1024),
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
    assert_eq!(
        fixture.state.responses_request_spool.read_count_for_test(),
        spool_reads + 1,
        "routing reads the source-backed request once"
    );

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

    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let settled: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pending_proxy_settlements WHERE request_id = $1 AND state = 'applied'",
            )
            .bind(request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            if settled == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("large source-backed request must financially settle");
    drain_completed_response_archive(&fixture).await;
    assert_exactly_once_side_effects(&fixture, request_id, Some("resp-codex")).await;
    assert_eq!(
        fixture.state.responses_request_spool.read_count_for_test(),
        spool_reads + 1,
        "terminal journal preparation must reuse the semantic projection without rereading raw input"
    );
    let mut found_receipt = false;
    for receipt in fixture.state.pending_terminals.receipt_jsons_for_test() {
        if receipt["request_id"] != request_id.to_string() {
            continue;
        }
        let bytes = serde_json::to_vec(&receipt).unwrap();
        assert!(
            bytes.len() < RETAINED_BYTES,
            "journal must not copy raw padding"
        );
        assert_eq!(
            receipt["conversation"]["request_json"],
            json!({"input": "small semantic input"})
        );
        found_receipt = true;
    }
    assert!(
        found_receipt,
        "settled receipt is retained at the retirement gate"
    );

    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    let cluster: Option<String> =
        sqlx::query_scalar("SELECT conversation_cluster_id FROM request_records WHERE id = $1")
            .bind(rows[0].request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        cluster.is_some(),
        "terminal projection must preserve source-backed conversation membership"
    );
    let stored_atoms: Vec<String> = sqlx::query_scalar(
        "SELECT content_json FROM semantic_atoms WHERE content_hash IN (SELECT atom_hash FROM context_nodes)",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        stored_atoms
            .iter()
            .all(|content| !content.contains(&"x".repeat(1024))),
        "会话正文不能收下原始请求 JSON 里的填充"
    );
    let return_document = crate::conversation::raw_return_document(
        completed_codex_sse("projected after EOF").as_bytes(),
    )
    .expect("原始返回 JSON");
    let session_body = crate::conversation::session_body_from_documents(
        &json!({"input": "small semantic input"}),
        &return_document,
    );
    let (_atoms, nodes) = session_body.into_prefix();
    let leaf: Option<String> = sqlx::query_scalar(
        "SELECT leaf_node_hash FROM conversation_observations WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        leaf.as_deref(),
        nodes.last().map(|node| node.node_hash.as_str())
    );
    pool.close().await;
    drop(retirement);
}

#[tokio::test]
async fn dropping_downstream_body_records_client_cancelled_without_poisoning_upstream() {
    let fixture = codex_route_fixture("downstream-client-cancelled").await;
    let active_owners = fixture.state.proxy_lifecycles.active();
    codex_output_limits::enable_provider_default_limits(&fixture).await;
    let (endpoint, release_body, upstream) =
        gated_sse_upstream(completed_codex_sse("never consumed").into_bytes()).await;
    let response = send_codex_route_to_endpoint(
        &fixture,
        endpoint,
        "/v1/responses",
        json!({"model": fixture.model, "input": "disconnect", "stream": true,
            "max_output_tokens": 16}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();

    drop(response);
    let receipt = cancellation::assert_unknown_receipt(
        &fixture,
        request_id,
        499,
        "client_cancelled",
        active_owners,
    )
    .await;
    assert_eq!(receipt["terminal_cause"], "client_disconnected");
    release_body.send(()).unwrap();
    upstream.await.unwrap();
    cancellation::assert_unknown_request(&fixture, request_id, active_owners).await;
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
        .expect("local delivery observations must not block valid output");
    assert!(String::from_utf8_lossy(&delivered).contains("response.completed"));
    assert!(String::from_utf8_lossy(&delivered).contains("delivery state fault"));
    assert!(!String::from_utf8_lossy(&delivered).contains("upstream request failed"));

    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    assert_eq!(rows[0].error_code, None);
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 2));
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
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex")).await;
    upstream.verify().await;
}

#[tokio::test]
async fn delivery_prepare_database_fault_does_not_block_output_or_erase_usage() {
    assert_delivery_database_fault("prepare", "delivery_prepared").await;
}

#[tokio::test]
async fn delivery_confirm_database_fault_does_not_block_output_or_erase_usage() {
    assert_delivery_database_fault("confirm", "delivery_started").await;
}

async fn assert_codex_terminal_rejection(
    label: &str,
    sse: &str,
    expected_error: &str,
    expected_prefix: Option<&str>,
) {
    let fixture = codex_route_fixture(label).await;
    let active_owners = fixture.state.proxy_lifecycles.active();
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
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
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

    let receipt = cancellation::assert_unknown_receipt(
        &fixture,
        request_id,
        502,
        expected_error,
        active_owners,
    )
    .await;
    assert_eq!(
        receipt["response_object"],
        format!("gap://{request_id}/response")
    );
    upstream.verify().await;
}

#[tokio::test]
async fn codex_clean_eof_without_terminal_is_durable_and_explicit_downstream() {
    let fixture = codex_route_fixture("clean-eof-without-terminal").await;
    let active_owners = fixture.state.proxy_lifecycles.active();
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
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .expect("a clean EOF without a terminal must stay valid SSE");
    let delivered = String::from_utf8(delivered.to_vec()).unwrap();
    assert!(delivered.contains("response.created"));
    assert_eq!(delivered.matches("event: response.failed").count(), 1);
    assert!(!delivered.contains("response.completed"));

    cancellation::assert_unknown_receipt(
        &fixture,
        request_id,
        502,
        "upstream_eof_without_terminal",
        active_owners,
    )
    .await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].request_id, request_id);
    assert_eq!(
        rows[0].upstream_account_id,
        Some(fixture.upstream_account_id)
    );
    assert_eq!(rows[0].route_id, Some(fixture.route_id));
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

pub(super) async fn pending_response_archive(
    fixture: &CodexRouteFixture,
    receipt: &Value,
) -> String {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    drain_archive_capture(fixture).await;
    let request_id = receipt["request_id"].as_str().unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let spool = sqlx::query(
        "SELECT tenant_id, reservation_id, state, chunk_count, byte_count \
         FROM response_archive_spools WHERE request_id = $1",
    )
    .bind(request_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        spool.get::<String, _>("tenant_id"),
        receipt["tenant_id"].as_str().unwrap()
    );
    assert_eq!(
        spool.get::<String, _>("reservation_id"),
        receipt["reservation"]["id"].as_str().unwrap()
    );
    assert_eq!(spool.get::<String, _>("state"), "pending");
    let chunks = sqlx::query(
        "SELECT seq, ciphertext, byte_count FROM response_archive_spool_chunks \
         WHERE request_id = $1 ORDER BY seq",
    )
    .bind(request_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(chunks.len() as i64, spool.get::<i64, _>("chunk_count"));
    assert!(!chunks.is_empty());
    let mut archived = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let sequence = chunk.get::<i64, _>("seq");
        assert_eq!(sequence, index as i64);
        let ciphertext = chunk.get::<String, _>("ciphertext");
        let (sealed, compressed, version) = match ciphertext.strip_prefix("zstd1.") {
            Some(sealed) => (sealed, true, 2),
            None => (ciphertext.as_str(), false, 1),
        };
        let aad = format!(
            "memeloop-token-center/response-archive-spool/v{version}/{}/{request_id}/{}/{sequence}",
            spool.get::<String, _>("tenant_id"),
            spool.get::<String, _>("reservation_id"),
        );
        let envelope: Value = crate::provider::open_envelope(
            sealed,
            fixture.state.config.key_pepper.as_bytes(),
            aad.as_bytes(),
        )
        .unwrap();
        let payload = URL_SAFE_NO_PAD
            .decode(envelope["bytes"].as_str().unwrap())
            .unwrap();
        let expected_bytes = usize::try_from(chunk.get::<i64, _>("byte_count")).unwrap();
        let payload = if compressed {
            zstd::bulk::decompress(&payload, expected_bytes).unwrap()
        } else {
            payload
        };
        assert_eq!(payload.len(), expected_bytes);
        archived.extend_from_slice(&payload);
    }
    assert_eq!(archived.len() as i64, spool.get::<i64, _>("byte_count"));
    pool.close().await;
    String::from_utf8(archived).unwrap()
}

pub(super) async fn assert_unknown_response_archives_omit(
    fixture: &CodexRouteFixture,
    sensitive: &str,
) {
    let receipt = cancellation::single_unknown_receipt(fixture).await;
    drain_archive_capture(fixture).await;
    let locator = receipt["response_object"].as_str().unwrap();
    if let Some(inline) = locator.strip_prefix("inline-json:") {
        assert!(!inline.contains(sensitive));
    } else {
        assert!(locator.starts_with("gap://"));
    }
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM response_archive_spools WHERE request_id = $1")
            .bind(receipt["request_id"].as_str().unwrap())
            .fetch_optional(&pool)
            .await
            .unwrap();
    if state.as_deref() == Some("pending") {
        let archived = pending_response_archive(fixture, &receipt).await;
        assert!(!archived.contains(sensitive));
    } else {
        assert!(state.is_none() || state.as_deref() == Some("gap"));
        let chunks: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM response_archive_spool_chunks WHERE request_id = $1",
        )
        .bind(receipt["request_id"].as_str().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(chunks, 0);
    }
    pool.close().await;
}

#[tokio::test]
async fn codex_failed_then_bare_secret_event_never_reaches_delivery_or_archive() {
    let fixture = codex_route_fixture("failed-bare-secret-event").await;
    let active_owners = fixture.state.proxy_lifecycles.active();
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
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let delivered = String::from_utf8(body.to_vec()).unwrap();
    assert!(delivered.contains("upstream request failed"));
    assert!(delivered.contains("data: [DONE]"));
    assert!(!delivered.contains("provider-payload-secret"));
    assert!(!delivered.contains("bare-event-secret"));

    let receipt = cancellation::assert_unknown_receipt(
        &fixture,
        request_id,
        502,
        "upstream_failed_response",
        active_owners,
    )
    .await;
    let archived = pending_response_archive(&fixture, &receipt).await;
    assert!(archived.contains("upstream request failed"));
    assert!(!archived.contains("provider-payload-secret"));
    assert!(!archived.contains("bare-event-secret"));
    upstream.verify().await;
}
