use super::*;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn failed_uploads_keep_typed_status_without_dispatch_billing_or_resource_leaks() {
    let fixture = codex_route_fixture("body-read-errors").await;
    let upstream = MockServer::start().await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let initial_account: (i64, i64) = sqlx::query_as(
        "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
    )
    .bind(fixture.credit_account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let initial_permits = fixture.state.gateway_body_read_permits.available_permits();
    let mut request_count = 0;
    for (path, maximum) in [
        (
            "/v1/responses",
            fixture.state.config.responses_body_max_bytes as usize,
        ),
        (
            "/v1/responses/compact",
            fixture.state.config.responses_body_max_bytes as usize,
        ),
        ("/v1/chat/completions", 4 * 1024 * 1024),
    ] {
        for failure in ["read", "declared_limit", "stream_limit"] {
            let (body, declared) = match failure {
                "declared_limit" => (Body::empty(), Some(maximum + 1)),
                "stream_limit" => (Body::from(vec![b'x'; maximum + 1]), None),
                _ => (
                    Body::from_stream(futures_util::stream::iter([
                        Ok(bytes::Bytes::from(vec![b'x'; 1024 * 1024])),
                        Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            "private-upload-error-canary",
                        )),
                    ])),
                    Some(maximum),
                ),
            };
            let mut request = Request::post(path)
                .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(declared) = declared {
                request = request.header(header::CONTENT_LENGTH, declared);
            }
            let request = request.body(body).unwrap();
            let response = tokio::time::timeout(
                Duration::from_secs(5),
                codex_transport::with_test_endpoint(
                    upstream.uri(),
                    router_for_role(fixture.state.clone(), RuntimeRole::Gateway).oneshot(request),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            let (status, code) = if failure == "read" {
                (StatusCode::BAD_REQUEST, "request_body_read_failed")
            } else {
                (StatusCode::PAYLOAD_TOO_LARGE, "request_body_too_large")
            };
            assert_eq!(response.status(), status, "{path}");
            assert!(
                response
                    .extensions()
                    .get::<crate::server::RejectedUpload>()
                    .is_some(),
                "{path}: admission rejection keeps HTTP/1 teardown protection"
            );
            let request_id =
                Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            let document: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(document["error"]["code"], code);
            assert!(!String::from_utf8_lossy(&body).contains("private-upload-error-canary"));
            assert!(!String::from_utf8_lossy(&body).contains(&fixture.key));
            fixture.state.early_requests.drain_for_test().await;
            request_count += 1;
            let rows = fixture
                .state
                .db
                .list_requests(fixture.key_id, 20)
                .await
                .unwrap();
            assert_eq!(rows.len(), request_count);
            let row = rows
                .iter()
                .find(|row| row.request_id == request_id)
                .unwrap();
            assert_eq!(row.status_code, Some(i64::from(status.as_u16())));
            assert_eq!(row.error_code.as_deref(), Some(code));
            assert_eq!(
                row.terminal_cause_code.as_deref(),
                Some("gateway_pre_admission")
            );
            assert_eq!(
                row.usage_basis,
                Some(crate::model::RequestUsageBasis::NotObserved)
            );
            assert!(!row.billing.billable);
            assert!(row.upstream_account_id.is_none());
            assert!(row.route_id.is_none());
            assert!(upstream.received_requests().await.unwrap().is_empty());
            assert_eq!(fixture.state.gateway_body_reads.load(Ordering::Acquire), 0);
            assert_eq!(fixture.state.proxy_memory_budget.snapshot().0, 0);
            assert_eq!(
                fixture.state.responses_request_spool.snapshot().used_bytes,
                0
            );
            assert_eq!(
                fixture
                    .state
                    .responses_request_spool
                    .snapshot()
                    .active_files,
                0
            );
            assert_eq!(
                fixture.state.gateway_body_read_permits.available_permits(),
                initial_permits
            );
            let account: (i64, i64) = sqlx::query_as(
                "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
            )
            .bind(fixture.credit_account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(account, initial_account);
        }
    }
    for statement in [
        "SELECT COUNT(*) FROM usage_reservations",
        "SELECT COUNT(*) FROM ledger_entries WHERE kind = 'usage'",
        "SELECT COUNT(*) FROM request_events WHERE event_kind = 'started'",
        "SELECT SUM(cost_micros) FROM request_records",
    ] {
        let actual: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(statement.to_owned()))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(actual, 0, "{statement}");
    }
    let finished: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_events WHERE event_kind = 'finished'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(finished as usize, request_count);
    pool.close().await;
}
