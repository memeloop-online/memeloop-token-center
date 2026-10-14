use super::*;
use std::convert::Infallible;
use std::sync::{Arc, atomic::Ordering};

async fn body_metric(state: &AppState, token: &str, expected: usize) {
    let response = router_for_role(state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::get("/metrics")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains(&format!(
        "memeloop_token_center_background_work_items{{queue=\"gateway_body_reads\",state=\"active\"}} {expected}"
    )));
}

#[tokio::test]
async fn body_activity_counts_metered_and_gated_reads_and_releases_on_cancel() {
    let mut fixture = codex_route_fixture("body-activity-cancel").await;
    let service = fixture
        .state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "body-activity-metrics".to_owned(),
                scopes: vec!["metrics:read".to_owned()],
                tenant_external_id: None,
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    for (path, gated) in [
        ("/v1/chat/completions", false),
        ("/v1/alpha/search", false),
        ("/v1/embeddings", false),
        ("/v1/messages", false),
        ("/v1/messages/count_tokens", false),
        ("/v1/images/generations", true),
    ] {
        fixture.state.gateway_body_read_permits =
            Arc::new(tokio::sync::Semaphore::new(usize::from(gated)));
        let (started, started_wait) = tokio::sync::oneshot::channel();
        let (_release, released) = tokio::sync::oneshot::channel::<()>();
        let body = Body::from_stream(futures_util::stream::once(async move {
            let _ = started.send(());
            let _ = released.await;
            Ok::<_, Infallible>(Bytes::from_static(b"{}"))
        }));
        let mut request = Box::pin(
            router_for_role(fixture.state.clone(), RuntimeRole::Gateway).oneshot(
                Request::post(path)
                    .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body)
                    .unwrap(),
            ),
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut request => panic!("body must remain pending: {result:?}"),
                result = started_wait => result.unwrap(),
            }
        })
        .await
        .unwrap();
        assert_eq!(fixture.state.gateway_body_reads.load(Ordering::Acquire), 1);
        assert_eq!(
            fixture.state.gateway_body_read_permits.available_permits(),
            0
        );
        if !gated {
            assert!(fixture.state.proxy_memory_budget.snapshot().0 >= 12 * 1024 * 1024);
        }
        body_metric(&fixture.state, &service.token, 1).await;
        drop(request);
        assert_eq!(fixture.state.gateway_body_reads.load(Ordering::Acquire), 0);
        assert_eq!(fixture.state.proxy_memory_budget.snapshot().0, 0);
        assert_eq!(
            fixture.state.gateway_body_read_permits.available_permits(),
            usize::from(gated)
        );
        body_metric(&fixture.state, &service.token, 0).await;
        let response = router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
            .oneshot(
                Request::post(path)
                    .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_client_error(), "{}", response.status());
        drop(response);
        assert_eq!(fixture.state.gateway_body_reads.load(Ordering::Acquire), 0);
        assert_eq!(fixture.state.proxy_memory_budget.snapshot().0, 0);
    }
}
