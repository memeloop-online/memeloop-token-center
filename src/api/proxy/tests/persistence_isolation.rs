use super::*;

mod buffered_semantic_isolation;

#[tokio::test]
async fn deferred_persistence_normal_failure_and_saturation_preserve_forwarding() {
    for mode in [
        "normal",
        "failure",
        "response_failure",
        "response_failure_persistent",
        "saturation",
        "pool_starvation",
        "pool_closed",
    ] {
        for stream in [false, true] {
            eprintln!("persistence archive scenario: {mode}, stream={stream}");
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
                sqlx::query("CREATE TRIGGER reject_request_archive_spools BEFORE INSERT ON request_archive_spools BEGIN SELECT RAISE(ABORT, 'archive failure'); END")
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            if matches!(
                mode,
                "failure" | "response_failure" | "response_failure_persistent"
            ) {
                sqlx::query("CREATE TRIGGER reject_response_archive_spools BEFORE INSERT ON response_archive_spools BEGIN SELECT RAISE(ABORT, 'archive failure'); END")
                    .execute(&pool)
                    .await
                    .unwrap();
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
            let begin_failure = if matches!(mode, "failure" | "response_failure") {
                Some(
                    crate::response_archive_spool::pause_next_begin_failure_for_test(
                        &fixture.state,
                    ),
                )
            } else {
                None
            };
            let upstream = MockServer::start().await;
            let request = json!({"model": fixture.model, "input": "isolation", "stream": stream});
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(
                    completed_codex_sse("forwarding stays available"),
                    "text/event-stream",
                ))
                .expect(1)
                .mount(&upstream)
                .await;
            let delivered = tokio::time::timeout(Duration::from_secs(2), async {
                let response =
                    send_codex_route(&fixture, &upstream, "/v1/responses", request.clone()).await;
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
            let release_begin_failure = if let Some((entered, release)) = begin_failure {
                tokio::time::timeout(Duration::from_secs(3), entered)
                    .await
                    .expect("response spool begin must report its database failure")
                    .unwrap();
                Some(release)
            } else {
                None
            };
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
            assert_settlement_once_with_inline_request(&fixture, rows[0].request_id).await;
            tokio::time::timeout(
                Duration::from_secs(5),
                fixture
                    .state
                    .responses_request_spool
                    .drain_uploads_for_test(),
            )
            .await
            .expect("request source upload must finish independently of response metadata");
            match mode {
                "normal" | "saturation" | "pool_starvation" => {
                    assert_exactly_once_side_effects(
                        &fixture,
                        rows[0].request_id,
                        Some("resp-codex"),
                    )
                    .await;
                }
                "failure" | "response_failure" => {
                    assert!(
                        fixture
                            .state
                            .db
                            .session_body_locators(rows[0].request_id)
                            .await
                            .unwrap()
                            .unwrap()
                            .pending
                    );
                    let failed_refs = fixture
                        .state
                        .db
                        .request_archive_refs(fixture.key_id, rows[0].request_id)
                        .await
                        .unwrap();
                    assert_eq!(
                        failed_refs.response_archive_state,
                        crate::model::RequestArchiveState::Gap
                    );
                    if mode == "response_failure" {
                        assert_eq!(
                            failed_refs.request_archive_state,
                            crate::model::RequestArchiveState::Bound
                        );
                    }
                    if mode == "failure" {
                        gateway_persistence::assert_readable_source_without_spool(
                            &fixture,
                            &pool,
                            rows[0].request_id,
                            &request,
                        )
                        .await;
                        sqlx::query("DROP TRIGGER reject_request_archive_spools")
                            .execute(&pool)
                            .await
                            .unwrap();
                    }
                    sqlx::query("DROP TRIGGER reject_response_archive_spools")
                        .execute(&pool)
                        .await
                        .unwrap();
                    release_begin_failure.unwrap().send(()).unwrap();
                    fixture.state.db.drain_gateway_persistence_for_test().await;
                    assert_exactly_once_side_effects(
                        &fixture,
                        rows[0].request_id,
                        Some("resp-codex"),
                    )
                    .await;
                    let refs = fixture
                        .state
                        .db
                        .request_archive_refs(fixture.key_id, rows[0].request_id)
                        .await
                        .unwrap();
                    assert_eq!(
                        refs.request_archive_state,
                        crate::model::RequestArchiveState::Bound
                    );
                    assert_eq!(
                        refs.response_archive_state,
                        crate::model::RequestArchiveState::Bound
                    );
                    let captured = fixture
                        .state
                        .archive
                        .get(refs.response_object.as_deref().unwrap())
                        .await
                        .unwrap();
                    let expected = if stream {
                        Bytes::from(completed_codex_sse("forwarding stays available"))
                    } else {
                        delivered.clone()
                    };
                    assert_eq!(captured, expected);
                    assert_eq!(blake3::hash(&captured), blake3::hash(&expected));
                    assert_eq!(
                        fixture
                            .state
                            .archive
                            .get(&refs.request_object)
                            .await
                            .unwrap()
                            .as_ref(),
                        serde_json::to_vec(&request).unwrap().as_slice()
                    );
                }
                "response_failure_persistent" => {
                    drain_archive_capture(&fixture).await;
                    let gaps: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'archive_gap' AND error_code = 'response_capture_failed'")
                        .bind(rows[0].request_id.to_string())
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                    assert_eq!(gaps, 1);
                    assert!(
                        !fixture
                            .state
                            .db
                            .session_body_locators(rows[0].request_id)
                            .await
                            .unwrap()
                            .unwrap()
                            .pending
                    );
                    assert_settlement_once_with_inline_request(&fixture, rows[0].request_id).await;
                    let refs = fixture
                        .state
                        .db
                        .request_archive_refs(fixture.key_id, rows[0].request_id)
                        .await
                        .unwrap();
                    assert_eq!(
                        refs.request_archive_state,
                        crate::model::RequestArchiveState::Bound
                    );
                    assert_eq!(
                        refs.response_archive_state,
                        crate::model::RequestArchiveState::Gap
                    );
                    assert_exactly_once_side_effects(
                        &fixture,
                        rows[0].request_id,
                        Some("resp-codex"),
                    )
                    .await;
                }
                "pool_closed" => {
                    let refs = fixture
                        .state
                        .db
                        .request_archive_refs(fixture.key_id, rows[0].request_id)
                        .await
                        .unwrap();
                    assert_eq!(
                        refs.request_archive_state,
                        crate::model::RequestArchiveState::Bound
                    );
                    assert_eq!(
                        crate::gateway_body::request_spool::source_request_id(&refs.request_object),
                        Some(rows[0].request_id)
                    );
                    assert!(matches!(
                        refs.response_archive_state,
                        crate::model::RequestArchiveState::Pending
                            | crate::model::RequestArchiveState::Gap
                    ));
                }
                _ => unreachable!("the persistence mode list is exhaustive"),
            }
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

#[tokio::test]
async fn buffered_capture_capacity_gap_preserves_delivery_source_and_settlement_once() {
    let fixture = codex_route_fixture("buffered-capture-capacity-gap").await;
    let held = fixture
        .state
        .persistence
        .hold_capture_capacity_for_test()
        .await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("complete at archive capacity"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let request =
        json!({"model": fixture.model, "input": "preserve complete request", "stream": false});
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        send_codex_route(&fixture, &upstream, "/v1/responses", request.clone()),
    )
    .await
    .expect("capture capacity must not queue the model request");
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&delivered).unwrap()["output"][0]["content"][0]["text"],
        "complete at archive capacity"
    );
    wait_for_request_settlement(&fixture, 1).await;
    drop(held);
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex")).await;
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    assert_eq!(
        refs.request_archive_state,
        crate::model::RequestArchiveState::Bound
    );
    assert_eq!(
        refs.response_archive_state,
        crate::model::RequestArchiveState::Gap
    );
    assert_eq!(
        refs.response_object,
        Some(format!("gap://{}/response/capacity", rows[0].request_id))
    );
    assert!(
        !fixture
            .state
            .db
            .session_body_locators(rows[0].request_id)
            .await
            .unwrap()
            .unwrap()
            .pending
    );
    let original = serde_json::to_vec(&request).unwrap();
    let captured = fixture
        .state
        .archive
        .get(&refs.request_object)
        .await
        .unwrap();
    assert_eq!(captured.as_ref(), original.as_slice());
    assert_eq!(blake3::hash(&captured), blake3::hash(&original));
    upstream.verify().await;
}

#[tokio::test]
async fn response_archive_binding_and_terminal_receipt_preserve_both_completion_orders() {
    archive_terminal_completion_orders(None).await;
}

#[tokio::test]
async fn postgres_response_archive_binding_and_terminal_receipt_preserve_both_completion_orders() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "PostgreSQL required in CI"
        );
        return;
    };
    let isolated = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    archive_terminal_completion_orders(Some(isolated.url())).await;
    isolated.cleanup().await;
}

async fn archive_terminal_completion_orders(database_url: Option<&str>) {
    for bind_first in [true, false] {
        for stream in [false, true] {
            let fixture = codex_route_fixture_with_database(
                &format!("archive-terminal-order-{bind_first}-{stream}"),
                "archive",
                database_url,
            )
            .await;
            let held = if bind_first {
                Some(fixture.state.pending_terminals.hold_writer_for_test().await)
            } else {
                None
            };
            let upstream = MockServer::start().await;
            let sse = completed_codex_sse("preserve the bound response");
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(sse.clone(), "text/event-stream"),
                )
                .expect(1)
                .mount(&upstream)
                .await;
            let request = json!({"model": fixture.model, "input": "keep source and response ownership", "stream": stream});
            let response =
                send_codex_route(&fixture, &upstream, "/v1/responses", request.clone()).await;
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
            assert_eq!(rows.len(), 1);
            let request_id = rows[0].request_id;
            let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
            if bind_first {
                let completed: Option<i64> =
                    sqlx::query_scalar("SELECT completed_at FROM request_records WHERE id = $1")
                        .bind(request_id.to_string())
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                assert_eq!(completed, None);
            } else {
                wait_for_request_settlement(&fixture, 1).await;
            }
            drain_archive_capture(&fixture).await;
            assert!(crate::response_archive_spool::process_one_for_test(&fixture.state).await);
            let before = fixture
                .state
                .db
                .request_archive_refs(fixture.key_id, request_id)
                .await
                .unwrap();
            assert_eq!(
                before.response_archive_state,
                crate::model::RequestArchiveState::Bound
            );
            let bound = before.response_object.unwrap();
            if bind_first {
                let completed: Option<i64> =
                    sqlx::query_scalar("SELECT completed_at FROM request_records WHERE id = $1")
                        .bind(request_id.to_string())
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                assert_eq!(
                    completed, None,
                    "archive binding precedes financial terminal"
                );
            }
            drop(held);
            wait_for_request_settlement(&fixture, 1).await;
            assert_exactly_once_side_effects(&fixture, request_id, Some("resp-codex")).await;
            let refs = fixture
                .state
                .db
                .request_archive_refs(fixture.key_id, request_id)
                .await
                .unwrap();
            assert_eq!(
                refs.request_archive_state,
                crate::model::RequestArchiveState::Bound
            );
            assert_eq!(
                refs.response_archive_state,
                crate::model::RequestArchiveState::Bound
            );
            assert_eq!(refs.response_object.as_deref(), Some(bound.as_str()));
            let captured = fixture.state.archive.get(&bound).await.unwrap();
            let expected = if stream { Bytes::from(sse) } else { delivered };
            assert_eq!(captured, expected);
            assert_eq!(blake3::hash(&captured), blake3::hash(&expected));
            let original = serde_json::to_vec(&request).unwrap();
            let captured = fixture
                .state
                .archive
                .get(&refs.request_object)
                .await
                .unwrap();
            assert_eq!(captured.as_ref(), original.as_slice());
            assert_eq!(blake3::hash(&captured), blake3::hash(&original));
            upstream.verify().await;
            pool.close().await;
            fixture.state.db.close().await;
            fixture.state.persistence_db.close().await;
        }
    }
}
