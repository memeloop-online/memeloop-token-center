use super::*;
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

async fn http2_failure_upstream(
    failure: Failure,
) -> (
    String,
    Arc<AtomicUsize>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let received = Arc::new(AtomicUsize::new(0));
    let (release, mut released) = tokio::sync::oneshot::channel();
    let count = received.clone();
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = &mut released => break,
                accepted = listener.accept() => {
                    let (stream, _) = accepted.unwrap();
                    let count = count.clone();
                    connections.spawn(async move {
                        let mut connection = http2::server::handshake(stream).await.unwrap();
                        let mut held_requests = Vec::new();
                        while let Some(accepted) = connection.accept().await {
                            let Ok((request, mut response)) = accepted else {
                                break;
                            };
                            assert_eq!(request.version(), http::Version::HTTP_2);
                            assert_eq!(request.method(), http::Method::POST);
                            assert_eq!(request.uri().path(), codex_transport::RESPONSES_PATH);
                            let attempt = count.fetch_add(1, Ordering::SeqCst);
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
                                    body.send_reset(http2::Reason::CANCEL);
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
                                        tokio::spawn(async move {
                                            tokio::time::sleep(Duration::from_millis(20)).await;
                                            body.send_reset(http2::Reason::CANCEL);
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
    (endpoint, received, release, server)
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
        "request_timeout_millis": 3000,
        "failover_deadline_millis": 5000
    });
    sqlx::query("UPDATE upstream_accounts SET config_json = $1 WHERE id = $2")
        .bind(config.to_string())
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

async fn assert_terminal_failure(failure: Failure, label: &str) {
    let fixture = codex_route_fixture(label).await;
    set_short_http2_deadline(&fixture).await;
    let standby =
        add_codex_standby_route(&fixture, &format!("codex-route-{label}"), "account-456").await;
    fixture.state.codex_clients.install_test_client(
        wreq::Client::builder()
            .http2_only()
            .retry(wreq::retry::Policy::never())
            .no_proxy()
            .build()
            .unwrap(),
    );
    let (endpoint, received, release, server) = http2_failure_upstream(failure).await;
    let response = tokio::time::timeout(
        Duration::from_secs(8),
        send_codex_route_to_endpoint(
            &fixture,
            endpoint,
            "/v1/responses",
            json!({"model": fixture.model, "input": "failure safety", "stream": true}),
        ),
    )
    .await
    .expect("a failed HTTP/2 POST must terminate");
    let expected_delivery_status = match failure {
        Failure::PostHeaderSilence => StatusCode::OK,
        _ => StatusCode::BAD_GATEWAY,
    };
    assert_eq!(response.status(), expected_delivery_status);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(!body.contains("untrusted-diagnostic"));
    assert!(!body.contains("upstream_http2_reset"));
    let receipt = cancellation::single_unknown_receipt(&fixture).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let expected_attempts = if matches!(
        failure,
        Failure::ResetThenTransientThenReset | Failure::ResetThenRateLimitThenReset
    ) {
        3
    } else if matches!(
        failure,
        Failure::RejectedPriorReset
            | Failure::RejectedFinalReset
            | Failure::PreHeaderReset
            | Failure::PreHeaderSilence
            | Failure::PreHeaderRefusedReset
    ) {
        2
    } else {
        1
    };
    assert_eq!(
        received.load(Ordering::SeqCst),
        expected_attempts,
        "H2 recovery has one logical-request allowance across 400 retry and candidate failover"
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
        | Failure::ResetThenRateLimitThenReset => "upstream_transport_http2_reset",
        Failure::PostHeaderSilence => "upstream_http2_reset",
        Failure::RejectedReset
        | Failure::RejectedPlain
        | Failure::RejectedPriorReset
        | Failure::RejectedFinalReset => "http_502",
        Failure::ResetThenSuccess => unreachable!(),
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
    let events = fixture
        .state
        .db
        .all_request_events_after(0, None, 500)
        .await
        .unwrap();
    assert!(
        !events
            .iter()
            .any(|event| event.request_id == rows[0].request_id && event.event_kind == "finished")
    );
    assert_eq!(receipt["terminal_cause"].as_str(), expected_cause);
    let (expected_account, expected_route) = if matches!(
        failure,
        Failure::RejectedPriorReset
            | Failure::RejectedFinalReset
            | Failure::ResetThenRateLimitThenReset
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
async fn pre_header_rst_stream_recovers_once_without_double_settlement() {
    assert_terminal_failure(Failure::PreHeaderReset, "h2-pre-header-rst").await;
}

#[tokio::test]
async fn pre_header_refused_stream_recovers_once_without_double_settlement() {
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
async fn pre_header_reset_then_success_settles_one_logical_request() {
    for stream in [false, true] {
        let fixture = codex_route_fixture(if stream {
            "h2-recover-stream"
        } else {
            "h2-recover-buffered"
        })
        .await;
        set_short_http2_deadline(&fixture).await;
        fixture.state.codex_clients.install_test_client(
            wreq::Client::builder()
                .http2_only()
                .retry(wreq::retry::Policy::never())
                .no_proxy()
                .build()
                .unwrap(),
        );
        let (endpoint, received, release, server) =
            http2_failure_upstream(Failure::ResetThenSuccess).await;
        let response = send_codex_route_to_endpoint(
            &fixture,
            endpoint,
            "/v1/responses",
            json!({"model": fixture.model, "input": "recover", "stream": stream}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("recovered"));
        wait_for_request_settlement(&fixture, 1).await;
        assert_eq!(received.load(Ordering::SeqCst), 2);
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status_code, Some(200));
        fixture.state.routing_persistence.drain_for_test().await;
        let attempts = fixture
            .state
            .db
            .request_egress_attempts(rows[0].request_id)
            .await
            .unwrap();
        assert_eq!(attempts.len(), 2);
        assert_ne!(attempts[0].attempt_id, attempts[1].attempt_id);
        assert!(attempts.iter().all(|attempt| attempt.completed));
        assert!(attempts.iter().all(|attempt| {
            attempt.upstream_account_id == fixture.upstream_account_id
                && attempt.route_id == fixture.route_id
                && attempt.selected.kind == "direct"
                && attempt.request_payload_bytes.is_some()
                && attempt.measurement_basis == "application_payload_estimate"
        }));
        let final_attempts: Vec<_> = attempts
            .iter()
            .filter(|attempt| attempt.final_response)
            .collect();
        assert_eq!(final_attempts.len(), 1);
        assert_eq!(
            final_attempts[0].response_payload_bytes,
            Some(completed_codex_sse("recovered").len() as i64)
        );
        assert!(attempts.iter().any(|attempt| {
            !attempt.final_response && attempt.response_payload_bytes.is_none()
        }));
        assert_exactly_once_side_effects_for(
            &fixture,
            rows[0].request_id,
            Some("resp-codex"),
            fixture.upstream_account_id,
            fixture.route_id,
        )
        .await;
        release.send(()).unwrap();
        server.await.unwrap();
    }
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
