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
async fn dense_small_response_is_delivered_once_and_client_body_retains_its_lease() {
    let upstream = MockServer::start().await;
    let original = serde_json::to_vec(&json!({
        "id":"dense-small", "object":"chat.completion",
        "choices":[{"index":0,"message":{"role":"assistant","content":"complete"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2},
        "dense":vec![0;300]
    })).unwrap();
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(original.clone())
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = resilient_route_fixture("dense-response-lease", &[(upstream.uri(), 0)]).await;
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    fixture.state.persistence.drain_for_test().await;
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.state.pending_terminals.drain(),
    )
    .await
    .unwrap();
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let refs = fixture
                .state
                .db
                .request_archive_refs(fixture.key_id, rows[0].request_id)
                .await
                .unwrap();
            if refs
                .response_object
                .as_deref()
                .is_some_and(|locator| !locator.starts_with("gap://"))
            {
                break;
            }
            crate::response_archive_spool::process_one_for_test(&fixture.state).await;
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("durable archive upload follows successful delivery independently");
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    let archived = fixture
        .state
        .archive
        .get(refs.response_object.as_deref().unwrap())
        .await
        .unwrap();
    assert_eq!(blake3::hash(&archived), blake3::hash(&original));
    assert_eq!(archived.as_ref(), original);
    assert!(fixture.state.proxy_memory_budget.snapshot().0 >= 64 * 1024);
    let delivered = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(delivered.as_ref(), original);
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
