use super::*;

#[tokio::test]
async fn unavailable_request_archive_does_not_change_sse_or_exactly_once_settlement() {
    request_archive_degradation_preserves_sse(false).await;
}

#[tokio::test]
async fn saturated_request_archive_does_not_change_sse_or_exactly_once_settlement() {
    request_archive_degradation_preserves_sse(true).await;
}

async fn request_archive_degradation_preserves_sse(saturated: bool) {
    let fixture = codex_route_fixture("optional-request-archive-failure").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_optional_capture BEFORE INSERT ON request_archive_spools BEGIN SELECT RAISE(ABORT, 'synthetic archive failure'); END")
        .execute(&pool).await.unwrap();
    let capacity = if saturated {
        Some(
            fixture
                .state
                .db
                .saturate_gateway_persistence_for_test()
                .await,
        )
    } else {
        None
    };
    let upstream = MockServer::start().await;
    let sse = completed_codex_sse("archive failure must not change delivery");
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse.clone(), "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;
    let request = json!({
        "model": fixture.model, "input": "optional persistence unavailable", "stream": true,
    });
    let response = send_codex_route(&fixture, &upstream, "/v1/responses", request.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap()
            .as_ref(),
        sse.as_bytes()
    );
    wait_for_request_settlement(&fixture, 1).await;
    drop(capacity);
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    assert_readable_source_without_spool(&fixture, &pool, rows[0].request_id, &request).await;
    // Restore metadata writes before the archive helper drains retained jobs.
    sqlx::query("DROP TRIGGER reject_optional_capture")
        .execute(&pool)
        .await
        .unwrap();
    drain_completed_response_archive(&fixture).await;
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex")).await;
    upstream.verify().await;
    pool.close().await;
}

pub(super) async fn assert_readable_source_without_spool(
    fixture: &CodexRouteFixture,
    pool: &sqlx::AnyPool,
    request_id: Uuid,
    expected: &Value,
) {
    let rejected: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_archive_spools WHERE request_id = $1")
            .bind(request_id.to_string())
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(rejected, 0);
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
        crate::gateway_body::request_spool::source_request_id(&refs.request_object),
        Some(request_id)
    );
    let raw = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(raw) = fixture.state.archive.get(&refs.request_object).await {
                break raw;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("source object survives metadata persistence rejection");
    assert_eq!(serde_json::from_slice::<Value>(&raw).unwrap(), *expected);
}
