use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy)]
enum Failure {
    PreHeaderReset,
    PreHeaderRefusedReset,
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
                        let mut held_headers = Vec::new();
                        let mut held_streams = Vec::new();
                        let mut held_requests = Vec::new();
                        while let Some(accepted) = connection.accept().await {
                            let Ok((request, mut response)) = accepted else {
                                break;
                            };
                            assert_eq!(request.version(), http::Version::HTTP_2);
                            assert_eq!(request.method(), http::Method::POST);
                            assert_eq!(request.uri().path(), codex_transport::RESPONSES_PATH);
                            let attempt = count.fetch_add(1, Ordering::SeqCst);
                            match failure {
                                Failure::PreHeaderReset => response.send_reset(http2::Reason::CANCEL),
                                Failure::PreHeaderRefusedReset => response.send_reset(http2::Reason::REFUSED_STREAM),
                                Failure::PreHeaderSilence => held_headers.push(response),
                                Failure::PostHeaderSilence => {
                                    let headers = http::Response::builder()
                                        .status(200)
                                        .header(http::header::CONTENT_TYPE, "text/event-stream")
                                        .body(())
                                        .unwrap();
                                    held_streams.push(response.send_response(headers, false).unwrap());
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
    wait_for_request_settlement(&fixture, 1).await;
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
        "only the initial 429 permits candidate failover; 502 and transport failures forbid replay"
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
    assert_eq!(rows[0].cost, "0");
    let expected_error = match failure {
        Failure::PreHeaderReset | Failure::PreHeaderRefusedReset => {
            "upstream_transport_http2_reset"
        }
        Failure::PreHeaderSilence => "upstream_request_timeout",
        Failure::PostHeaderSilence => "upstream_read_timeout",
        Failure::RejectedReset
        | Failure::RejectedPlain
        | Failure::RejectedPriorReset
        | Failure::RejectedFinalReset => "http_502",
    };
    let expected_cause = match failure {
        Failure::RejectedReset | Failure::RejectedFinalReset => Some("upstream_http2_reset"),
        Failure::RejectedPlain | Failure::RejectedPriorReset => None,
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
    let finished = events
        .iter()
        .find(|event| event.request_id == rows[0].request_id && event.event_kind == "finished")
        .unwrap();
    assert_eq!(finished.terminal_cause_code.as_deref(), expected_cause);
    let (expected_account, expected_route) = if expected_attempts == 2 {
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
    assert_exactly_once_side_effects_for(
        &fixture,
        rows[0].request_id,
        None,
        expected_account,
        expected_route,
    )
    .await;
}

#[tokio::test]
async fn pre_header_rst_stream_does_not_replay_or_double_settle() {
    assert_terminal_failure(Failure::PreHeaderReset, "h2-pre-header-rst").await;
}

#[tokio::test]
async fn pre_header_refused_stream_does_not_replay_or_double_settle() {
    assert_terminal_failure(Failure::PreHeaderRefusedReset, "h2-pre-header-refused").await;
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
