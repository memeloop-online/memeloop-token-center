use super::*;

async fn restored_rejection(fixture: &CodexRouteFixture, request_id: Uuid) -> Bytes {
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(reference) = fixture
                .state
                .archive
                .raw_capture_reference(key.tenant_id, request_id, "response")
                .await
                .unwrap()
            {
                return fixture
                    .state
                    .archive
                    .get_tenant_content(key.tenant_id, &reference, MAX_PROXY_RESPONSE_BODY)
                    .await
                    .unwrap();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("original rejection capture must finish independently of safe delivery")
}

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
async fn supplier_explanation_reaches_client_and_archive_without_logging_request_content() {
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
            let original = json!({
                    "error": {
                        "code": code,
                        "message": "private prompt/full-canary prompt/full private%20prompt%2Ffull-canary pr%69vate+prompt%2ffull-canary private-header/full-canary header/full private-header%2Ffull-canary",
                        "param": "private-header/full-canary"
                    },
                    "debug": "private prompt/full-canary"
                });
            let wire = serde_json::to_vec(&original).unwrap();
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(402).set_body_raw(wire.clone(), "application/json"))
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
            assert_eq!(bytes.as_ref(), wire.as_slice());
            cancellation::assert_single_unknown_receipt(&fixture, 402, Some("http_402")).await;
            let records = fixture.state.db.list_requests(fixture.key_id, 10).await.unwrap();
            let archived = restored_rejection(&fixture, records[0].request_id).await;
            assert_eq!(archived, bytes);
            let refs = fixture.state.db.request_archive_refs(fixture.key_id, records[0].request_id).await.unwrap();
            assert!(!refs.response_object.as_deref().unwrap().starts_with("inline-json:"));
            let detail = crate::api::request_detail::request_detail(&fixture.state, refs).await;
            assert_eq!(detail.response_body, original);
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
            "user-quota",
            403,
            "用户额度不足, 剩余额度: ＄-0.000160 (request id: quota-fixture)",
            "insufficient_user_quota",
        ),
        (
            "user-quota-safe",
            403,
            "用户额度不足",
            "insufficient_user_quota",
        ),
        (
            "expired",
            401,
            "private credential fragment%2Fcanary",
            "api_key_expired",
        ),
        ("rate", 429, "private header-canary", "rate_limit_exceeded"),
        (
            "tpm",
            429,
            "private header-canary",
            "ModelAccountTpmRateLimitExceeded",
        ),
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
                "error": { "message": message, "code": code, "api_key": "must-not-retain" },
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
        cancellation::assert_single_unknown_receipt(
            &fixture,
            i64::from(status),
            Some(&format!("http_{status}")),
        )
        .await;
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
        assert!(record.usage.tokens.is_none());
        assert_eq!(record.billing.cost, None);
        let stored = restored_rejection(&fixture, record.request_id).await;
        let original = json!({
            "error": { "message": message, "code": code, "api_key": "must-not-retain" },
            "usage": { "prompt_tokens": 500, "completion_tokens": 100 }
        });
        assert_eq!(stored.as_ref(), serde_json::to_vec(&original).unwrap());
        assert_eq!(bytes, stored);
        let refs = fixture
            .state
            .db
            .request_archive_refs(fixture.key_id, record.request_id)
            .await
            .unwrap();
        assert!(
            !refs
                .response_object
                .as_deref()
                .unwrap()
                .starts_with("inline-json:")
        );
        let projected = record.supplier_error.as_ref();
        if let Some(reason) =
            crate::supplier_error::SafeReason::from_supplier(Some(code), Some(message))
        {
            assert_eq!(
                projected.map(|summary| summary.code.as_str()),
                Some(reason.code())
            );
        }
        assert!(
            !serde_json::to_string(&projected)
                .unwrap()
                .contains("must-not-retain")
        );
        assert_eq!(refs.view.supplier_error.as_ref(), projected);
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
        assert_eq!(downloaded, stored);
        let detail = crate::api::request_detail::request_detail(&fixture.state, refs).await;
        assert_eq!(detail.view.supplier_error.as_ref(), projected);
        assert_eq!(detail.response_body, original);
        assert!(detail.archive.response.complete);
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
async fn compatible_http_402_preserves_unclassified_details_without_inventing_plan_failure() {
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
            .respond_with(ResponseTemplate::new(402).set_body_raw(body.clone(), content_type))
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
        assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
        let bytes = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), body.as_slice(), "{label}");
        cancellation::assert_single_unknown_receipt(&fixture, 402, Some("http_402")).await;
        let records = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(records[0].error_code.as_deref(), Some("http_402"));
        assert_eq!(records[0].billing.cost, None);
        assert!(records[0].usage.tokens.is_none());
        let refs = fixture
            .state
            .db
            .request_archive_refs(fixture.key_id, records[0].request_id)
            .await
            .unwrap();
        assert!(
            refs.view
                .supplier_error
                .as_ref()
                .is_none_or(|summary| summary.category == "upstream"),
            "{label} must not invent a classified failure"
        );
        assert_eq!(
            restored_rejection(&fixture, records[0].request_id)
                .await
                .as_ref(),
            body
        );
        upstream.verify().await;
    }
}

#[tokio::test]
async fn compatible_http_rejection_keeps_status_when_the_supplier_body_is_truncated() {
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
        Duration::from_secs(8),
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
    let body = response.into_body();
    release.send(()).unwrap();
    server.await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        to_bytes(body, MAX_PROXY_RESPONSE_BODY),
    )
    .await
    .expect("a truncated upstream error body must terminate");
    assert!(
        result.is_err(),
        "a premature EOF must not become a complete response"
    );
    cancellation::assert_single_unknown_receipt(&fixture, 402, Some("http_402")).await;
    let records = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].error_code.as_deref(), Some("http_402"));
    assert!(records[0].usage.tokens.is_none());
    assert_eq!(records[0].billing.cost, None);
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, records[0].request_id)
        .await
        .unwrap();
    assert!(
        refs.view
            .supplier_error
            .as_ref()
            .is_none_or(|summary| summary.category == "upstream")
    );
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fixture
                .state
                .archive
                .raw_capture_failed(key.tenant_id, records[0].request_id, "response")
                .await
                .unwrap()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("incomplete upstream body must leave explicit missing capture evidence");
    assert!(
        fixture
            .state
            .archive
            .raw_capture_reference(key.tenant_id, records[0].request_id, "response")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn supplier_tpm_sse_after_http_200_has_safe_record_and_detail_without_replay_or_success() {
    for (label, suffix) in [("tpm-eof", ""), ("tpm-done", "data: [DONE]\n\n")] {
        let upstream = MockServer::start().await;
        let event = json!({"error": {
            "type": "TooManyRequests",
            "code": "ModelAccountTpmRateLimitExceeded",
            "message": "TPM limit deepseek-v4-1-flash; Authorization: Bearer private-token-canary; private-body-canary",
            "debug": "private-debug-canary"
        }});
        let sse = format!("data: {event}\n\n{suffix}");
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse.clone(), "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;
        let fixture = response_usage_fixture_with_uri(label, upstream.uri(), 0).await;
        let response = send_chat_usage_request(
            &fixture,
            &json!({
                "model": fixture.model,
                "messages": [{"role": "user", "content": "private-body-canary"}],
                "max_tokens": 64, "stream": true
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(header::RETRY_AFTER).is_none());
        let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        let delivered = String::from_utf8(body.to_vec()).unwrap();
        assert!(delivered.contains("rate_limit_exceeded"));
        assert!(delivered.contains("ModelAccountTpmRateLimitExceeded"));
        assert!(delivered.contains("private-token-canary"));
        assert!(delivered.contains("private-body-canary"));
        for forbidden in ["private-debug-canary", "[DONE]", "response.completed"] {
            assert!(!delivered.contains(forbidden));
        }
        cancellation::assert_single_unknown_receipt(&fixture, 502, Some("rate_limited")).await;
        let records = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.status_code, Some(502));
        assert_eq!(record.error_code.as_deref(), Some("rate_limited"));
        assert!(record.usage.tokens.is_none());
        assert_eq!(record.billing.cost, None);
        let supplier = record
            .supplier_error
            .as_ref()
            .expect("safe supplier explanation");
        assert_eq!(supplier.code, "rate_limited");
        assert_eq!(
            supplier.message,
            crate::supplier_error::SafeReason::RateLimited.message()
        );
        let stored = restored_rejection(&fixture, record.request_id).await;
        assert_eq!(stored.as_ref(), sse.as_bytes());
        let refs = fixture
            .state
            .db
            .request_archive_refs(fixture.key_id, record.request_id)
            .await
            .unwrap();
        assert_eq!(refs.view.supplier_error, record.supplier_error);
        let detail = crate::api::request_detail::request_detail(&fixture.state, refs).await;
        assert_eq!(detail.view.supplier_error, record.supplier_error);
        assert_eq!(detail.view.completed_at, None);
        upstream.verify().await;
    }
}

#[test]
fn unknown_supplier_code_or_type_alone_does_not_invent_a_stream_rate_limit() {
    for code in [
        "UnknownTpmCode",
        "ModelAccountTpmRateLimitExceeded-private-token",
    ] {
        let mut capture = ResponsesSseCapture::for_delivery();
        capture.push(
            format!(
                "data: {}\n\n",
                json!({"error": {
                    "type": "TooManyRequests", "code": code, "message": "private-token-canary"
                }})
            )
            .as_bytes(),
        );
        let summary = capture.finish_summary();
        assert_eq!(summary.outcome, ResponsesSseOutcome::Failed);
        assert!(!summary.supplier_rate_limited);
        assert!(
            crate::supplier_error::supplier_error_from_record(
                None,
                Some(502),
                Some("upstream_failed_response")
            )
            .is_none()
        );
    }
    assert!(
        crate::supplier_error::supplier_error_from_record(None, Some(429), Some("http_429"))
            .is_none()
    );
}

#[test]
fn supplier_tpm_is_business_failure_on_strict_chat_usage_contract() {
    let mut capture = ResponsesSseCapture::for_openai_chat_usage();
    capture.push(b"data: {\"error\":{\"type\":\"TooManyRequests\",\"code\":\"ModelAccountTpmRateLimitExceeded\",\"message\":\"private-token-canary\"}}\n\ndata: [DONE]\n\n");
    let summary = capture.finish_summary();
    assert_eq!(summary.outcome, ResponsesSseOutcome::Failed);
    assert!(summary.supplier_rate_limited);
    assert!(!summary.protocol_invalid);
    assert!(!summary.observed_protocol_invalid);
    assert!(summary.usage.is_none());
}
