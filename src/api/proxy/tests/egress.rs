use super::*;

#[tokio::test]
async fn egress_records_every_real_send_and_counts_retry_payload_without_double_counting() {
    let fixture = codex_route_fixture("egress-retry-payload").await;
    let upstream = MockServer::start().await;
    let sent = std::sync::atomic::AtomicUsize::new(0);
    let rejection =
        json!({"error":{"type":"temporarily_unavailable","message":"retry"}}).to_string();
    let completed = completed_codex_sse("egress observed");
    let expected_response_bytes = rejection.len() + completed.len();
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(move |_: &wiremock::Request| {
            if sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                ResponseTemplate::new(400).set_body_raw(rejection.clone(), "application/json")
            } else {
                ResponseTemplate::new(200).set_body_raw(completed.clone(), "text/event-stream")
            }
        })
        .expect(2)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model":fixture.model,"input":"estimate all sends","stream":false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    wait_for_request_settlement(&fixture, 1).await;
    fixture.state.routing_persistence.drain_for_test().await;
    let requests = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    let attempts = fixture
        .state
        .db
        .request_egress_attempts(requests[0].request_id)
        .await
        .unwrap();
    assert_eq!(attempts.len(), 2);
    assert_ne!(attempts[0].attempt_id, attempts[1].attempt_id);
    assert_eq!(attempts[0].connect_attempt, attempts[1].connect_attempt);
    assert!(
        attempts.iter().all(|attempt| attempt.completed),
        "{attempts:?}"
    );
    assert!(
        attempts
            .iter()
            .all(|attempt| attempt.selected.kind == "direct"),
        "{attempts:?}"
    );
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt.final_response)
            .count(),
        1
    );
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(
        attempts
            .iter()
            .map(|attempt| attempt.request_payload_bytes.unwrap())
            .sum::<i64>(),
        received
            .iter()
            .map(|request| request.body.len() as i64)
            .sum::<i64>()
    );
    assert_eq!(
        attempts
            .iter()
            .map(|attempt| attempt.response_payload_bytes.unwrap())
            .sum::<i64>(),
        expected_response_bytes as i64
    );
    assert!(
        attempts
            .iter()
            .all(|attempt| attempt.measurement_basis == "application_payload_estimate")
    );
    let summaries = fixture
        .state
        .db
        .request_egress_summary_for_tenant(0, i64::MAX, Some("unrelated-tenant"))
        .await
        .unwrap();
    assert!(summaries.is_empty());
    let summaries = fixture
        .state
        .db
        .request_egress_summary_for_tenant(0, i64::MAX, Some("codex-route-egress-retry-payload"))
        .await
        .unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].recorded_attempts, 2);
    assert_eq!(summaries[0].final_response_attempts, 1);
    assert_eq!(summaries[0].wire_bytes, None);
    upstream.verify().await;
}

#[tokio::test]
async fn egress_sink_failure_and_saturation_do_not_change_successful_forwarding() {
    for saturation in [false, true] {
        let fixture = codex_route_fixture(if saturation {
            "egress-saturated"
        } else {
            "egress-storage-failed"
        })
        .await;
        let mut releases = Vec::new();
        if saturation {
            for _ in 0..4 {
                let (release, wait) = tokio::sync::oneshot::channel::<()>();
                releases.push(release);
                assert!(fixture.state.routing_persistence.submit(1, async move {
                    let _ = wait.await;
                    Ok(())
                }));
            }
        } else {
            let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
            sqlx::query("DROP TABLE request_egress_attempts")
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;
        }
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(completed_codex_sse("still delivered"), "text/event-stream"),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        let response = send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model":fixture.model,"input":"independent of sink","stream":false}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("still delivered"));
        drop(releases);
        fixture.state.routing_persistence.drain_for_test().await;
        upstream.verify().await;
    }
}
