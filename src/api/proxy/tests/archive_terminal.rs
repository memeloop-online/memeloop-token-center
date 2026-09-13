use super::*;

#[tokio::test]
async fn request_archive_failure_after_dispatch_does_not_skip_response_or_repeat_settlement() {
    let fixture = codex_route_fixture("request-capture-gap").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    // Simulate a terminal background request-archive failure only once the
    // upstream response has returned and its independent capture is inserted.
    sqlx::query("CREATE TRIGGER fail_request_archive_after_dispatch AFTER INSERT ON response_archive_spools BEGIN UPDATE request_archive_spools SET state = 'gap', last_error_code = 'capture_failed' WHERE request_id = NEW.request_id AND tenant_id = NEW.tenant_id AND reservation_id = NEW.reservation_id; END")
        .execute(&pool).await.unwrap();
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("response survives request capture failure"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({
            "model": fixture.model, "input": "capture independently", "stream": false
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    let request_id = rows[0].request_id;
    assert_eq!(rows[0].cost, "0.000005");
    assert_eq!(
        rows[0].archive_state,
        crate::model::RequestArchiveState::Pending
    );
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, request_id)
        .await
        .unwrap();
    assert_eq!(
        refs.request_archive_state,
        crate::model::RequestArchiveState::Gap
    );
    assert!(refs.request_archive_reason.is_some());
    assert_eq!(
        refs.response_archive_state,
        crate::model::RequestArchiveState::Pending
    );
    assert!(refs.response_archive_reason.is_none());
    let detail = crate::api::request_detail::request_detail(&fixture.state, refs).await;
    assert!(detail.archive.request.reason.is_some());
    assert!(detail.archive.response.reason.is_none());
    assert!(!detail.archive.response.complete);
    drain_completed_response_archive(&fixture).await;
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, request_id)
        .await
        .unwrap();
    assert_eq!(
        refs.view.archive_state,
        crate::model::RequestArchiveState::Gap
    );
    assert_eq!(
        refs.response_archive_state,
        crate::model::RequestArchiveState::Bound
    );
    assert_eq!(
        fixture
            .state
            .archive
            .get(refs.response_object.as_deref().unwrap())
            .await
            .unwrap()
            .as_ref(),
        delivered.as_ref()
    );
    assert_exactly_once_side_effects(&fixture, request_id, Some("resp-codex")).await;
    upstream.verify().await;
    pool.close().await;
}

#[tokio::test]
async fn request_capture_failure_rejects_before_upstream_and_rolls_back_admission() {
    let fixture = codex_route_fixture("request-capture-admission-failure").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_request_capture BEFORE INSERT ON request_archive_spools BEGIN SELECT RAISE(ABORT, 'fixture capture rejection'); END")
        .execute(&pool).await.unwrap();
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({
            "model": fixture.model, "input": "must be durable before dispatch", "stream": false
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(response.headers().contains_key(header::RETRY_AFTER));
    let requests: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_records")
        .fetch_one(&pool)
        .await
        .unwrap();
    let reservations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((requests, reservations), (0, 0));
    upstream.verify().await;
    pool.close().await;
}

#[tokio::test]
async fn incomplete_or_failed_tail_revokes_held_success_terminal() {
    for (label, payload, expected_gap) in [
        (
            "incomplete",
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"usable text\"}}]}\n\n",
                "data: [DONE]\n\ndata: ",
            ),
            true,
        ),
        (
            "failed",
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"usable text\"}}]}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture\"}}\n\n",
                "data: {\"error\":{\"message\":\"private provider detail\"}}\n\n",
            ),
            false,
        ),
    ] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(payload, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;
        let fixture = resilient_route_fixture(label, &[(upstream.uri(), 0)]).await;
        let response = send_resilient_chat(&fixture, None, true).await;
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("usable text"));
        assert!(text.contains("upstream stream did not complete"));
        assert!(!text.contains("[DONE]"));
        assert!(!text.contains("response.completed"));
        assert!(!text.contains("private provider detail"));
        if expected_gap {
            let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
            let state: String = sqlx::query_scalar("SELECT state FROM response_archive_spools")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(state, "gap");
            pool.close().await;
        }
        upstream.verify().await;
    }
}

#[tokio::test]
async fn terminal_delivery_observes_sealed_spool_or_explicit_capture_gap() {
    for fail_append in [false, true] {
        let upstream = MockServer::start().await;
        let payload = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"usable text\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(payload, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;
        let fixture = resilient_route_fixture(
            if fail_append {
                "terminal-gap"
            } else {
                "terminal-sealed"
            },
            &[(upstream.uri(), 0)],
        )
        .await;
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        if fail_append {
            // A SQLite RAISE(ABORT) queues transaction rollback when SQLx drops
            // the failed append. Under executor load that rollback can delay
            // the gap write past its separate ACK budget, injecting two
            // failures instead of the one this contract covers. The
            // fixture-scoped, one-shot latch fails only the producer append and
            // leaves gap persistence healthy.
            crate::response_archive_spool::fail_next_append_for_test(&fixture.state);
        }
        let response = send_resilient_chat(&fixture, None, true).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(body.as_ref(), payload.as_bytes());
        // No polling: receipt of terminal/EOF itself guarantees that capture
        // is no longer left as an unrecoverable "capturing" success.
        let (state, gap_reason): (String, Option<String>) =
            sqlx::query_as("SELECT state, last_error_code FROM response_archive_spools")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, if fail_append { "gap" } else { "pending" });
        assert_eq!(
            gap_reason.as_deref(),
            fail_append.then_some("capture_failed")
        );
        upstream.verify().await;
        pool.close().await;
    }
}
