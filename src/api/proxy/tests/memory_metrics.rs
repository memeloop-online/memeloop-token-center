use super::*;

#[tokio::test]
async fn response_memory_rejection_is_counted_without_retry_or_status_change() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [], "dense": vec![0; 1_100_000]
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = resilient_route_fixture("response-memory-metrics", &[(upstream.uri(), 0)]).await;
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let rows = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let rows = fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap();
            if rows.len() == 1
                && rows[0]
                    .operational_terminal
                    .as_ref()
                    .is_some_and(|terminal| {
                        terminal.financial_state == crate::model::RequestFinancialState::Unknown
                    })
            {
                break rows;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("memory rejection must become operationally terminal within the original budget");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status_code, Some(502));
    assert!(rows[0].completed_at.is_none());
    assert!(rows[0].usage.tokens.is_none());
    assert!(rows[0].billing.cost.is_none());
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_response_memory_capacity")
    );
    let rendered = fixture
        .state
        .metrics
        .render(&crate::metrics::RuntimeMetrics::default());
    assert!(
        rendered.lines().any(|line| line
            == "memeloop_token_center_proxy_memory_rejections_total{stage=\"response\"} 1")
    );
    for stage in ["ingress", "json", "retained", "route", "plugin"] {
        assert!(rendered.lines().any(|line| line
            == format!(
                "memeloop_token_center_proxy_memory_rejections_total{{stage=\"{stage}\"}} 0"
            )));
    }
    upstream.verify().await;
}

#[tokio::test]
async fn dense_small_json_is_delivered_once_instead_of_failing_its_initial_byte_envelope() {
    let upstream = MockServer::start().await;
    let body = json!({
        "id": "dense-small", "object": "chat.completion",
        "choices": [{"index":0,"message":{"role":"assistant","content":"complete"},"finish_reason":"stop"}],
        "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2},
        "dense": vec![0; 2000]
    });
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = resilient_route_fixture("dense-small-response", &[(upstream.uri(), 0)]).await;
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let delivered: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(delivered, body);
    let rendered = fixture
        .state
        .metrics
        .render(&crate::metrics::RuntimeMetrics::default());
    assert!(
        rendered.lines().any(|line| line
            == "memeloop_token_center_proxy_memory_rejections_total{stage=\"response\"} 0")
    );
    upstream.verify().await;
}

#[tokio::test]
async fn source_json_memory_counter_has_no_fabricated_ingress_or_route_label() {
    let fixture = codex_route_fixture("ingress-memory-metrics").await;
    let held = fixture.state.proxy_memory_budget.reservation();
    assert!(held.try_grow(fixture.state.config.proxy_memory_budget_bytes as usize, 1));
    let request = Request::post("/v1/responses")
        .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(response.headers().contains_key(header::RETRY_AFTER));
    let rendered = fixture
        .state
        .metrics
        .render(&crate::metrics::RuntimeMetrics::default());
    assert!(rendered.lines().any(
        |line| line == "memeloop_token_center_proxy_memory_rejections_total{stage=\"json\"} 1"
    ));
    assert!(
        rendered.lines().any(|line| line
            == "memeloop_token_center_proxy_memory_rejections_total{stage=\"ingress\"} 0")
    );
    assert!(!rendered.lines().any(|line| {
        line.starts_with("memeloop_token_center_gateway_body_rejections_total{")
            && line.contains("capacity_exhausted")
            && !line.ends_with(" 0")
    }));
}
