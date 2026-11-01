use super::*;

async fn fixture(label: &str, upstream: &MockServer) -> ResilientRouteFixture {
    let fixture = resilient_route_fixture(label, &[(upstream.uri(), 0)]).await;
    set_active(&fixture, false).await;
    fixture
}

async fn set_active(fixture: &ResilientRouteFixture, active: bool) {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE upstream_accounts SET status = $1 WHERE id = $2")
        .bind(if active { "active" } else { "disabled" })
        .bind(fixture.accounts[0].to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

fn rejected_chat(fixture: &ResilientRouteFixture) -> tokio::task::JoinHandle<Response> {
    let router = router_for_role(fixture.state.clone(), RuntimeRole::Gateway);
    let request = Request::post("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
        .body(Body::from(
            json!({
                "model": fixture.model,
                "messages": [{"role":"user", "content":"x".repeat(256 * 1024)}],
            })
            .to_string(),
        ))
        .unwrap();
    tokio::spawn(async move { router.oneshot(request).await.unwrap() })
}

async fn wait_for_pending(fixture: &ResilientRouteFixture, expected: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        // The detached request spool writer may still own ingress permits
        // after the handler returns. Observe journal isolation only once that
        // owner exits, while the journal is still pending. Cancellation must
        // continue to check queue and memory release without waiting on spool.
        while fixture.state.early_requests.pending_writes_for_test() != expected
            || (expected != 0
                && fixture
                    .state
                    .responses_request_spool
                    .snapshot()
                    .active_files
                    != 0)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("journal queue reached expected ownership state");
}

async fn assert_no_request_memory_after_spool(fixture: &ResilientRouteFixture) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture
            .state
            .responses_request_spool
            .snapshot()
            .active_files
            != 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("rejected request spool owners must release independently of the journal");
    assert_no_request_memory(fixture);
}

fn assert_no_request_memory(fixture: &ResilientRouteFixture) {
    let (used, _, retained, _) = fixture.state.proxy_memory_budget.snapshot();
    assert_eq!(used, 0, "journal wait must not own ingress budget");
    assert_eq!(
        retained, 0,
        "journal wait must not own retained request budget"
    );
}

#[tokio::test]
async fn blocked_journal_releases_rejected_chat_memory_before_valid_forwarding() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(successful_chat_response())
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = fixture("journal-forward-isolation", &upstream).await;
    let writer = fixture.state.early_requests.hold_writer_for_test().await;
    let first = rejected_chat(&fixture);
    wait_for_pending(&fixture, 1).await;
    assert_no_request_memory(&fixture);
    let second = rejected_chat(&fixture);
    let (first, second) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(first, second)
    })
    .await
    .expect("blocked journal must not indefinitely hold rejected responses");
    let rejected = [first.unwrap(), second.unwrap()];
    for response in &rejected {
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["x-mtc-request-recording"], "unavailable");
    }
    assert_no_request_memory_after_spool(&fixture).await;

    set_active(&fixture, true).await;
    let valid = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(valid.status(), StatusCode::OK);
    assert!(!valid.headers().contains_key("x-mtc-request-recording"));
    let _ = to_bytes(valid.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    drop(writer);

    for response in rejected {
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "no_authorized_upstream_candidate");
    }
    fixture.state.early_requests.drain_for_test().await;
    upstream.verify().await;
}

#[tokio::test]
async fn cancelling_journal_wait_releases_queue_and_memory_without_phantom_terminal() {
    let upstream = MockServer::start().await;
    let fixture = fixture("journal-cancel-wait", &upstream).await;
    let writer = fixture.state.early_requests.hold_writer_for_test().await;
    let request = rejected_chat(&fixture);
    wait_for_pending(&fixture, 1).await;
    assert_no_request_memory(&fixture);
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    wait_for_pending(&fixture, 0).await;
    assert_no_request_memory(&fixture);
    drop(writer);
    fixture.state.early_requests.drain_for_test().await;
    assert!(
        fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn journal_wait_timeout_is_explicit_and_recovery_projects_only_once() {
    let upstream = MockServer::start().await;
    let fixture = fixture("journal-timeout-recovery", &upstream).await;
    let writer = fixture.state.early_requests.hold_writer_for_test().await;
    let rejected = tokio::time::timeout(Duration::from_secs(3), rejected_chat(&fixture))
        .await
        .expect("journal acknowledgment wait is bounded")
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(rejected.headers()["x-mtc-request-recording"], "unavailable");
    assert_eq!(fixture.state.early_requests.pending_writes_for_test(), 0);
    assert_no_request_memory_after_spool(&fixture).await;
    assert!(
        fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    drop(writer);
    let recovered = rejected_chat(&fixture).await.unwrap();
    assert_eq!(recovered.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!recovered.headers().contains_key("x-mtc-request-recording"));
    let request_id =
        Uuid::parse_str(recovered.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    fixture.state.early_requests.drain_for_test().await;
    fixture.state.early_requests.drain_for_test().await;
    let records = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].request_id, request_id);
    assert!(!records[0].billing.billable);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    for statement in [
        "SELECT COUNT(*) FROM request_events WHERE event_kind = 'finished'",
        "SELECT COUNT(*) FROM request_stats_facts WHERE status_class = 'failure'",
    ] {
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(statement.to_owned()))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
    pool.close().await;
}

#[tokio::test]
async fn unavailable_journal_filesystem_keeps_original_error_and_no_memory_owner() {
    let upstream = MockServer::start().await;
    let mut fixture = fixture("journal-filesystem-unavailable", &upstream).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("blocked-journal");
    tokio::fs::write(&path, b"synthetic obstruction")
        .await
        .unwrap();
    fixture.state.early_requests = crate::early_request::Journal::new(
        path.clone(),
        fixture.state.db.independent_persistence_pool(),
    );
    let rejected = rejected_chat(&fixture).await.unwrap();
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        !rejected.headers().contains_key("x-mtc-request-recording"),
        "a blocked journal path must not drop the database record"
    );
    assert_no_request_memory_after_spool(&fixture).await;
    let body: Value =
        serde_json::from_slice(&to_bytes(rejected.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "no_authorized_upstream_candidate");
    fixture.state.early_requests.drain_for_test().await;
    assert_eq!(
        fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    tokio::fs::remove_file(&path).await.unwrap();
    let recovered = rejected_chat(&fixture).await.unwrap();
    assert!(!recovered.headers().contains_key("x-mtc-request-recording"));
    fixture.state.early_requests.drain_for_test().await;
    assert_eq!(
        fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
}
