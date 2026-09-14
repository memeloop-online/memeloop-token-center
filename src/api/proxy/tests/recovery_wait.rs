use super::*;

#[tokio::test]
async fn unsent_request_waits_for_database_recovery_but_never_bypasses_new_quota_isolation() {
    for isolate in [false, true] {
        let fixture = codex_route_fixture(if isolate {
            "wait-isolation"
        } else {
            "wait-recovery"
        })
        .await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(codex_transport::RESPONSES_PATH))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(completed_codex_sse("recovered"), "text/event-stream"),
            )
            .expect(if isolate { 0 } else { 1 })
            .mount(&upstream)
            .await;
        fixture
            .state
            .db
            .record_upstream_account_failure(
                fixture.upstream_account_id,
                1,
                UpstreamFailureKind::Unavailable,
            )
            .await
            .unwrap();
        let checkpoint = routing::recovery_wait::test_checkpoint(fixture.upstream_account_id);
        let response = send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "wait", "stream": false}),
        );
        tokio::pin!(response);
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                _ = checkpoint.notified() => {},
                _ = &mut response => panic!("request finished before the recovery barrier"),
            }
        })
        .await
        .unwrap();
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "waiting cannot dispatch"
        );
        // A committed DB transition, not a sleep margin, releases the waiter.
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        sqlx::query("UPDATE upstream_account_health SET cooldown_until = 0, last_failure_kind = $1 WHERE upstream_account_id = $2")
            .bind(if isolate { "quota_exhausted" } else { "unavailable" })
            .bind(fixture.upstream_account_id.to_string())
            .execute(&pool).await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), &mut response)
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if isolate {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::OK
            }
        );
        let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        upstream.verify().await;
        pool.close().await;
    }
}
