use super::*;

#[derive(Clone, Default)]
struct SupplierLogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

struct SupplierLogWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for SupplierLogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SupplierLogCapture {
    type Writer = SupplierLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SupplierLogWriter(self.0.clone())
    }
}

#[tokio::test]
async fn supplier_header_and_prompt_echoes_never_reach_response_archives_or_logs() {
    use tracing::instrument::WithSubscriber;

    let capture = SupplierLogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_span_events(
            tracing_subscriber::fmt::format::FmtSpan::NEW
                | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
        )
        .with_writer(capture.clone())
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);
    async {
        for (label, code) in [("safe-echo", "NoAvailablePlan"), ("unknown-echo", "private-header-fragment")] {
            let upstream = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(402).set_body_json(json!({
                    "error": {
                        "code": code,
                        "message": "private prompt/full-canary prompt/full private%20prompt%2Ffull-canary pr%69vate+prompt%2ffull-canary private-header/full-canary header/full private-header%2Ffull-canary",
                        "param": "private-header/full-canary"
                    },
                    "debug": "private prompt/full-canary"
                })))
                .expect(1).mount(&upstream).await;
            let fixture = response_usage_fixture_with_uri(label, upstream.uri(), 0).await;
            let request_body = json!({
                "model": fixture.model,
                "messages": [{"role": "user", "content": "private prompt/full-canary"}],
                "max_tokens": 64, "stream": false
            });
            let response = router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
                .oneshot(Request::post("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
                    .header("x-private-context", "private-header/full-canary")
                    .body(Body::from(serde_json::to_vec(&request_body).unwrap())).unwrap())
                .await.unwrap();
            assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
            let bytes = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY).await.unwrap();
            for canary in ["private prompt/full-canary", "prompt/full", "private%20prompt%2Ffull-canary",
                "pr%69vate+prompt%2ffull-canary", "private-header/full-canary", "header/full",
                "private-header%2Ffull-canary", "private-header-fragment"] {
                assert!(!String::from_utf8_lossy(&bytes).contains(canary));
            }
            if code == "NoAvailablePlan" {
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(value["error"]["code"], "no_active_plan");
                assert_eq!(value["error"]["message"], "当前账号没有可用套餐");
            } else {
                assert_eq!(bytes, upstream_error::fallback_body());
            }
            wait_for_request_settlement(&fixture, 1).await;
            let records = fixture.state.db.list_requests(fixture.key_id, 10).await.unwrap();
            let refs = fixture.state.db.request_archive_refs(fixture.key_id, records[0].request_id).await.unwrap();
            let stored = refs.response_object.as_deref().unwrap().strip_prefix("inline-json:").unwrap();
            assert_eq!(stored.as_bytes(), bytes.as_ref());
            let detail = crate::api::request_detail::request_detail(&fixture.state, refs).await;
            assert_eq!(detail.response_body, serde_json::from_slice::<Value>(&bytes).unwrap());
            upstream.verify().await;
        }
    }.with_subscriber(dispatch).await;
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    for canary in [
        "private prompt/full-canary",
        "prompt/full",
        "private%20prompt%2Ffull-canary",
        "pr%69vate+prompt%2ffull-canary",
        "private-header/full-canary",
        "header/full",
        "private-header%2Ffull-canary",
        "private-header-fragment",
    ] {
        assert!(!logs.contains(canary));
    }
}

#[tokio::test]
async fn compatible_http_errors_classify_safe_reasons_and_preserve_financial_attribution() {
    for (label, status, message, code) in [
        ("plan", 402, "当前账号没有可用套餐", "NoAvailablePlan"),
        (
            "auth",
            401,
            "Invalid credential compatibility-upstream-secret; token=other-canary",
            "invalid_api_key",
        ),
        (
            "quota",
            429,
            "Quota exhausted for this account",
            "insufficient_quota",
        ),
        (
            "expired",
            401,
            "private credential fragment%2Fcanary",
            "api_key_expired",
        ),
        ("rate", 429, "private header-canary", "rate_limit_exceeded"),
        ("model", 400, "private model-canary", "unsupported_model"),
        ("model-missing", 400, "Model not found", "model_not_found"),
        (
            "model-unsupported",
            400,
            "Unsupported model",
            "unsupported_model",
        ),
        (
            "unknown",
            500,
            "Supplier encountered an unspecified processing error",
            "processing_error",
        ),
    ] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                "error": { "message": message, "code": code, "debug": "must-not-retain" },
                "usage": { "prompt_tokens": 500, "completion_tokens": 100 }
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        let fixture = response_usage_fixture_with_uri(label, upstream.uri(), 0).await;
        let response = send_chat_usage_request(
            &fixture,
            &json!({
                "model": fixture.model,
                "messages": [{ "role": "user", "content": "submitted-private-canary" }],
                "max_tokens": 64,
                "stream": false
            }),
        )
        .await;
        assert_eq!(response.status().as_u16(), status);
        let bytes = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        let delivered: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(delivered["error"]["type"], "upstream_error");
        if let Some(reason) =
            crate::supplier_error::SafeReason::from_supplier(Some(code), Some(message))
        {
            assert_eq!(delivered["error"]["code"], reason.code());
            assert_eq!(delivered["error"]["message"], reason.message());
            assert_eq!(delivered["error"]["mtc_safe_reason"], reason.code());
            let (safe_code, safe_message) = reason.provider_detail(Some(code), Some(message));
            assert_eq!(delivered["error"]["mtc_provider_code"].as_str(), safe_code);
            assert_eq!(
                delivered["error"]["mtc_provider_message"].as_str(),
                safe_message
            );
        } else {
            assert_eq!(bytes, upstream_error::fallback_body());
        }
        for canary in [
            "compatibility-upstream-secret",
            "other-canary",
            "submitted-private-canary",
            "must-not-retain",
            "fragment%2Fcanary",
            "header-canary",
            "model-canary",
        ] {
            assert!(!String::from_utf8_lossy(&bytes).contains(canary));
        }
        assert!(delivered.get("usage").is_none());
        wait_for_request_settlement(&fixture, 1).await;
        let records = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.status_code, Some(i64::from(status)));
        assert_eq!(
            record.error_code.as_deref(),
            Some(format!("http_{status}").as_str())
        );
        assert_eq!(
            record.upstream_account_id,
            Some(fixture.upstream_account_id)
        );
        assert_eq!(record.route_id, Some(fixture.route_id));
        assert_eq!(
            record.usage_basis,
            Some(crate::model::RequestUsageBasis::NotObserved)
        );
        assert_eq!(record.input_tokens, 0);
        assert_eq!(record.output_tokens, 0);
        assert_eq!(record.cost, "0");
        let refs = fixture
            .state
            .db
            .request_archive_refs(fixture.key_id, record.request_id)
            .await
            .unwrap();
        let stored = refs
            .response_object
            .as_deref()
            .unwrap()
            .strip_prefix("inline-json:")
            .unwrap();
        assert_eq!(stored.as_bytes(), bytes.as_ref());
        let projected =
            crate::supplier_error::supplier_error_from_inline_json(refs.response_object.as_deref());
        assert_eq!(record.supplier_error, projected);
        assert_eq!(refs.view.supplier_error, projected);
        let download = crate::api::request_detail::request_archive_content_response(
            &fixture.state,
            &http::HeaderMap::new(),
            &refs,
            crate::api::request_detail::RequestArchiveSide::Response,
        )
        .await
        .unwrap();
        assert_eq!(download.status().as_u16(), 200);
        let downloaded = to_bytes(download.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        assert_eq!(downloaded, bytes);
        let detail = crate::api::request_detail::request_detail(&fixture.state, refs).await;
        assert_eq!(detail.view.supplier_error, projected);
        assert_eq!(detail.response_body, delivered);
        assert!(detail.archive.response.complete);
        assert_exactly_once_side_effects(&fixture, record.request_id, None).await;
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        let spools: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM response_archive_spools WHERE request_id = $1",
        )
        .bind(record.request_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(spools, 0);
        pool.close().await;
        upstream.verify().await;
    }
}

#[tokio::test]
async fn compatible_http_402_without_safe_structured_detail_does_not_invent_a_plan_failure() {
    for (label, content_type, body) in [
        ("empty", "application/json", Vec::new()),
        (
            "unknown-shape",
            "application/json",
            br#"{"unexpected":"private-canary"}"#.to_vec(),
        ),
        (
            "malformed",
            "application/json",
            br#"{"error":{"message":"private-canary"}"#.to_vec(),
        ),
        (
            "duplicate",
            "application/json",
            br#"{"error":{"code":"NoAvailablePlan","code":"private-canary"}}"#.to_vec(),
        ),
        (
            "unknown-code",
            "application/json",
            br#"{"error":{"message":"Unknown account rejection","code":402}}"#.to_vec(),
        ),
        (
            "partial",
            "application/json",
            br#"{"error":{"message":"fallback request private-canary","code":"request-fragment"}}"#
                .to_vec(),
        ),
        (
            "encoded",
            "application/json",
            br#"{"error":{"message":"fallback%20request","code":"private%2Fcanary"}}"#.to_vec(),
        ),
        ("html", "text/html", b"<html>private-canary</html>".to_vec()),
        ("plaintext", "text/plain", b"private-canary".to_vec()),
        (
            "sse",
            "text/event-stream",
            b"data: {\"error\":{\"message\":\"private-canary\"}}\n\n".to_vec(),
        ),
        ("huge", "application/json", vec![b'x'; 128 * 1024]),
    ] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(402).set_body_raw(body, content_type))
            .expect(1)
            .mount(&upstream)
            .await;
        let fixture = response_usage_fixture_with_uri(label, upstream.uri(), 0).await;
        let response = send_chat_usage_request(
            &fixture,
            &json!({
                "model": fixture.model,
                "messages": [{"role": "user", "content": "fallback request"}],
                "max_tokens": 64, "stream": false
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        let bytes = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        assert_eq!(bytes, upstream_error::fallback_body());
        let records = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(records[0].error_code.as_deref(), Some("http_402"));
        assert_eq!(records[0].cost, "0");
        let refs = fixture
            .state
            .db
            .request_archive_refs(fixture.key_id, records[0].request_id)
            .await
            .unwrap();
        assert_eq!(
            refs.response_object
                .as_deref()
                .unwrap()
                .strip_prefix("inline-json:")
                .unwrap()
                .as_bytes(),
            bytes.as_ref()
        );
        upstream.verify().await;
    }
}

#[tokio::test]
async fn compatible_http_stalled_json_rejects_before_supplier_body_or_release() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (release, released) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut connection, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        let mut buffer = [0; 1024];
        while !received.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = connection.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            received.extend_from_slice(&buffer[..count]);
            assert!(received.len() <= 16 * 1024);
        }
        connection.write_all(b"HTTP/1.1 402 Payment Required\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n").await.unwrap();
        released.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
        drop(connection);
    });
    let fixture = response_usage_fixture_with_uri("stalled-error-detail", endpoint, 0).await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        send_chat_usage_request(
            &fixture,
            &json!({
                "model": fixture.model,
                "messages": [{"role": "user", "content": "stalled body request"}],
                "max_tokens": 64, "stream": false
            }),
        ),
    )
    .await
    .expect("a stalled error body must not wait for the transport timeout or supplier release");
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    let bytes = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert_eq!(bytes, upstream_error::fallback_body());
    release.send(()).unwrap();
    server.await.unwrap();
    let records = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].error_code.as_deref(), Some("http_402"));
    assert_eq!(
        records[0].usage_basis,
        Some(crate::model::RequestUsageBasis::NotObserved)
    );
    assert_eq!(records[0].cost, "0");
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, records[0].request_id)
        .await
        .unwrap();
    assert_eq!(
        refs.response_object
            .as_deref()
            .unwrap()
            .strip_prefix("inline-json:")
            .unwrap()
            .as_bytes(),
        bytes.as_ref()
    );
}
