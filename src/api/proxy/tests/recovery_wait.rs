use super::*;

#[tokio::test]
async fn sixty_five_production_recovery_waiters_reach_the_database_without_count_rejection() {
    use std::future::Future;

    let fixture = codex_route_fixture("sixty-five-recovery-waiters").await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let candidate = fixture
        .state
        .db
        .resolve_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            &fixture.model,
            Protocol::OpenAiResponses.name(),
            RouteSelectionOptions {
                upstream_account_hint: None,
                avoid_route_account: None,
                selection_seed: Uuid::now_v7(),
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap()
        .remove(0);
    let held = fixture.state.db.hold_group_snapshot_pool_for_tests().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut waiters = Vec::new();
    for _ in 0..65 {
        let mut waiting = Box::pin(routing::recovery_wait::wait(
            &fixture.state,
            Uuid::now_v7(),
            candidate.clone(),
            deadline,
        ));
        std::future::poll_fn(|context| {
            assert!(
                waiting.as_mut().poll(context).is_pending(),
                "all 65 unsent waiters must queue rather than hit a count rejection"
            );
            std::task::Poll::Ready(())
        })
        .await;
        waiters.push(waiting);
    }
    drop(held);
    for waiting in waiters {
        let (route, admission, mut guard) = tokio::time::timeout_at(deadline, waiting)
            .await
            .unwrap()
            .unwrap()
            .expect("every healthy candidate must pass the real database recheck");
        assert_eq!(route.account_id, fixture.upstream_account_id);
        assert_eq!(route.credential_generation, candidate.credential_generation);
        assert_eq!(route.transport_revision, candidate.transport_revision);
        assert!(admission.is_healthy());
        guard.abandon_without_observe().await;
    }
}

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
