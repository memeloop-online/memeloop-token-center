use super::*;

#[tokio::test]
async fn response_memory_rejection_is_counted_without_retry_or_status_change() {
    let upstream = MockServer::start().await;
    // Small encoded input, but a JSON tree larger than the reserved envelope.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [], "dense": vec![0; 2000]
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = resilient_route_fixture("response-memory-metrics", &[(upstream.uri(), 0)]).await;
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
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
async fn ingress_memory_counter_has_no_fabricated_route_label() {
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
    assert!(
        rendered.lines().any(|line| line
            == "memeloop_token_center_proxy_memory_rejections_total{stage=\"ingress\"} 1")
    );
    assert!(!rendered.lines().any(|line| {
        line.starts_with("memeloop_token_center_gateway_body_rejections_total{")
            && line.contains("capacity_exhausted")
            && !line.ends_with(" 0")
    }));
}
