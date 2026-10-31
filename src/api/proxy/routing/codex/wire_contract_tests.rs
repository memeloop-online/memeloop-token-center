use super::*;
use ::http;
use bytes::Bytes;
use hyper::body::Body as _;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn credential() -> UpstreamCredential {
    UpstreamCredential::OAuth {
        access_token: "synthetic-access".into(),
        refresh_token: None,
        expires_at: Some(i64::MAX),
        header: "authorization".into(),
        prefix: "Bearer ".into(),
        adapter_state: Some(
            json!({"schema":"openai-codex-oauth-v1","account_id":"synthetic-account"}),
        ),
        proxy_url: None,
        proxy_network_scope: None,
    }
}

#[derive(Clone, Default)]
struct CandidateFailureWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CandidateFailureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn local_candidate_failures_log_only_fixed_reasons_and_preserve_disposition() {
    let mut private_credential = credential();
    if let UpstreamCredential::OAuth {
        access_token,
        refresh_token,
        proxy_url,
        ..
    } = &mut private_credential
    {
        *access_token = "ACCESS_CANARY".into();
        *refresh_token = Some("REFRESH_CANARY".into());
        *proxy_url = Some("socks5h://PROXY_CANARY.invalid:1080".into());
    }
    let route = ResolvedUpstream {
        route_id: Uuid::new_v4(),
        account_id: Uuid::new_v4(),
        credential_generation: 13,
        transport_revision: 42,
        driver: codex_transport::DRIVER.into(),
        base_url: "https://HOST_CANARY.invalid/PRIVATE_PATH_CANARY".into(),
        config: json!({"private": "CONFIG_CANARY"}),
        upstream_model: "MODEL_CANARY".into(),
        credential: private_credential,
    };
    let context = CodexAttemptContext {
        request_id: Uuid::new_v4(),
        candidate_rank: 2,
        outbound_attempt: 1,
        transport_policy: runtime_transport_policy(&Value::Null, 1).unwrap(),
        deadline: CodexRequestDeadline {
            request: Some(tokio::time::Instant::now() + std::time::Duration::from_secs(1)),
            read_inactivity: std::time::Duration::from_secs(1),
        },
    };
    let writer = CandidateFailureWriter::default();
    let sink = writer.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(move || sink.clone())
        .finish();
    let reasons = [
        CandidatePreparationFailure::ProxyGroupSelection,
        CandidatePreparationFailure::TransportSnapshot,
    ];
    tracing::subscriber::with_default(subscriber, || {
        for reason in reasons {
            let preserved = reason
                .map_result(Ok::<_, &str>("BODY_CANARY"), &route, context)
                .unwrap();
            assert_eq!(preserved, "BODY_CANARY");
            let error = reason
                .map_result(Err::<(), _>("RAW_ERROR_CANARY"), &route, context)
                .unwrap_err();
            assert!(matches!(error, ProxySendError::CandidateUnavailable));
            assert_eq!(
                super::super::failover_disposition(None, Some(&error)),
                super::super::outcome::FailoverDisposition::CandidateNotDispatched
            );
            assert!(super::super::classify_attempt_failure(&Err(error), None).is_none());
        }
    });
    let captured = writer.0.lock().unwrap();
    let logs = std::str::from_utf8(&captured).unwrap();
    assert!(!logs.contains("CANARY"));
    let events: Vec<Value> = logs
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.len(), reasons.len());
    for (event, reason) in events.iter().zip(reasons) {
        let fields = event["fields"].as_object().unwrap();
        assert_eq!(fields.len(), 10);
        for name in fields.keys() {
            assert!(
                [
                    "message",
                    "request_id",
                    "route_id",
                    "upstream_account_id",
                    "credential_generation",
                    "transport_revision",
                    "candidate_rank",
                    "outbound_attempt",
                    "reason",
                    "stage"
                ]
                .contains(&name.as_str())
            );
        }
        assert_eq!(fields["reason"], reason.label());
        assert_eq!(fields["stage"], "codex_candidate_preparation_failed");
        assert_eq!(fields["request_id"], context.request_id.to_string());
        assert_eq!(fields["route_id"], route.route_id.to_string());
        assert_eq!(fields["upstream_account_id"], route.account_id.to_string());
        assert_eq!(fields["credential_generation"], 13);
        assert_eq!(fields["transport_revision"], 42);
        assert_eq!(fields["candidate_rank"], 2);
        assert_eq!(fields["outbound_attempt"], 1);
    }
}

#[test]
fn native_metadata_ows_is_removed_without_changing_the_payload_or_sensitive_marker() {
    let client = crate::build_codex_http_client().unwrap();
    for input in ["metadata", " metadata", "metadata ", "\tmetadata\t", " \t "] {
        let mut downstream = HeaderMap::new();
        let mut value = http::HeaderValue::from_str(input).unwrap();
        value.set_sensitive(true);
        downstream.insert("x-codex-turn-metadata", value);
        let request = codex_transport::apply_codex_wire_headers(
            client.post("http://wire.example.test/responses").body("{}"),
            &downstream,
            &credential(),
            "synthetic-session",
            0,
        )
        .unwrap()
        .build()
        .unwrap();
        let forwarded = &request.headers()["x-codex-turn-metadata"];
        assert_eq!(forwarded.as_bytes(), input.as_bytes().trim_ascii());
        assert!(forwarded.is_sensitive());
        assert_eq!(request.body().unwrap().size_hint().exact(), Some(2));
    }
}

async fn strict_peer(
    use_proxy: bool,
) -> (
    String,
    String,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let target = if use_proxy {
        "http://wire.example.test/responses".into()
    } else {
        format!("http://{address}/responses")
    };
    let connections = Arc::new(AtomicUsize::new(0));
    let resets = Arc::new(AtomicUsize::new(0));
    let connection_count = connections.clone();
    let reset_count = resets.clone();
    let server = tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            connection_count.fetch_add(1, Ordering::SeqCst);
            let reset_count = reset_count.clone();
            tasks.spawn(async move {
                if use_proxy {
                    let mut greeting = [0; 2];
                    socket.read_exact(&mut greeting).await.unwrap();
                    assert_eq!(greeting[0], 5);
                    let mut methods = vec![0; greeting[1] as usize];
                    socket.read_exact(&mut methods).await.unwrap();
                    socket.write_all(&[5, 0]).await.unwrap();
                    let mut connect = [0; 4];
                    socket.read_exact(&mut connect).await.unwrap();
                    assert_eq!(connect, [5, 1, 0, 3]);
                    let length = socket.read_u8().await.unwrap();
                    let mut host = vec![0; length as usize];
                    socket.read_exact(&mut host).await.unwrap();
                    assert_eq!(host, b"wire.example.test");
                    assert_eq!(socket.read_u16().await.unwrap(), 80);
                    socket
                        .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                        .await
                        .unwrap();
                }
                let mut builder = http2::server::Builder::new();
                builder.initial_window_size(1024);
                let mut connection = builder.handshake::<_, Bytes>(socket).await.unwrap();
                let mut bodies = tokio::task::JoinSet::new();
                while let Some(accepted) = connection.accept().await {
                    let Ok((request, mut response)) = accepted else {
                        break;
                    };
                    assert_eq!(request.method(), http::Method::POST);
                    let invalid = request.headers().values().any(|value| {
                        value
                            .as_bytes()
                            .first()
                            .is_some_and(|b| matches!(b, b' ' | b'\t'))
                            || value
                                .as_bytes()
                                .last()
                                .is_some_and(|b| matches!(b, b' ' | b'\t'))
                    });
                    if invalid {
                        reset_count.fetch_add(1, Ordering::SeqCst);
                        response.send_reset(http2::Reason::PROTOCOL_ERROR);
                        continue;
                    }
                    for name in [
                        "connection",
                        "keep-alive",
                        "proxy-connection",
                        "transfer-encoding",
                        "upgrade",
                        "te",
                    ] {
                        assert!(!request.headers().contains_key(name));
                    }
                    let declared = request.headers()[http::header::CONTENT_LENGTH]
                        .to_str()
                        .unwrap()
                        .parse::<usize>()
                        .unwrap();
                    let session_id = request.headers()["session-id"].clone();
                    bodies.spawn(async move {
                        let mut body = request.into_body();
                        let mut total = 0;
                        let mut received = Vec::new();
                        while let Some(chunk) = body.data().await {
                            let chunk = chunk.unwrap();
                            total += chunk.len();
                            received.extend_from_slice(&chunk);
                            body.flow_control().release_capacity(chunk.len()).unwrap();
                        }
                        assert!(body.is_end_stream());
                        assert_eq!(total, declared);
                        let cache_binding = serde_json::from_slice::<Value>(&received)
                            .ok()
                            .and_then(|body| body.get("prompt_cache_key").cloned());
                        if let Some(binding) = cache_binding.as_ref() {
                            assert_eq!(binding.as_str().unwrap().as_bytes(), session_id.as_bytes());
                        }
                        let headers = http::Response::builder()
                            .status(200)
                            .header("x-wire-body-bytes", total)
                            .header("x-wire-cache-binding", cache_binding.is_some().to_string())
                            .body(())
                            .unwrap();
                        response.send_response(headers, true).unwrap();
                    });
                }
                while let Some(result) = bodies.join_next().await {
                    result.unwrap();
                }
            });
        }
    });
    (
        target,
        format!("socks5h://{address}"),
        connections,
        resets,
        server,
    )
}

#[tokio::test]
async fn prepared_opaque_cache_bindings_survive_direct_and_socks_strict_h2_peer() {
    for use_proxy in [false, true] {
        let (target, proxy, connections, resets, server) = strict_peer(use_proxy).await;
        let client = crate::codex_http_client_builder_with_proxy(
            crate::provider::CodexTransportPolicy::default(),
            crate::CODEX_HTTP2_KEEP_ALIVE_INTERVAL,
            use_proxy.then_some(proxy.as_str()),
        )
        .unwrap()
        .http2_prior_knowledge()
        .build()
        .unwrap();
        let config = json!({
            "base_url": codex_transport::BASE_URL,
            "network_scope": "public",
            "reservation_token_bounds": {"gpt-codex": 10}
        });
        let mut distinct = std::collections::HashSet::new();
        let inputs = [
            " leading",
            "leading",
            "trailing ",
            "trailing",
            " ",
            "  ",
            "inside space",
            "%20leading",
            "%",
            "a%b",
        ];
        for input in inputs.into_iter().map(str::to_owned).chain([
            " ".repeat(256),
            "%".repeat(256),
            "x".repeat(256),
        ]) {
            let mut previous = None;
            for _ in 0..2 {
                let mut body = json!({"input": [], "prompt_cache_key": input});
                let plan =
                    codex_transport::prepare_request(&mut body, "gpt-codex", &config).unwrap();
                if let Some(previous) = previous.as_ref() {
                    assert_eq!(previous, &plan.session_id);
                } else {
                    previous = Some(plan.session_id.clone());
                }
                let serialized = serde_json::to_vec(&body).unwrap();
                let builder = client
                    .post(&target)
                    .version(http::Version::HTTP_2)
                    .body(serialized);
                let request = codex_transport::apply_codex_wire_headers(
                    builder,
                    &HeaderMap::new(),
                    &credential(),
                    &plan.session_id,
                    0,
                )
                .unwrap()
                .build()
                .unwrap();
                let response = client.execute(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()["x-wire-cache-binding"], "true");
            }
            assert!(distinct.insert(previous.unwrap()));
        }
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        assert_eq!(resets.load(Ordering::SeqCst), 0);
        server.abort();
    }
}

#[tokio::test]
async fn strict_h2_peer_reproduces_remote_protocol_error_for_untrimmed_field_values() {
    for use_proxy in [false, true] {
        let (target, proxy, connections, resets, server) = strict_peer(use_proxy).await;
        let client = crate::codex_http_client_builder_with_proxy(
            crate::provider::CodexTransportPolicy::default(),
            crate::CODEX_HTTP2_KEEP_ALIVE_INTERVAL,
            use_proxy.then_some(proxy.as_str()),
        )
        .unwrap()
        .http2_prior_knowledge()
        .build()
        .unwrap();
        let request = client
            .post(target)
            .version(http::Version::HTTP_2)
            .body("{}")
            .header("x-codex-turn-metadata", " metadata ");
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), request.send())
            .await
            .unwrap()
            .unwrap_err();
        let http2 = upstream_response::codex_http2_error(&error).unwrap();
        assert!(http2.is_reset());
        assert!(http2.is_remote());
        assert!(!http2.is_library());
        assert!(!http2.is_go_away());
        assert_eq!(
            http2.reason(),
            Some(u32::from(http2::Reason::PROTOCOL_ERROR))
        );
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        assert_eq!(resets.load(Ordering::SeqCst), 1);
        server.abort();
    }
}

#[tokio::test]
async fn native_h2_headers_and_body_length_survive_reused_multiplexed_connection() {
    for use_proxy in [false, true] {
        let (target, proxy, connections, resets, server) = strict_peer(use_proxy).await;
        let client = crate::codex_http_client_builder_with_proxy(
            crate::provider::CodexTransportPolicy::default(),
            crate::CODEX_HTTP2_KEEP_ALIVE_INTERVAL,
            use_proxy.then_some(proxy.as_str()),
        )
        .unwrap()
        .http2_prior_knowledge()
        .build()
        .unwrap();
        let mut downstream = HeaderMap::new();
        for name in ["x-codex-turn-metadata", "x-client-request-id", "version"] {
            downstream.insert(name, http::HeaderValue::from_static(" \tmetadata\t "));
        }
        downstream.insert("originator", http::HeaderValue::from_static("codex_cli_rs"));
        downstream.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("codex_cli_rs/0.1.0 "),
        );
        downstream.insert(
            http::header::CONTENT_LENGTH,
            http::HeaderValue::from_static("999999"),
        );
        let send = |size: usize, observed: bool| {
            let client = client.clone();
            let target = target.clone();
            let downstream = downstream.clone();
            async move {
                let builder = client
                    .post(target)
                    .version(http::Version::HTTP_2)
                    .body(Bytes::from(vec![b'x'; size]));
                let mut request = codex_transport::apply_codex_wire_headers(
                    builder,
                    &downstream,
                    &credential(),
                    "synthetic-session",
                    0,
                )
                .unwrap()
                .build()
                .unwrap();
                let consumption = send_diagnostics::BodyConsumption::default();
                if observed {
                    consumption.attach(&mut request);
                }
                let response = client.execute(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response.headers()["x-wire-body-bytes"]
                        .to_str()
                        .unwrap()
                        .parse::<usize>()
                        .unwrap(),
                    size
                );
                if observed {
                    assert_eq!(consumption.bytes(), size as u64);
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            send(2, false).await;
            futures_util::future::join_all([
                send(0, false),
                send(16385, false),
                send(131073, false),
                send(262145, true),
            ])
            .await;
        })
        .await
        .unwrap();
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        assert_eq!(resets.load(Ordering::SeqCst), 0);
        server.abort();
    }
}
