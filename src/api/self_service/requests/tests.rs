use super::*;
use crate::api::tests::test_state;

fn request_detail_refs(request_id: Uuid) -> crate::model::RequestArchiveRefs {
    crate::model::RequestArchiveRefs {
        view: crate::model::RequestView {
            usage_basis: None,
            compaction: None,
            first_output_ms: None,
            generation_duration_ms: None,
            request_id,
            created_at: 1,
            completed_at: Some(2),
            source_completed_at: None,
            lifecycle_state: crate::model::RequestLifecycleState::Succeeded,
            protocol: "openai".to_owned(),
            model: "request-detail-test".to_owned(),
            upstream_model: None,
            upstream_account_id: Some(Uuid::nil()),
            route_id: Some(Uuid::nil()),
            status_code: Some(200),
            duration_ms: Some(1),
            input_tokens: 1,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 1,
            cost: "0".to_owned(),
            currency: Some("USD".to_owned()),
            usage: crate::model::RequestUsageView {
                tokens: Some(crate::model::RequestTokenUsageView {
                    input_tokens: Some(1),
                    cached_input_tokens: Some(0),
                    cache_write_tokens: Some(0),
                    output_tokens: Some(1),
                }),
                generation: None,
            },
            billing: crate::model::RequestBillingView {
                billable: true,
                cost: Some("0".to_owned()),
                currency: Some("USD".to_owned()),
            },
            error_code: None,
            terminal_cause_code: None,
            supplier_error: None,
            archive_state: crate::model::RequestArchiveState::Bound,
            credential_identity: None,
            session_context: None,
        },
        request_object: "inline-json:{\"prompt\":\"detail body\"}".to_owned(),
        response_object: None,
        response_json: Some(json!({"output": "detail body"})),
        provenance: None,
        request_archive_state: crate::model::RequestArchiveState::Bound,
        request_archive_reason: None,
        response_archive_state: crate::model::RequestArchiveState::Bound,
        response_archive_reason: None,
    }
}

#[tokio::test]
async fn request_detail_response_has_exact_content_length_and_bounded_json_body() {
    let (state, _directory) = test_state().await;
    let response = request_detail_response(&state, request_detail_refs(Uuid::now_v7()))
        .await
        .expect("request detail response");
    let content_length = response.headers()[header::CONTENT_LENGTH]
        .to_str()
        .expect("ASCII Content-Length")
        .parse::<usize>()
        .expect("numeric Content-Length");
    let body = axum::body::to_bytes(response.into_body(), MAX_ARCHIVE_DETAIL_RESPONSE)
        .await
        .expect("bounded request detail body");
    assert_eq!(body.len(), content_length);
    let detail: Value = serde_json::from_slice(&body).expect("request detail JSON body");
    assert_eq!(detail["upstream_account_id"], Uuid::nil().to_string());
    assert_eq!(detail["route_id"], Uuid::nil().to_string());
    assert_eq!(detail["completed_at"], 2);
    assert!(detail.as_object().unwrap().contains_key("supplier_error"));
    assert!(detail["supplier_error"].is_null());
    assert_eq!(detail["currency"], "USD");
    assert_eq!(detail["request_body"]["prompt"], "detail body");
    assert_eq!(detail["response_body"]["output"], "detail body");
}

#[tokio::test]
async fn request_detail_failure_keeps_durable_archive_state_separate() {
    let (state, _directory) = test_state().await;
    let mut refs = request_detail_refs(Uuid::now_v7());
    refs.request_object = format!("inline-json:{}0{}", "[".repeat(65), "]".repeat(65));
    let response = request_detail_response(&state, refs)
        .await
        .expect("bounded request detail response");
    let body = axum::body::to_bytes(response.into_body(), MAX_ARCHIVE_DETAIL_RESPONSE)
        .await
        .expect("bounded request detail body");
    let detail: Value = serde_json::from_slice(&body).expect("request detail JSON body");
    assert_eq!(detail["archive"]["request"]["state"], "bound");
    assert_eq!(detail["archive"]["request"]["complete"], false);
    assert_eq!(
        detail["archive"]["request"]["reason"],
        "archive_payload_invalid"
    );
}

#[tokio::test]
async fn request_archive_content_streams_ranges_with_stable_etag() {
    let (state, _directory) = test_state().await;
    let archived = Bytes::from_static(b"0123456789abcdefghijklmnopqrstuvwxyz");
    let location = state
        .archive
        .put_content(archived.clone())
        .await
        .expect("archive request body");
    let mut refs = request_detail_refs(Uuid::now_v7());
    refs.request_object = location;

    let mut first_headers = HeaderMap::new();
    first_headers.insert(header::RANGE, HeaderValue::from_static("bytes=10-19"));
    let first = request_archive_content_response(
        &state,
        &first_headers,
        &refs,
        RequestArchiveSide::Request,
    )
    .await
    .expect("first archive range");
    assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(first.headers()[header::CONTENT_RANGE], "bytes 10-19/36");
    assert_eq!(first.headers()[header::CONTENT_LENGTH], "10");
    assert_eq!(first.headers()[header::ACCEPT_RANGES], "bytes");
    let etag = first.headers()[header::ETAG].clone();
    let first_body = axum::body::to_bytes(first.into_body(), 10)
        .await
        .expect("first archive bytes");
    assert_eq!(&first_body[..], b"abcdefghij");

    let mut next_headers = HeaderMap::new();
    next_headers.insert(header::RANGE, HeaderValue::from_static("bytes=20-29"));
    next_headers.insert(header::IF_MATCH, etag.clone());
    let next =
        request_archive_content_response(&state, &next_headers, &refs, RequestArchiveSide::Request)
            .await
            .expect("next archive range");
    assert_eq!(next.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(next.headers()[header::ETAG], etag);
    let next_body = axum::body::to_bytes(next.into_body(), 10)
        .await
        .expect("next archive bytes");
    assert_eq!(&next_body[..], b"klmnopqrst");
}

#[tokio::test]
async fn request_archive_content_streams_beyond_detail_snapshot_limit() {
    let (state, _directory) = test_state().await;
    let archived = Bytes::from(vec![b'x'; MAX_ARCHIVE_DETAIL_BODY + 17]);
    let expected_len = archived.len();
    let location = state
        .archive
        .put_content(archived)
        .await
        .expect("archive large request body");
    let mut refs = request_detail_refs(Uuid::now_v7());
    refs.request_object = location;

    let response = request_archive_content_response(
        &state,
        &HeaderMap::new(),
        &refs,
        RequestArchiveSide::Request,
    )
    .await
    .expect("large archive response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_LENGTH]
            .to_str()
            .expect("ASCII content length"),
        expected_len.to_string()
    );
    let body = axum::body::to_bytes(response.into_body(), expected_len)
        .await
        .expect("complete large archive bytes");
    assert_eq!(body.len(), expected_len);
}

#[tokio::test]
async fn request_archive_content_rejects_stale_etag_before_streaming() {
    let (state, _directory) = test_state().await;
    let refs = request_detail_refs(Uuid::now_v7());
    let mut headers = HeaderMap::new();
    headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-3"));
    headers.insert(header::IF_MATCH, HeaderValue::from_static("\"stale\""));
    let response =
        request_archive_content_response(&state, &headers, &refs, RequestArchiveSide::Request)
            .await
            .expect("stale range response");
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
    assert!(response.headers().contains_key(header::ETAG));
}

#[tokio::test]
async fn request_archive_content_does_not_expose_policy_metadata_as_body() {
    let (state, _directory) = test_state().await;
    let mut refs = request_detail_refs(Uuid::now_v7());
    refs.request_object = "metadata-only-json:{\"bytes\":123}".to_owned();
    let response = request_archive_content_response(
        &state,
        &HeaderMap::new(),
        &refs,
        RequestArchiveSide::Request,
    )
    .await
    .expect("metadata-only archive response");
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .expect("metadata-only error body");
    let error: Value = serde_json::from_slice(&body).expect("structured archive error");
    assert_eq!(error["error"]["code"], "archive_content_unavailable");
    assert_eq!(
        error["error"]["reason"],
        "media_body_not_archived_by_policy"
    );
}

#[tokio::test]
async fn invalid_native_supplier_envelopes_cannot_escape_detail_or_archive_content() {
    let (state, _directory) = test_state().await;
    let valid = json!({"error": {
        "type": "upstream_error", "code": "model_unavailable",
        "message": "The requested upstream model is unavailable",
        "mtc_safe_reason": "model_unavailable",
        "mtc_provider_code": "model_not_found",
        "mtc_provider_message": "Model not found"
    }});
    for field in ["mtc_provider_code", "mtc_provider_message", "unknown_field"] {
        let mut tampered = valid.clone();
        tampered["error"][field] = json!("Authorization: Bearer token=private-canary");
        let mut refs = request_detail_refs(Uuid::now_v7());
        refs.response_object = Some(format!("inline-json:{tampered}"));
        refs.view.status_code = Some(400);
        refs.view.error_code = Some("http_400".to_owned());
        refs.view.supplier_error =
            crate::supplier_error::supplier_error_from_inline_json(refs.response_object.as_deref());
        assert!(refs.view.supplier_error.is_none());
        let download = request_archive_content_response(
            &state,
            &HeaderMap::new(),
            &refs,
            RequestArchiveSide::Response,
        )
        .await
        .unwrap();
        assert_eq!(download.status(), StatusCode::CONFLICT);
        let download = axum::body::to_bytes(download.into_body(), MAX_ARCHIVE_DETAIL_RESPONSE)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&download).contains("private-canary"));
        let download: Value = serde_json::from_slice(&download).unwrap();
        assert_eq!(download["error"]["reason"], "archive_payload_invalid");
        let response = request_detail_response(&state, refs).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), MAX_ARCHIVE_DETAIL_RESPONSE)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("private-canary"));
        let detail: Value = serde_json::from_slice(&body).unwrap();
        assert!(detail["response_body"].is_null());
        assert!(detail["supplier_error"].is_null());
        assert_eq!(detail["archive"]["response"]["complete"], false);
        assert_eq!(
            detail["archive"]["response"]["reason"],
            "archive_payload_invalid"
        );
        assert_eq!(detail["status_code"], 400);
        assert_eq!(detail["error_code"], "http_400");
    }
}

#[tokio::test]
async fn native_supplier_and_legacy_failed_archives_retain_their_exact_bodies() {
    let (state, _directory) = test_state().await;
    for raw in [
        r#"{"error":{"type":"upstream_error","code":"model_unavailable","message":"The requested upstream model is unavailable","mtc_safe_reason":"model_unavailable","mtc_provider_code":"model_not_found","mtc_provider_message":"Model not found"}}"#,
        r#"{"error":{"type":"upstream_error","code":"no_active_plan","message":"当前账号没有可用套餐","mtc_safe_reason":"no_active_plan"}}"#,
        r#"{"error":{"type":"upstream_error","message":"upstream rejected the request"}}"#,
        r#"{"error":{"code":"supplier_original_error","message":"original failure body"}}"#,
        "data: {\"error\":{\"message\":\"upstream stream failed\"}}\n\n",
    ] {
        let mut refs = request_detail_refs(Uuid::now_v7());
        refs.response_object = Some(format!("inline-json:{raw}"));
        refs.view.status_code = Some(502);
        refs.view.supplier_error =
            crate::supplier_error::supplier_error_from_inline_json(refs.response_object.as_deref());
        let projected = refs.view.supplier_error.clone();
        let download = request_archive_content_response(
            &state,
            &HeaderMap::new(),
            &refs,
            RequestArchiveSide::Response,
        )
        .await
        .unwrap();
        assert_eq!(download.status(), StatusCode::OK);
        let download = axum::body::to_bytes(download.into_body(), MAX_ARCHIVE_DETAIL_RESPONSE)
            .await
            .unwrap();
        assert_eq!(download.as_ref(), raw.as_bytes());
        let detail = crate::api::request_detail::request_detail(&state, refs).await;
        assert_eq!(detail.view.supplier_error, projected);
        assert!(detail.archive.response.complete);
        let expected =
            serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_owned()));
        assert_eq!(detail.response_body, expected);
    }
}

#[tokio::test]
async fn request_detail_serializes_the_shared_supplier_reason_at_the_top_level() {
    let (state, _directory) = test_state().await;
    let mut refs = request_detail_refs(Uuid::now_v7());
    refs.view.status_code = Some(402);
    refs.view.error_code = Some("http_402".to_owned());
    refs.view.lifecycle_state = crate::model::RequestLifecycleState::Failed;
    refs.view.supplier_error = Some(crate::supplier_error::SupplierError {
        code: "no_active_plan".to_owned(),
        message: "当前账号没有可用套餐".to_owned(),
    });
    let response = request_detail_response(&state, refs).await.unwrap();
    let body = axum::body::to_bytes(response.into_body(), MAX_ARCHIVE_DETAIL_RESPONSE)
        .await
        .unwrap();
    let detail: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        detail["supplier_error"],
        json!({"code": "no_active_plan", "message": "当前账号没有可用套餐"})
    );
    assert_eq!(detail["status_code"], 402);
    assert_eq!(detail["error_code"], "http_402");
    assert!(
        detail.get("view").is_none(),
        "RequestDetail flattens the manual RequestView serializer"
    );
}
