use ::http::{HeaderMap, HeaderValue};
use bytes::Bytes;

use super::*;

#[tokio::test]
async fn native_request_991089_tls_encoded_contract() {
    const LENGTH: usize = 991_089;
    let mut value = serde_json::json!({
        "model": "synthetic-model", "input": "", "store": false,
        "prompt_cache_key": "synthetic-session"
    });
    let fixed = serde_json::to_vec(&value).unwrap().len();
    value["input"] = serde_json::Value::String("x".repeat(LENGTH - fixed));
    let payload = Bytes::from(serde_json::to_vec(&value).unwrap());
    assert_eq!(payload.len(), LENGTH);
    let credential = UpstreamCredential::OAuth {
        access_token: "synthetic-access".into(),
        refresh_token: None,
        expires_at: Some(i64::MAX),
        header: "authorization".into(),
        prefix: "Bearer ".into(),
        adapter_state: Some(
            serde_json::json!({"schema": "openai-codex-oauth-v1", "account_id": "synthetic-account"}),
        ),
        proxy_url: None,
        proxy_network_scope: None,
    };
    let mut downstream = HeaderMap::new();
    for (name, value) in [
        ("version", "synthetic-version"),
        ("x-codex-beta-features", "synthetic-beta"),
        ("x-codex-turn-metadata", "{\"synthetic\":true}"),
        ("x-client-request-id", "synthetic-request"),
        ("x-codex-window-id", "synthetic-window"),
        ("thread-id", "synthetic-thread"),
        ("originator", "codex_cli_rs"),
        ("user-agent", "codex_cli_rs/0.0.0 synthetic"),
    ] {
        downstream.insert(name, HeaderValue::from_static(value));
    }
    let consumption = BodyConsumption::default();
    let observed = consumption.clone();
    let sent = payload.clone();
    let mut evidence =
        crate::http2_upload_tests::native_encoded::verify(payload, move |client, url, proxy| {
            let builder = codex_transport::apply_wreq_wire_headers(
                client.post(url).proxy(proxy).body(sent),
                &downstream,
                &credential,
                "synthetic-session",
                0,
            )
            .unwrap();
            let (_, built) = builder.build_split();
            let mut request = built.unwrap();
            for (name, value) in &downstream {
                assert!(
                    request.headers().get(name) == Some(value),
                    "native adapter omitted or changed a synthetic input header"
                );
            }
            for (name, value) in [
                ("authorization", "Bearer synthetic-access"),
                ("accept", "text/event-stream"),
                ("accept-encoding", "identity"),
                ("content-type", "application/json"),
                ("session-id", "synthetic-session"),
                ("chatgpt-account-id", "synthetic-account"),
            ] {
                assert!(
                    request
                        .headers()
                        .get(name)
                        .is_some_and(|actual| actual == value),
                    "native adapter omitted or changed a synthetic wire header"
                );
            }
            assert_eq!(request.headers().len(), 14);
            assert!(!request.headers().contains_key(header::CONTENT_LENGTH));
            assert_eq!(
                request.body().unwrap().size_hint().exact(),
                Some(LENGTH as u64)
            );
            observed.attach(&mut request);
            request
        })
        .await;
    assert_eq!(consumption.bytes(), LENGTH as u64);
    assert!(consumption.polls() > 0);
    evidence["body_polled_bytes"] = serde_json::json!(consumption.bytes());
    evidence["body_polls"] = serde_json::json!(consumption.polls());
    evidence["body_polling_is_wire_ack"] = serde_json::json!(false);
    eprintln!("MTC_HTTP2_UPLOAD_EVIDENCE {evidence}");
}

#[tokio::test]
async fn body_observation_preserves_frames_length_and_end_stream_without_claiming_delivery() {
    for payload in [Bytes::new(), Bytes::from_static(b"synthetic-body")] {
        let consumption = BodyConsumption::default();
        let mut request = crate::build_codex_http_client()
            .unwrap()
            .post("https://example.test/responses")
            .body(payload.clone())
            .build()
            .unwrap();
        let headers = request.headers().clone();
        let original_end = request.body().unwrap().is_end_stream();
        consumption.attach(&mut request);
        assert_eq!(request.headers(), &headers);
        assert_eq!(
            request.body().unwrap().size_hint().exact(),
            Some(payload.len() as u64)
        );
        assert_eq!(request.body().unwrap().is_end_stream(), original_end);
        assert_eq!(consumption.polls(), 0);
        assert_eq!(consumption.bytes(), 0);
        let mut body = request.body_mut().take().unwrap();
        let mut received = Vec::new();
        while let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await
        {
            let frame = frame.unwrap();
            if let Some(data) = frame.data_ref() {
                received.extend_from_slice(data);
            }
        }
        assert_eq!(received, payload.as_ref());
        assert!(body.is_end_stream());
        assert!(consumption.polls() > 0);
        assert_eq!(consumption.bytes(), payload.len() as u64);
    }
}

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
    assert_eq!(
        evidence.request_protocol_policy,
        "client_default_negotiation"
    );
    assert_eq!(evidence.forbidden_connection_header_count, 0);
    assert_eq!(evidence.forbidden_connection_header_mask, 0);
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
        assert_eq!(evidence.forbidden_connection_header_mask, 1);
        assert!(evidence.te_present);
        assert!(!evidence.te_trailers_only);
        assert_eq!(request.headers()[header::CONTENT_LENGTH], declared);
        assert_eq!(request.body().unwrap().size_hint().exact(), Some(3));
        assert!(request.try_clone().is_some());
    }
    let request = client
        .post("https://example.test/responses")
        .version(Version::HTTP_2)
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
    assert_eq!(evidence.request_protocol_policy, "explicit_http_2");
}
