use ::http::{HeaderMap, HeaderValue};
use bytes::Bytes;

use super::*;

#[derive(Clone, Default)]
struct Http2EvidenceWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Http2EvidenceWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn http2_attempt_evidence() -> Http2AttemptEvidence {
    let route = ResolvedUpstream {
        route_id: Uuid::new_v4(),
        account_id: Uuid::new_v4(),
        credential_generation: 13,
        transport_revision: 42,
        driver: codex_transport::DRIVER.into(),
        base_url: "https://HOST_CANARY.invalid/PRIVATE_PATH_CANARY".into(),
        config: serde_json::json!({
            (crate::db::transport_proxy_management::CONFIG_KEY): {
                "group_id": Uuid::new_v4().to_string()
            },
            "private": "CONFIG_CANARY"
        }),
        upstream_model: "MODEL_CANARY".into(),
        credential: UpstreamCredential::OAuth {
            access_token: "ACCESS_CANARY".into(),
            refresh_token: Some("REFRESH_CANARY".into()),
            expires_at: Some(i64::MAX),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: Some("socks5h://PROXY_CANARY.invalid:1080".into()),
            proxy_network_scope: None,
        },
    };
    Http2AttemptEvidence::capture(
        &route,
        TransportIdentity {
            client_instance_id: Uuid::new_v4(),
            cache_hit: true,
            selected_proxy_matches_client_key: Some(true),
            connect_attempt: 2,
            proxy_member_index: Some(1),
            proxy_member_count: 3,
            selection_epoch: 21,
            group_selection_version: Some(2),
            request_local_selection: false,
        },
        CodexAttemptContext {
            request_id: Uuid::new_v4(),
            candidate_rank: 2,
            outbound_attempt: 1,
            transport_policy: runtime_transport_policy(&Value::Null, 1).unwrap(),
            deadline: CodexRequestDeadline {
                request: Some(tokio::time::Instant::now() + std::time::Duration::from_secs(5)),
                read_inactivity: std::time::Duration::from_secs(5),
            },
        },
        true,
        BodyConsumption::default(),
    )
}

#[tokio::test]
async fn typed_h2_before_headers_and_body_share_immutable_attempt_without_replay_or_secrets() {
    use futures_util::StreamExt;
    use tracing::instrument::WithSubscriber;

    for before_headers in [true, false] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let accepted = Arc::new(AtomicU64::new(0));
        let request_count = accepted.clone();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut connection = http2::server::handshake(socket).await.unwrap();
            let (request, mut response) = connection.accept().await.unwrap().unwrap();
            assert_eq!(request.method(), ::http::Method::POST);
            request_count.fetch_add(1, Ordering::SeqCst);
            let response_task = tokio::spawn(async move {
                let mut body = request.into_body();
                let mut received = Vec::new();
                while let Some(chunk) = body.data().await {
                    let chunk = chunk.unwrap();
                    received.extend_from_slice(&chunk);
                    body.flow_control().release_capacity(chunk.len()).unwrap();
                }
                assert_eq!(received, b"BODY_CANARY");
                if before_headers {
                    response.send_reset(http2::Reason::PROTOCOL_ERROR);
                } else {
                    let headers = ::http::Response::builder()
                        .header("content-type", "text/event-stream")
                        .header("x-private", "HEADER_CANARY")
                        .body(())
                        .unwrap();
                    let mut stream = response.send_response(headers, false).unwrap();
                    stream
                        .send_data(Bytes::from_static(b"BODY_CANARY"), false)
                        .unwrap();
                    released.await.unwrap();
                    stream.send_reset(http2::Reason::PROTOCOL_ERROR);
                }
            });
            assert!(
                connection.accept().await.is_none(),
                "unexpected replay POST"
            );
            response_task.await.unwrap();
        });
        let evidence = http2_attempt_evidence();
        let expected_request = evidence.request_id;
        let expected_account = evidence.upstream_account_id;
        let expected_client = evidence.identity.client_instance_id;
        let expected_group = evidence.binding_group_id;
        let writer = Http2EvidenceWriter::default();
        let sink = writer.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || sink.clone())
            .finish();
        let client = crate::build_codex_http_client().unwrap();
        let operation = async {
            let mut request = client
                .post(format!("http://{address}/responses"))
                .version(Version::HTTP_2)
                .header("authorization", "Bearer ACCESS_CANARY")
                .body(Bytes::from_static(b"BODY_CANARY"))
                .build()
                .unwrap();
            evidence.body_consumption.attach(&mut request);
            let result = client.execute(request).await;
            if before_headers {
                let error = result.expect_err("peer reset must precede headers");
                evidence.observe_error(&error, true);
                assert!(matches!(
                    classify_codex_send_error(error),
                    ProxySendError::NonRetryableTransport(TransportFailureKind::Http2Reset)
                ));
                drop(release);
            } else {
                let response = result.unwrap();
                let expected_headers = response.headers().clone();
                let expected_length = response.content_length();
                let parts = evidence.observe_response(response).into_parts();
                assert_eq!(parts.status, ::http::StatusCode::OK);
                assert_eq!(parts.version, Version::HTTP_2);
                assert_eq!(parts.headers, expected_headers);
                assert_eq!(parts.content_length, expected_length);
                let mut stream = parts.stream;
                assert_eq!(stream.next().await.unwrap().unwrap(), b"BODY_CANARY"[..]);
                release.send(()).unwrap();
                assert_eq!(
                    stream.next().await.unwrap(),
                    Err(upstream_response::UPSTREAM_HTTP2_RESET)
                );
                drop(stream);
            }
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            operation.with_subscriber(subscriber),
        )
        .await
        .unwrap();
        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
        let captured = writer.0.lock().unwrap();
        let logs = std::str::from_utf8(&captured).unwrap();
        for secret in [
            "BODY_CANARY",
            "ACCESS_CANARY",
            "REFRESH_CANARY",
            "HEADER_CANARY",
            "PROXY_CANARY",
            "CONFIG_CANARY",
            "MODEL_CANARY",
            "HOST_CANARY",
            "PRIVATE_PATH_CANARY",
        ] {
            assert!(!logs.contains(secret));
        }
        let events: Vec<Value> = logs
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            events
                .iter()
                .all(|event| event["fields"]["message"] != "Optional H2 attempt binding")
        );
        let fields: Vec<_> = events
            .iter()
            .map(|event| &event["fields"])
            .filter(|fields| {
                fields["stage"]
                    == if before_headers {
                        "codex_http2_before_response_headers"
                    } else {
                        "codex_http2_response_body"
                    }
            })
            .collect();
        assert_eq!(fields.len(), 1);
        let fields = fields[0];
        assert_eq!(fields["request_id"], expected_request.to_string());
        assert_eq!(fields["upstream_account_id"], expected_account.to_string());
        assert_eq!(fields["credential_generation"], 13);
        assert_eq!(fields["transport_revision"], 42);
        assert_eq!(fields["candidate_rank"], 2);
        assert_eq!(fields["outbound_attempt"], 1);
        assert_eq!(fields["connect_attempt"], 2);
        assert_eq!(fields["client_instance_id"], expected_client.to_string());
        assert_eq!(fields["client_cache_hit"], true);
        assert_eq!(fields["proxy_member_index"], "Some(1)");
        assert_eq!(fields["proxy_member_count"], 3);
        assert_eq!(fields["selection_epoch"], 21);
        assert_eq!(fields["group_selection_version"], "Some(2)");
        assert_eq!(fields["binding_group_id"], format!("{expected_group:?}"));
        assert_eq!(fields["http2_remote"], true);
        assert_eq!(fields["http2_library"], false);
        assert_eq!(fields["http2_reset"], true);
        assert_eq!(fields["http2_goaway"], false);
        assert_eq!(fields["http2_reason"], "Some(1)");
        assert_eq!(fields["wire_delivery_state"], "unknown");
        assert_eq!(fields["request_body_consumed_bytes"], 11);
        assert_eq!(
            fields["body_consumption_scope"],
            "transport_body_poll_not_wire_commit"
        );
        assert_eq!(
            fields["client_identity_scope"],
            "cached_client_not_connection_or_stream"
        );
    }
}

#[tokio::test]
async fn generic_errors_do_not_create_h2_origin_or_log_raw_error() {
    let evidence = http2_attempt_evidence();
    let writer = Http2EvidenceWriter::default();
    let sink = writer.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || sink.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let error = std::io::Error::other("RAW_ERROR_CANARY");
        evidence.observe_error(&error, true);
        evidence.observe_error(&error, false);
    });
    assert!(writer.0.lock().unwrap().is_empty());
}

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
        crate::http2_upload_tests::native_encoded::verify(payload, move |client, url| {
            let builder = codex_transport::apply_codex_wire_headers(
                client.post(url).body(sent),
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
    crate::http2_upload_tests::native_encoded::emit(evidence);
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
    let request = codex_transport::apply_codex_wire_headers(
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
