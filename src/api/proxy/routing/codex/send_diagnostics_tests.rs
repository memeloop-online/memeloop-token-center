use ::http::{HeaderMap, HeaderValue};
use bytes::Bytes;

use super::*;

#[test]
fn native_request_discards_stale_length_and_forbidden_downstream_headers() {
    let client = crate::build_codex_http_client().unwrap();
    let credential = UpstreamCredential::OAuth {
        access_token: "synthetic-access".into(),
        refresh_token: None,
        expires_at: Some(i64::MAX),
        header: "authorization".into(),
        prefix: "Bearer ".into(),
        adapter_state: Some(serde_json::json!({
            "schema": "openai-codex-oauth-v1",
            "account_id": "synthetic-account"
        })),
        proxy_url: None,
        proxy_network_scope: None,
    };
    let serialized = Bytes::from(
        serde_json::to_vec(&serde_json::json!({"input": "合成", "store": false})).unwrap(),
    );
    let mut downstream = HeaderMap::new();
    for (name, value) in [
        ("content-length", "999999"),
        ("connection", "keep-alive, x-private-header"),
        ("keep-alive", "timeout=5"),
        ("proxy-connection", "keep-alive"),
        ("transfer-encoding", "chunked"),
        ("upgrade", "websocket"),
        ("te", "gzip"),
        ("x-private-header", "private-header-value"),
        ("x-codex-turn-metadata", "private-turn-value"),
    ] {
        downstream.insert(name, HeaderValue::from_static(value));
    }
    let request = codex_transport::apply_wreq_wire_headers(
        client
            .post("https://chatgpt.com/backend-api/codex/responses")
            .body(serialized.clone()),
        &downstream,
        &credential,
        "synthetic-session",
        0,
    )
    .unwrap()
    .build()
    .unwrap();
    let before = request.headers().clone();
    let evidence = PreparedRequestEvidence::capture(&request, serialized.len());
    assert_eq!(evidence.known_payload_bytes, serialized.len());
    assert_eq!(evidence.body_size_hint_exact, Some(serialized.len() as u64));
    assert_eq!(evidence.prepared_content_length_count, 0);
    assert_eq!(evidence.prepared_content_length, None);
    assert_eq!(evidence.content_length_state, "not_set_in_built_request");
    assert_eq!(evidence.forbidden_connection_header_count, 0);
    assert!(!evidence.te_present);
    assert!(evidence.te_trailers_only);
    assert!(!request.headers().contains_key("x-private-header"));
    assert_eq!(request.headers(), &before);
    assert!(request.try_clone().is_some());
    assert_eq!(
        request.body().unwrap().size_hint().exact(),
        Some(serialized.len() as u64)
    );
    let rendered = format!("{evidence:?}");
    for secret in [
        "synthetic-access",
        "synthetic-account",
        "synthetic-session",
        "private-header-value",
        "private-turn-value",
        "合成",
    ] {
        assert!(!rendered.contains(secret));
    }
}

#[test]
fn length_and_forbidden_header_observation_never_rewrites_or_rejects_request() {
    let client = crate::build_codex_http_client().unwrap();
    for (declared, expected) in [
        ("3", "explicit_matches_payload"),
        ("4", "explicit_mismatch"),
        ("not-a-length", "explicit_invalid"),
    ] {
        let request = client
            .post("https://example.test/responses")
            .body(Bytes::from_static(b"abc"))
            .header(header::CONTENT_LENGTH, declared)
            .header(header::CONNECTION, "keep-alive")
            .header(header::TE, "gzip")
            .build()
            .unwrap();
        let evidence = PreparedRequestEvidence::capture(&request, 3);
        assert_eq!(evidence.content_length_state, expected);
        assert_eq!(evidence.prepared_content_length_count, 1);
        assert_eq!(evidence.forbidden_connection_header_count, 1);
        assert!(evidence.te_present);
        assert!(!evidence.te_trailers_only);
        assert_eq!(request.headers()[header::CONTENT_LENGTH], declared);
        assert_eq!(request.body().unwrap().size_hint().exact(), Some(3));
        assert!(request.try_clone().is_some());
    }
    let request = client
        .post("https://example.test/responses")
        .body(Bytes::from_static(b"abc"))
        .header(header::CONTENT_LENGTH, "3")
        .header(header::CONTENT_LENGTH, "3")
        .header(header::TE, "trailers, TRAILERS")
        .build()
        .unwrap();
    let evidence = PreparedRequestEvidence::capture(&request, 3);
    assert_eq!(evidence.content_length_state, "explicit_multiple");
    assert_eq!(evidence.prepared_content_length_count, 2);
    assert!(evidence.te_trailers_only);
}
