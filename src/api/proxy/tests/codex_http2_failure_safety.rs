use super::*;
use futures_util::StreamExt as _;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy)]
enum Failure {
    PreHeaderReset,
    PreHeaderRefusedReset,
    ResetThenSuccess,
    ResetThenTransientThenReset,
    ResetThenRateLimitThenReset,
    PreHeaderSilence,
    PostHeaderSilence,
    RejectedReset,
    RejectedPlain,
    RejectedPriorReset,
    RejectedFinalReset,
}

// The fixture is cleartext HTTP/2. Observe frames only after their bytes
// were written and flushed; the subsequent PING cannot overtake those bytes.
struct HeaderObservedIo {
    inner: tokio::net::TcpStream,
    written: Vec<u8>,
    pending_headers: Vec<u32>,
    headers: tokio::sync::broadcast::Sender<u32>,
}

impl tokio::io::AsyncRead for HeaderObservedIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        tokio::io::AsyncRead::poll_read(std::pin::Pin::new(&mut self.inner), cx, buf)
    }
}

impl tokio::io::AsyncWrite for HeaderObservedIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let count =
            match tokio::io::AsyncWrite::poll_write(std::pin::Pin::new(&mut self.inner), cx, buf) {
                std::task::Poll::Ready(Ok(count)) => count,
                other => return other,
            };
        self.written.extend_from_slice(&buf[..count]);
        while self.written.len() >= 9 {
            let length = (usize::from(self.written[0]) << 16)
                | (usize::from(self.written[1]) << 8)
                | usize::from(self.written[2]);
            if self.written.len() < 9 + length {
                break;
            }
            let kind = self.written[3];
            let flags = self.written[4];
            let stream = u32::from_be_bytes(self.written[5..9].try_into().unwrap()) & 0x7fff_ffff;
            if kind == 1 {
                assert_ne!(flags & 4, 0, "fixture status HEADERS must fit one frame");
                self.pending_headers.push(stream);
            }
            self.written.drain(..9 + length);
        }
        std::task::Poll::Ready(Ok(count))
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match tokio::io::AsyncWrite::poll_flush(std::pin::Pin::new(&mut self.inner), cx) {
            std::task::Poll::Ready(Ok(())) => {
                for stream in std::mem::take(&mut self.pending_headers) {
                    let _ = self.headers.send(stream);
                }
                std::task::Poll::Ready(Ok(()))
            }
            other => other,
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        tokio::io::AsyncWrite::poll_shutdown(std::pin::Pin::new(&mut self.inner), cx)
    }
}

async fn http2_failure_upstream(
    failure: Failure,
) -> (
    String,
    Arc<AtomicUsize>,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::watch::Sender<bool>,
    tokio::sync::mpsc::UnboundedReceiver<u16>,
    tokio::sync::mpsc::UnboundedReceiver<u16>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let received = Arc::new(AtomicUsize::new(0));
    let (release, mut released) = tokio::sync::oneshot::channel();
    let count = received.clone();
    let (reset_gate, reset_allowed) = tokio::sync::watch::channel(false);
    let (headers_ready, observed_headers) = tokio::sync::mpsc::unbounded_channel();
    let (reset_sent, observed_resets) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = &mut released => break,
                accepted = listener.accept() => {
                    let (stream, _) = accepted.unwrap();
                    let count = count.clone();
                    let reset_allowed = reset_allowed.clone();
                    let headers_ready = headers_ready.clone();
                    let reset_sent = reset_sent.clone();
                    connections.spawn(async move {
                        let (headers_forwarded, _) = tokio::sync::broadcast::channel(8);
                        let io = HeaderObservedIo { inner: stream, written: Vec::new(), pending_headers: Vec::new(), headers: headers_forwarded.clone() };
                        let mut connection = http2::server::handshake(io).await.unwrap();
                        let ping_pong = Arc::new(tokio::sync::Mutex::new(connection.ping_pong().unwrap()));
                        let mut held_requests = Vec::new();
                        while let Some(accepted) = connection.accept().await {
                            let Ok((request, mut response)) = accepted else {
                                break;
                            };
                            assert_eq!(request.version(), http::Version::HTTP_2);
                            assert_eq!(request.method(), http::Method::POST);
                            assert_eq!(request.uri().path(), codex_transport::RESPONSES_PATH);
                            let attempt = count.fetch_add(1, Ordering::SeqCst);
                            let mut written_headers = headers_forwarded.subscribe();
                            if matches!(failure, Failure::ResetThenRateLimitThenReset) {
                                assert_eq!(
                                    request.headers().get("chatgpt-account-id").unwrap(),
                                    if attempt < 2 { "account-123" } else { "account-456" }
                                );
                            }
                            match failure {
                                Failure::PreHeaderReset => response.send_reset(http2::Reason::CANCEL),
                                Failure::PreHeaderRefusedReset => response.send_reset(http2::Reason::REFUSED_STREAM),
                                Failure::ResetThenSuccess if attempt == 0 => response.send_reset(http2::Reason::CANCEL),
                                Failure::ResetThenSuccess => {
                                    let headers = http::Response::builder()
                                        .status(200)
                                        .header(http::header::CONTENT_TYPE, "text/event-stream")
                                        .body(())
                                        .unwrap();
                                    let mut body = response.send_response(headers, false).unwrap();
                                    body.send_data(Bytes::from(completed_codex_sse("recovered")), true).unwrap();
                                }
                                Failure::ResetThenTransientThenReset if attempt == 1 => {
                                    let headers = http::Response::builder()
                                        .status(400)
                                        .header(http::header::CONTENT_TYPE, "application/json")
                                        .body(())
                                        .unwrap();
                                    let mut body = response.send_response(headers, false).unwrap();
                                    body.send_data(Bytes::from_static(br#"{"error":{"type":"request_error","message":"The service is under high demand."}}"#), true).unwrap();
                                }
                                Failure::ResetThenTransientThenReset => response.send_reset(http2::Reason::CANCEL),
                                Failure::ResetThenRateLimitThenReset if attempt == 1 => {
                                    let headers = http::Response::builder()
                                        .status(429)
                                        .header(http::header::CONTENT_TYPE, "application/json")
                                        .body(())
                                        .unwrap();
                                    let mut body = response.send_response(headers, false).unwrap();
                                    body.send_data(Bytes::from_static(br#"{"error":{"type":"rate_limit_error","message":"Rate limit reached"}}"#), true).unwrap();
                                }
                                Failure::ResetThenRateLimitThenReset => response.send_reset(http2::Reason::CANCEL),
                                Failure::PreHeaderSilence => response.send_reset(http2::Reason::CANCEL),
                                Failure::PostHeaderSilence => {
                                    let headers = http::Response::builder()
                                        .status(200)
                                        .header(http::header::CONTENT_TYPE, "text/event-stream")
                                        .body(())
                                        .unwrap();
                                    let mut body = response.send_response(headers, false).unwrap();
                                    // Let the connection driver flush the headers before
                                    // resetting the body, rather than cancelling queued headers.
                                    let mut reset_allowed = reset_allowed.clone();
                                    let ping_pong = ping_pong.clone();
                                    let headers_ready = headers_ready.clone();
                                    let reset_sent = reset_sent.clone();
                                    let stream_id = body.stream_id().as_u32();
                                    tokio::spawn(async move {
                                        while written_headers.recv().await.unwrap() != stream_id {}
                                        ping_pong.lock().await.ping(http2::Ping::opaque()).await.unwrap();
                                        headers_ready.send(200).unwrap();
                                        if !*reset_allowed.borrow() {
                                            reset_allowed.changed().await.unwrap();
                                        }
                                        body.send_reset(http2::Reason::CANCEL);
                                        reset_sent.send(200).unwrap();
                                    });
                                }
                                Failure::RejectedReset | Failure::RejectedPlain | Failure::RejectedPriorReset | Failure::RejectedFinalReset => {
                                    let headers = http::Response::builder()
                                        .status(if matches!(failure, Failure::RejectedPriorReset | Failure::RejectedFinalReset) && attempt == 0 { 429 } else { 502 })
                                        .header(http::header::CONTENT_TYPE, "application/json")
                                        .body(())
                                        .unwrap();
                                    let mut body = response.send_response(headers, false).unwrap();
                                    if matches!(failure, Failure::RejectedReset)
                                        || matches!(failure, Failure::RejectedPriorReset) && attempt == 0
                                        || matches!(failure, Failure::RejectedFinalReset) && attempt == 1
                                    {
                                        let mut reset_allowed = reset_allowed.clone();
                                        let ping_pong = ping_pong.clone();
                                        let headers_ready = headers_ready.clone();
                                        let reset_sent = reset_sent.clone();
                                        let stream_id = body.stream_id().as_u32();
                                        let status = if matches!(failure, Failure::RejectedPriorReset) && attempt == 0 { 429 } else { 502 };
                                        tokio::spawn(async move {
                                            while written_headers.recv().await.unwrap() != stream_id {}
                                            ping_pong.lock().await.ping(http2::Ping::opaque()).await.unwrap();
                                            headers_ready.send(status).unwrap();
                                            if !*reset_allowed.borrow() {
                                                reset_allowed.changed().await.unwrap();
                                            }
                                            body.send_reset(http2::Reason::CANCEL);
                                            reset_sent.send(status).unwrap();
                                        });
                                    } else {
                                        body.send_data(Bytes::from_static(b"{\"error\":\"upstream_http2_reset Bearer untrusted-diagnostic\"}"), true).unwrap();
                                    }
                                }
                            }
                            held_requests.push(request);
                        }
                    });
                }
            }
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    });
    (
        endpoint,
        received,
        release,
        reset_gate,
        observed_headers,
        observed_resets,
        server,
    )
}

async fn set_short_http2_deadline(fixture: &CodexRouteFixture) {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let mut config: Value = serde_json::from_str(
        &sqlx::query_scalar::<_, String>("SELECT config_json FROM upstream_accounts WHERE id = $1")
            .bind(fixture.upstream_account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    config["transport_policy"] = json!({
        "connect_attempts": 1,
        "candidate_attempts": 2,
        "connect_timeout_millis": 100,
        "read_timeout_millis": 1000,
        "request_timeout_millis": 3000
    });
    sqlx::query("UPDATE upstream_accounts SET config_json = $1 WHERE id = $2")
        .bind(config.to_string())
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

fn assert_exact_http2_reset_body_error(error: axum::Error) {
    // Axum retains the original stream code inside its typed error wrappers.
    // Inspect the leaf, never provider text or a substring in a debug dump.
    let mut cause: &(dyn std::error::Error + 'static) = &error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    assert_eq!(cause.to_string(), upstream_response::UPSTREAM_HTTP2_RESET);
}

async fn assert_terminal_failure(failure: Failure, label: &str) {
    let fixture = codex_route_fixture(label).await;
    set_short_http2_deadline(&fixture).await;
    let standby =
        add_codex_standby_route(&fixture, &format!("codex-route-{label}"), "account-456").await;
    fixture.state.codex_clients.install_test_client(
        reqwest::Client::builder()
            .http2_prior_knowledge()
            .retry(reqwest::retry::never())
            .no_proxy()
            .build()
            .unwrap(),
    );
    let (
        endpoint,
        received,
        release,
        reset_gate,
        mut observed_headers,
        mut observed_resets,
        server,
    ) = http2_failure_upstream(failure).await;
    let send = send_codex_route_to_endpoint(
        &fixture,
        endpoint,
        "/v1/responses",
        json!({"model": fixture.model, "input": "failure safety", "stream": true}),
    );
    tokio::pin!(send);
    let mut reset_observed = false;
    let response = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            tokio::select! {
                response = &mut send => break response,
                status = observed_headers.recv() => {
                    let status = status.expect("mock header gate must remain open");
                    let expected = match failure {
                        Failure::PostHeaderSilence => 200,
                        Failure::RejectedPriorReset => 429,
                        Failure::RejectedReset | Failure::RejectedFinalReset => 502,
                        _ => panic!("unexpected reset header gate"),
                    };
                    assert_eq!(status, expected);
                    reset_observed = true;
                    reset_gate.send(true).unwrap();
                }
            }
        }
    })
    .await
    .expect("a failed HTTP/2 POST must terminate");
    let expected_delivery_status = match failure {
        Failure::PostHeaderSilence => StatusCode::OK,
        _ => StatusCode::BAD_GATEWAY,
    };
    assert_eq!(response.status(), expected_delivery_status);
    let expected_reset_status = match failure {
        Failure::PostHeaderSilence => Some(200),
        Failure::RejectedPriorReset => Some(429),
        Failure::RejectedReset | Failure::RejectedFinalReset => Some(502),
        _ => None,
    };
    if expected_reset_status.is_some() && !reset_observed {
        let status = tokio::time::timeout(Duration::from_secs(2), observed_headers.recv())
            .await
            .unwrap()
            .expect("post-header reset gate");
        assert_eq!(Some(status), expected_reset_status);
        reset_gate.send(true).unwrap();
    }
    if let Some(status) = expected_reset_status {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), observed_resets.recv())
                .await
                .unwrap(),
            Some(status),
            "target reset must actually be injected"
        );
    }
    // The reset cases were released only after the client acknowledged a
    // PING queued behind the headers, never after a scheduler delay.
    let mut stream = response.into_body().into_data_stream();
    let mut body = Vec::new();
    let mut reset_body_error = false;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                assert!(body.len() + chunk.len() <= MAX_PROXY_RESPONSE_BODY);
                body.extend_from_slice(&chunk);
            }
            Err(error)
                if matches!(
                    failure,
                    Failure::RejectedReset | Failure::RejectedFinalReset
                ) =>
            {
                assert_exact_http2_reset_body_error(error);
                reset_body_error = true;
                break;
            }
            Err(error) => panic!("unexpected response body failure: {error:?}"),
        }
    }
    if matches!(
        failure,
        Failure::RejectedReset | Failure::RejectedFinalReset
    ) && !reset_body_error
    {
        let diagnostic: Value =
            serde_json::from_slice(&body).expect("completed gateway error body must be JSON");
        assert!(
            diagnostic.get("error").is_some(),
            "completed gateway error body must retain its error envelope"
        );
    }
    let body = String::from_utf8_lossy(&body);
    let has_provider_text = matches!(
        failure,
        Failure::RejectedPlain | Failure::RejectedPriorReset
    );
    assert_eq!(body.contains("untrusted-diagnostic"), has_provider_text);
    assert_eq!(
        body.contains("upstream_http2_reset Bearer"),
        has_provider_text
    );
    let receipt = cancellation::single_unknown_receipt(&fixture).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let expected_attempts = if matches!(
        failure,
        Failure::RejectedPriorReset | Failure::RejectedFinalReset
    ) {
        2
    } else {
        1
    };
    assert_eq!(
        received.load(Ordering::SeqCst),
        expected_attempts,
        "ambiguous H2 failure never creates another POST"
    );
    release.send(()).unwrap();
    server.await.unwrap();

    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(rows[0].billing.cost, None);
    let expected_error = match failure {
        Failure::PreHeaderReset
        | Failure::PreHeaderSilence
        | Failure::PreHeaderRefusedReset
        | Failure::ResetThenTransientThenReset
        | Failure::ResetThenRateLimitThenReset
        | Failure::ResetThenSuccess => "upstream_transport_http2_reset",
        Failure::PostHeaderSilence => "upstream_http2_reset",
        Failure::RejectedReset
        | Failure::RejectedPlain
        | Failure::RejectedPriorReset
        | Failure::RejectedFinalReset => "http_502",
    };
    let expected_cause = match failure {
        Failure::RejectedReset | Failure::RejectedFinalReset => Some("upstream_http2_reset"),
        Failure::RejectedPlain | Failure::RejectedPriorReset => Some("upstream_http_status"),
        _ => Some(expected_error),
    };
    assert_eq!(rows[0].error_code.as_deref(), Some(expected_error));
    assert_eq!(rows[0].terminal_cause_code.as_deref(), expected_cause);
    let detail = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    assert_eq!(detail.view.terminal_cause_code.as_deref(), expected_cause);
    // Recheck the shared operational-event/unknown-finance contract after
    // the transport has fully stopped, including absence of duplicate work.
    cancellation::assert_unknown_request(&fixture, rows[0].request_id, 0).await;
    assert_eq!(receipt["terminal_cause"].as_str(), expected_cause);
    let (expected_account, expected_route) = if matches!(
        failure,
        Failure::RejectedPriorReset | Failure::RejectedFinalReset
    ) {
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        let route_id: String = sqlx::query_scalar(
            "SELECT id FROM model_routes WHERE upstream_account_id = $1 AND public_model = $2",
        )
        .bind(standby.to_string())
        .bind(&fixture.model)
        .fetch_one(&pool)
        .await
        .unwrap();
        pool.close().await;
        (standby, Uuid::parse_str(&route_id).unwrap())
    } else {
        (fixture.upstream_account_id, fixture.route_id)
    };
    assert_eq!(rows[0].upstream_account_id, Some(expected_account));
    assert_eq!(rows[0].route_id, Some(expected_route));
}

#[tokio::test]
async fn pre_header_rst_stream_is_not_replayed_or_double_settled() {
    assert_terminal_failure(Failure::PreHeaderReset, "h2-pre-header-rst").await;
}

#[tokio::test]
async fn pre_header_refused_stream_without_delivery_proof_is_not_replayed() {
    assert_terminal_failure(Failure::PreHeaderRefusedReset, "h2-pre-header-refused").await;
}

#[tokio::test]
async fn pre_header_reset_recovery_allowance_is_not_refreshed_by_transient_400() {
    assert_terminal_failure(
        Failure::ResetThenTransientThenReset,
        "h2-recovery-shared-400",
    )
    .await;
}

#[tokio::test]
async fn pre_header_reset_recovery_allowance_is_not_refreshed_by_candidate_failover() {
    assert_terminal_failure(
        Failure::ResetThenRateLimitThenReset,
        "h2-recovery-shared-candidate",
    )
    .await;
}

#[tokio::test]
async fn ambiguous_reset_does_not_probe_a_second_successful_post() {
    assert_terminal_failure(Failure::ResetThenSuccess, "h2-reset-no-probe").await;
}

#[tokio::test]
async fn pre_header_silence_does_not_replay_or_double_settle() {
    assert_terminal_failure(Failure::PreHeaderSilence, "h2-pre-header-silence").await;
}

#[tokio::test]
async fn post_header_silence_does_not_replay_or_double_settle() {
    assert_terminal_failure(Failure::PostHeaderSilence, "h2-post-header-silence").await;
}

#[tokio::test]
async fn http_502_preserves_verified_final_response_reset_independently() {
    assert_terminal_failure(Failure::RejectedReset, "h2-502-final-reset").await;
}

#[tokio::test]
async fn http_502_does_not_infer_a_cause_from_untrusted_response_text() {
    assert_terminal_failure(Failure::RejectedPlain, "h2-502-unknown").await;
}

#[tokio::test]
async fn http_502_does_not_inherit_an_earlier_attempt_reset() {
    assert_terminal_failure(Failure::RejectedPriorReset, "h2-502-prior-reset").await;
}

#[tokio::test]
async fn http_502_preserves_only_the_final_attempt_reset_after_failover() {
    assert_terminal_failure(Failure::RejectedFinalReset, "h2-502-final-reset-after-429").await;
}
