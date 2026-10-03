use super::*;

#[tokio::test]
async fn deferred_persistence_normal_failure_and_saturation_preserve_forwarding() {
    for mode in [
        "normal",
        "failure",
        "saturation",
        "pool_starvation",
        "pool_closed",
    ] {
        for stream in [false, true] {
            let fixture = codex_route_fixture(&format!("persistence-{mode}-{stream}")).await;
            let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
            let release = std::sync::Arc::new(tokio::sync::Notify::new());
            let mut held = Vec::new();
            let pool_holders = if mode == "pool_starvation" {
                fixture
                    .state
                    .persistence_db
                    .hold_group_snapshot_pool_for_tests()
                    .await
            } else {
                Vec::new()
            };
            if mode == "pool_closed" {
                fixture.state.persistence_db.close().await;
            }
            if mode == "failure" {
                for table in ["request_archive_spools", "response_archive_spools"] {
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "CREATE TRIGGER reject_{table} BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT, 'archive failure'); END"
                    ))).execute(&pool).await.unwrap();
                }
            } else if mode == "saturation" {
                for _ in 0..4 {
                    let release = release.clone();
                    let (entered, entering) = tokio::sync::oneshot::channel();
                    assert!(fixture.state.persistence.submit(1, async move {
                        entered.send(()).unwrap();
                        release.notified().await;
                        Ok(())
                    }));
                    held.push(entering);
                }
                for entering in held {
                    entering.await.unwrap();
                }
            }
            let upstream = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(
                    completed_codex_sse("forwarding stays available"),
                    "text/event-stream",
                ))
                .expect(1)
                .mount(&upstream)
                .await;
            let delivered = tokio::time::timeout(Duration::from_secs(2), async {
                let response = send_codex_route(
                    &fixture,
                    &upstream,
                    "/v1/responses",
                    json!({"model": fixture.model, "input": "isolation", "stream": stream}),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK, "{mode}, stream={stream}");
                to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                    .await
                    .unwrap()
            })
            .await
            .expect("dispatch, first byte and EOF must precede persistence release");
            assert!(
                std::str::from_utf8(&delivered)
                    .unwrap()
                    .contains("forwarding stays available")
            );
            if mode == "saturation" {
                assert!(
                    fixture
                        .state
                        .persistence
                        .render()
                        .contains("outcome=\"capacity\"}")
                );
            }
            release.notify_waiters();
            drop(pool_holders);
            wait_for_request_settlement(&fixture, 1).await;
            let rows = fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex"))
                .await;
            upstream.verify().await;
            pool.close().await;
        }
    }
}

#[tokio::test]
async fn pending_response_archive_begin_does_not_hold_first_byte_or_eof() {
    let fixture = codex_route_fixture("pending-response-archive-eof").await;
    let (entered, release) =
        crate::response_archive_spool::pause_next_begin_for_test(&fixture.state);
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("complete while archive is paused"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "no archive wait", "stream": true}),
    )
    .await;
    entered.await.unwrap();
    let delivered = tokio::time::timeout(
        Duration::from_secs(2),
        to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY),
    )
    .await
    .expect("EOF must not wait for archive begin")
    .unwrap();
    assert!(
        std::str::from_utf8(&delivered)
            .unwrap()
            .contains("complete while archive is paused")
    );
    wait_for_request_settlement(&fixture, 1).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.state.proxy_memory_budget.snapshot().0 != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("paused archive must not retain forwarding memory");
    assert!(fixture.state.persistence.stream_memory.snapshot().0 > 0);
    release.send(()).unwrap();
    upstream.verify().await;
}
