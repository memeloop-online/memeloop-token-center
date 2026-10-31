use super::*;

#[tokio::test]
async fn original_capture_owns_silent_and_empty_bodies_through_shutdown() {
    for empty in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::initialize(crate::config::Config::for_test(format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("lazy-original.db").display()
        )))
        .await
        .unwrap();
        let identity = crate::db::ArchiveSpoolIdentity {
            tenant_id: Uuid::now_v7(),
            request_id: Uuid::now_v7(),
            reservation_id: Uuid::now_v7(),
        };
        let expected = if empty {
            Bytes::new()
        } else {
            Bytes::from_static(b"body after silent headers")
        };
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let response =
            UpstreamResponse::Prefetched {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                version: http::Version::HTTP_11,
                content_length: None,
                stream: Box::pin(futures_util::stream::unfold(
                    receiver,
                    |mut receiver| async move {
                        receiver.recv().await.map(|bytes| (Ok(bytes), receiver))
                    },
                )),
            }
            .with_original_archive_capture(&state, identity);
        assert_eq!(
            state.proxy_archive_stream_permits.available_permits(),
            crate::PROXY_ARCHIVE_STREAM_CONCURRENCY
        );
        assert_eq!(state.persistence.stream_memory.snapshot().0, 0);
        assert_eq!(state.persistence.pending_archives_for_test(), 1);
        // Dropping before the first poll must retain the original, including
        // a genuinely empty upstream response, and keep shutdown waiting.
        drop(response);
        let mut drain = Box::pin(state.drain_proxy_tasks());
        assert!(futures_util::poll!(&mut drain).is_pending());
        if !empty {
            sender.send(expected.clone()).await.unwrap();
        }
        drop(sender);
        tokio::time::timeout(Duration::from_secs(10), drain)
            .await
            .unwrap();
        let object = state
            .archive
            .raw_capture_reference(identity.tenant_id, identity.request_id, "response")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(object.size_bytes, expected.len() as u64);
        assert_eq!(
            object.blake3_digest,
            blake3::hash(&expected).to_hex().to_string()
        );
        assert_eq!(
            state
                .archive
                .get_tenant_content(identity.tenant_id, &object, expected.len())
                .await
                .unwrap(),
            expected
        );
        assert!(
            !state
                .archive
                .raw_capture_failed(identity.tenant_id, identity.request_id, "response")
                .await
                .unwrap()
        );
        assert_eq!(state.persistence.pending_archives_for_test(), 0);
        assert_eq!(state.persistence.stream_memory.snapshot().0, 0);
    }
}

#[tokio::test]
async fn original_capture_waits_for_bounded_writers_without_losing_overlapping_bodies() {
    use crate::response_archive_spool::ResponseArchiveProducer;
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::initialize(crate::config::Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("overlapping-originals.db").display()
    )))
    .await
    .unwrap();
    let mut captures = Vec::new();
    let mut expected = Vec::new();
    for index in 0..crate::PROXY_ARCHIVE_STREAM_CONCURRENCY {
        let identity = crate::db::ArchiveSpoolIdentity {
            tenant_id: Uuid::now_v7(),
            request_id: Uuid::now_v7(),
            reservation_id: Uuid::now_v7(),
        };
        let body = Bytes::from(vec![index as u8; 128 * 1024]);
        let capture = ResponseArchiveProducer::begin_original(&state, identity)
            .await
            .unwrap();
        captures.push(capture);
        expected.push((identity, body));
    }
    assert_eq!(state.proxy_archive_stream_permits.available_permits(), 0);
    assert_eq!(
        state.persistence.stream_memory.snapshot().0,
        crate::response_archive_spool::RAW_STREAM_MEMORY_BYTES
    );
    let identity = crate::db::ArchiveSpoolIdentity {
        tenant_id: Uuid::now_v7(),
        request_id: Uuid::now_v7(),
        reservation_id: Uuid::now_v7(),
    };
    let mut waiting = Box::pin(ResponseArchiveProducer::begin_original(&state, identity));
    assert!(futures_util::poll!(&mut waiting).is_pending());
    let mut first = captures.remove(0);
    assert!(first.append_with_backpressure(expected[0].1.clone()).await);
    first.seal().unwrap().wait().await;
    let admitted = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    captures.push(admitted);
    expected.push((identity, Bytes::from(vec![99_u8; 128 * 1024])));
    for (mut capture, (_, body)) in captures.into_iter().zip(expected.iter().skip(1)) {
        assert!(capture.append_with_backpressure(body.clone()).await);
        capture.seal().unwrap().wait().await;
    }
    for (identity, body) in expected {
        let object = state
            .archive
            .raw_capture_reference(identity.tenant_id, identity.request_id, "response")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(object.size_bytes, body.len() as u64);
        assert_eq!(
            object.blake3_digest,
            blake3::hash(&body).to_hex().to_string()
        );
        assert_eq!(
            state
                .archive
                .get_tenant_content(identity.tenant_id, &object, body.len())
                .await
                .unwrap(),
            body
        );
        assert!(
            !state
                .archive
                .raw_capture_failed(identity.tenant_id, identity.request_id, "response")
                .await
                .unwrap()
        );
    }
    assert_eq!(state.persistence.stream_memory.snapshot().0, 0);
}

#[tokio::test]
async fn cancelled_original_admission_records_missing_evidence_without_leaking_permits() {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::initialize(crate::config::Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory
            .path()
            .join("cancel-original-admission.db")
            .display()
    )))
    .await
    .unwrap();
    let permits = state
        .proxy_archive_stream_permits
        .clone()
        .acquire_many_owned(crate::PROXY_ARCHIVE_STREAM_CONCURRENCY as u32)
        .await
        .unwrap();
    let identity = crate::db::ArchiveSpoolIdentity {
        tenant_id: Uuid::now_v7(),
        request_id: Uuid::now_v7(),
        reservation_id: Uuid::now_v7(),
    };
    let mut waiting = Box::pin(
        crate::response_archive_spool::ResponseArchiveProducer::begin_original(&state, identity),
    );
    assert!(futures_util::poll!(&mut waiting).is_pending());
    drop(waiting);
    state.persistence.drain_for_test().await;
    assert!(
        state
            .archive
            .raw_capture_failed(identity.tenant_id, identity.request_id, "response")
            .await
            .unwrap()
    );
    assert!(
        state
            .archive
            .raw_capture_reference(identity.tenant_id, identity.request_id, "response")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(state.persistence.stream_memory.snapshot().0, 0);
    drop(permits);
    assert_eq!(
        state.proxy_archive_stream_permits.available_permits(),
        crate::PROXY_ARCHIVE_STREAM_CONCURRENCY
    );
}

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
    let original = br#"{ "opaque": "exact rejected encoding bytes" }"#;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(original.as_slice(), "application/json")
                .insert_header("content-encoding", "gzip"),
        )
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
    let _writers = tokio::time::timeout(
        Duration::from_secs(5),
        fixture
            .state
            .proxy_archive_stream_permits
            .acquire_many(crate::PROXY_ARCHIVE_STREAM_CONCURRENCY as u32),
    )
    .await
    .unwrap()
    .unwrap();
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    assert_eq!(
        refs.response_archive_state,
        crate::model::RequestArchiveState::Bound
    );
    assert!(
        refs.response_object
            .as_deref()
            .is_none_or(|locator| !locator.starts_with("inline-json:"))
    );
    assert_eq!(
        fixture
            .state
            .archive
            .get(refs.response_object.as_deref().unwrap())
            .await
            .unwrap()
            .as_ref(),
        original
    );
    upstream.verify().await;
}

#[tokio::test]
async fn original_rejection_is_archived_without_leaking_it_into_the_delivered_error() {
    let upstream = MockServer::start().await;
    let original = br#"{ "error": { "message": "private-original-provider-text", "opaque": {"tool_call_id":"call-1"} } }"#;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(403).set_body_raw(original.as_slice(), "application/json"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = resilient_route_fixture("raw-rejection", &[(upstream.uri(), 0)]).await;
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&delivered).contains("private-original-provider-text"));
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
                if let Some(reference) = fixture
                    .state
                    .archive
                    .raw_capture_reference(tenant, request, "response")
                    .await
                    .unwrap()
                {
                    let restored = fixture
                        .state
                        .archive
                        .get_tenant_content(tenant, &reference, MAX_PROXY_RESPONSE_BODY)
                        .await
                        .unwrap();
                    assert_eq!(restored.as_ref(), original.as_slice());
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("safe delivery must not replace the original archive");
    pool.close().await;
    upstream.verify().await;
}

#[tokio::test]
async fn raw_archive_preserves_upstream_framing_before_streaming_or_buffered_translation() {
    for streaming in [true, false] {
        let fixture = codex_route_fixture("raw-upstream-framing").await;
        let upstream = MockServer::start().await;
        let payload = format!(
            ": original provider heartbeat\r\n\r\n{}",
            completed_codex_sse("original tool and provider bytes")
        );
        Mock::given(method("POST"))
            .and(path(codex_transport::RESPONSES_PATH))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(payload.clone(), "text/event-stream"),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        let response = send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "capture wire bytes", "stream": streaming}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        if streaming {
            assert!(String::from_utf8_lossy(&delivered).contains("response.completed"));
        } else {
            assert_eq!(
                serde_json::from_slice::<Value>(&delivered).unwrap()["id"],
                "resp-codex"
            );
        }
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
                    if let Some(reference) = fixture
                        .state
                        .archive
                        .raw_capture_reference(tenant, request, "response")
                        .await
                        .unwrap()
                    {
                        let captured = fixture
                            .state
                            .archive
                            .get_tenant_content(tenant, &reference, MAX_PROXY_RESPONSE_BODY)
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
        .expect("original upstream bytes must be independently recoverable");
        let chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spool_chunks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(chunks, 0);
        pool.close().await;
        upstream.verify().await;
    }
}

#[tokio::test]
async fn legacy_request_metadata_failure_does_not_mask_durable_objects_or_repeat_settlement() {
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
    let _delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
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
        crate::model::RequestArchiveState::Bound
    );
    assert!(refs.request_archive_reason.is_none());
    assert_eq!(
        refs.response_archive_state,
        crate::model::RequestArchiveState::Bound
    );
    assert!(refs.response_archive_reason.is_none());
    let detail = crate::api::request_detail::request_detail(&fixture.state, refs).await;
    assert!(detail.archive.request.reason.is_none());
    assert!(detail.archive.request.complete);
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
        crate::model::RequestArchiveState::Bound
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
        completed_codex_sse("response survives request capture failure").as_bytes()
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
    // The retained metadata job retries until this injected outage is removed.
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
    for (label, payload) in [
        (
            "incomplete",
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"usable text\"}}]}\n\n",
                "data: [DONE]\n\ndata: ",
            ),
        ),
        (
            "failed",
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"usable text\"}}]}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture\"}}\n\n",
                "data: {\"error\":{\"message\":\"private provider detail\"}}\n\n",
            ),
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
        // Invalid SSE grammar changes delivery/normalization, not the bytes
        // actually returned by the supplier. A normal HTTP EOF seals those
        // bytes in both cases; a transport error is fenced separately.
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
                    if let Some(reference) = fixture
                        .state
                        .archive
                        .raw_capture_reference(tenant, request, "response")
                        .await
                        .unwrap()
                    {
                        let restored = fixture
                            .state
                            .archive
                            .get_tenant_content(tenant, &reference, MAX_PROXY_RESPONSE_BODY)
                            .await
                            .unwrap();
                        assert_eq!(restored.as_ref(), payload.as_bytes());
                        assert!(
                            !fixture
                                .state
                                .archive
                                .raw_capture_failed(tenant, request, "response")
                                .await
                                .unwrap()
                        );
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("malformed supplier SSE must remain available byte-for-byte");
        pool.close().await;
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

#[tokio::test]
async fn consumed_and_discarded_readers_retain_full_capture_with_bounded_backpressure() {
    for consume_tail in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::initialize(crate::config::Config::for_test(format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("raw-reader.db").display()
        )))
        .await
        .unwrap();
        let identity = crate::db::ArchiveSpoolIdentity {
            tenant_id: Uuid::now_v7(),
            request_id: Uuid::now_v7(),
            reservation_id: Uuid::now_v7(),
        };
        let prefix = Bytes::from_static(b"private original prefix");
        let tail = Bytes::from(vec![b'x'; 4 * 1024 * 1024]);
        let mut expected = blake3::Hasher::new();
        expected.update(&prefix);
        expected.update(&tail);
        let response = UpstreamResponse::Prefetched {
            status: StatusCode::FORBIDDEN,
            headers: HeaderMap::new(),
            version: http::Version::HTTP_11,
            content_length: None,
            stream: Box::pin(futures_util::stream::iter([Ok(prefix.clone()), Ok(tail)])),
        }
        .with_archive_capture(
            crate::response_archive_spool::ResponseArchiveProducer::begin(
                &state,
                identity,
                state.persistence.stream_memory.reservation(),
            ),
        );
        let mut reader = response.bytes_stream();
        assert_eq!(reader.next().await.unwrap().unwrap(), prefix);
        if consume_tail {
            assert_eq!(reader.next().await.unwrap().unwrap().len(), 4 * 1024 * 1024);
            assert!(reader.next().await.is_none());
        }
        drop(reader);
        let reference = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(reference) = state
                    .archive
                    .raw_capture_reference(identity.tenant_id, identity.request_id, "response")
                    .await
                    .unwrap()
                {
                    break reference;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("discarded reader must retain the entire upstream capture");
        assert_eq!(reference.size_bytes, prefix.len() as u64 + 4 * 1024 * 1024);
        assert_eq!(
            reference.blake3_digest,
            expected.finalize().to_hex().to_string()
        );
        state
            .archive
            .verify_tenant_content(identity.tenant_id, &reference)
            .await
            .unwrap();
        assert!(
            !state
                .archive
                .raw_capture_failed(identity.tenant_id, identity.request_id, "response")
                .await
                .unwrap()
        );
    }
}
