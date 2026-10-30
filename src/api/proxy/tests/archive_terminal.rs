use super::*;

async fn assert_unknown_archive_terminal(
    fixture: &ResilientRouteFixture,
    status_code: i64,
    error_code: Option<&str>,
) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let rows = fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap();
            if rows.len() == 1
                && rows[0].operational_terminal.as_ref().is_some_and(|terminal| {
                    terminal.financial_state == crate::model::RequestFinancialState::Unknown
                })
            {
                let record = &rows[0];
                assert_eq!(record.status_code, Some(status_code));
                assert_eq!(record.error_code.as_deref(), error_code);
                assert_eq!(record.completed_at, None);
                assert_eq!(record.billing.cost, None);
                assert!(record.usage.tokens.is_none());
                let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
                let financial = sqlx::query(
                    "SELECT q.completed_at, r.status, r.actual_micros, p.state, p.receipt_id, p.receipt_digest, p.cost_micros, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = q.reservation_id) AS ledger_count FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id JOIN pending_proxy_settlements p ON p.request_id = q.id WHERE q.id = $1",
                )
                .bind(record.request_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
                assert_eq!(financial.get::<Option<i64>, _>("completed_at"), None);
                assert_eq!(financial.get::<String, _>("status"), "reserved");
                assert_eq!(financial.get::<Option<i64>, _>("actual_micros"), None);
                assert_eq!(financial.get::<String, _>("state"), "pending");
                assert_eq!(financial.get::<Option<String>, _>("receipt_id"), None);
                assert_eq!(financial.get::<Option<String>, _>("receipt_digest"), None);
                assert_eq!(financial.get::<Option<i64>, _>("cost_micros"), None);
                assert_eq!(financial.get::<i64, _>("ledger_count"), 0);
                pool.close().await;
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("operational completion must preserve unknown finance and its uncharged hold");
}

#[tokio::test]
async fn encoded_buffered_upstream_is_rejected_once_without_replay() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(successful_chat_response().insert_header("content-encoding", "gzip"))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = resilient_route_fixture("encoded-buffered", &[(upstream.uri(), 0)]).await;
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_unknown_archive_terminal(&fixture, 502, Some("upstream_invalid_content_encoding")).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_invalid_content_encoding")
    );
    upstream.verify().await;
}

#[tokio::test]
async fn request_archive_failure_after_dispatch_does_not_skip_response_or_repeat_settlement() {
    let fixture = codex_route_fixture("request-capture-gap").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
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
    let response = fixture
        .state
        .db
        .with_request_archive_capture_for_test(send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({
                "model": fixture.model, "input": "capture independently", "stream": false
            }),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    drain_archive_capture(&fixture).await;
    let failed = sqlx::query("UPDATE request_archive_spools SET state = 'gap', last_error_code = 'capture_failed' WHERE EXISTS (SELECT 1 FROM request_records r WHERE r.id = request_archive_spools.request_id AND r.tenant_id = request_archive_spools.tenant_id AND r.reservation_id = request_archive_spools.reservation_id)")
        .execute(&pool).await.unwrap();
    assert_eq!(failed.rows_affected(), 1);
    wait_for_request_settlement(&fixture, 1).await;
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
        crate::model::RequestArchiveState::Gap
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
        crate::model::RequestArchiveState::Bound
    );
    assert!(refs.response_archive_reason.is_none());
    let detail = crate::api::request_detail::request_detail(&fixture.state, refs).await;
    assert!(detail.archive.request.reason.is_some());
    assert!(detail.archive.response.reason.is_none());
    assert!(detail.archive.response.complete);
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
async fn request_capture_failure_preserves_admission_and_dispatches_once() {
    let fixture = codex_route_fixture("request-capture-admission-failure").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_request_capture BEFORE INSERT ON request_archive_spools BEGIN SELECT RAISE(ABORT, 'fixture capture rejection'); END")
        .execute(&pool).await.unwrap();
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("response survives optional spool rejection"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let request = json!({
        "model": fixture.model, "input": "only safety must be durable before dispatch", "stream": false
    });
    let response = send_codex_route(&fixture, &upstream, "/v1/responses", request.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&delivered).unwrap()["output"][0]["content"][0]["text"],
        "response survives optional spool rejection"
    );
    let requests: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_records")
        .fetch_one(&pool)
        .await
        .unwrap();
    let reservations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((requests, reservations), (1, 1));
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    gateway_persistence::assert_readable_source_without_spool(
        &fixture,
        &pool,
        rows[0].request_id,
        &request,
    )
    .await;
    sqlx::query("DROP TRIGGER reject_request_capture")
        .execute(&pool)
        .await
        .unwrap();
    drain_completed_response_archive(&fixture).await;
    wait_for_request_settlement(&fixture, 1).await;
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex")).await;
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
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let row: Option<(String, String)> =
                        sqlx::query_as("SELECT tenant_id, id FROM request_records LIMIT 1")
                            .fetch_optional(&pool)
                            .await
                            .unwrap();
                    if let Some((tenant, request)) = row {
                        let tenant = Uuid::parse_str(&tenant).unwrap();
                        let request = Uuid::parse_str(&request).unwrap();
                        if fixture
                            .state
                            .archive
                            .raw_capture_failed(tenant, request, "response")
                            .await
                            .unwrap()
                        {
                            assert!(
                                fixture
                                    .state
                                    .archive
                                    .raw_capture_reference(tenant, request, "response")
                                    .await
                                    .unwrap()
                                    .is_none()
                            );
                            break;
                        }
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("owned writer must durably fence the incomplete capture");
            pool.close().await;
        }
        upstream.verify().await;
    }
}

#[tokio::test]
async fn terminal_delivery_transfers_capture_to_the_owned_writer_or_records_a_gap() {
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
        let begin_pause = (!fail_append)
            .then(|| crate::response_archive_spool::pause_next_begin_for_test(&fixture.state));
        let fence_probe = fail_append
            .then(|| crate::response_archive_spool::fence_probe::install(&fixture.state));
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
        let body =
            tokio::spawn(async move { to_bytes(response.into_body(), 1024 * 1024).await.unwrap() });
        let begin_release = if let Some((entering, release)) = begin_pause {
            tokio::time::timeout(Duration::from_secs(1), entering)
                .await
                .expect("owned writer must reach the deterministic pre-begin pause")
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let row: Option<(String, Option<i64>, String)> = sqlx::query_as(
                        "SELECT q.id, q.completed_at, o.response_object FROM request_records q JOIN proxy_operational_terminals o ON o.request_id = q.id WHERE o.completed_at IS NOT NULL",
                    )
                    .fetch_optional(&pool)
                    .await
                    .unwrap();
                    if let Some((request_id, financial_completed_at, response_object)) = row
                        && response_object == format!("gap://{request_id}/response")
                    {
                        assert_eq!(financial_completed_at, None);
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("short stream must record its operational terminal before the paused writer begins");
            Some(release)
        } else {
            None
        };
        let fence_owner = if let Some((probe, entered, release)) = fence_probe {
            tokio::time::timeout(Duration::from_secs(5), entered)
                .await
                .expect("abandoned capture must reach its owned fence")
                .unwrap();
            Some((probe, release))
        } else {
            None
        };
        let body = tokio::time::timeout(Duration::from_secs(5), body)
            .await
            .expect("terminal delivery and EOF must not await a cancelled writer fence")
            .unwrap();
        assert_eq!(body.as_ref(), payload.as_bytes());
        if let Some(release) = begin_release {
            release.send(()).unwrap();
        }
        let fence_probe = fence_owner.map(|(probe, release)| {
            assert_eq!(
                probe.calls(),
                1,
                "terminal path must not start a duplicate gap write"
            );
            release.send(()).unwrap();
            probe
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let row: Option<(String, String)> =
                    sqlx::query_as("SELECT tenant_id, id FROM request_records LIMIT 1")
                        .fetch_optional(&pool)
                        .await
                        .unwrap();
                if let Some((tenant, request)) = row {
                    let tenant = Uuid::parse_str(&tenant).unwrap();
                    let request = Uuid::parse_str(&request).unwrap();
                    let reference = fixture
                        .state
                        .archive
                        .raw_capture_reference(tenant, request, "response")
                        .await
                        .unwrap();
                    if fail_append {
                        if fixture
                            .state
                            .archive
                            .raw_capture_failed(tenant, request, "response")
                            .await
                            .unwrap()
                        {
                            assert!(reference.is_none());
                            break;
                        }
                    } else if let Some(reference) = reference {
                        let captured = fixture
                            .state
                            .archive
                            .get_tenant_content(tenant, &reference, 1024 * 1024)
                            .await
                            .unwrap();
                        assert_eq!(captured.as_ref(), payload.as_bytes());
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the owned writer must eventually seal or fence the capture");
        let chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spool_chunks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(chunks, 0);
        if let Some(probe) = fence_probe {
            assert_eq!(probe.calls(), 1);
        }
        assert_unknown_archive_terminal(&fixture, 200, None).await;
        upstream.verify().await;
        pool.close().await;
    }
}
