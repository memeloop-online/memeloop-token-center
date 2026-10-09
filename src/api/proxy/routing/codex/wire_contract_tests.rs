use super::*;
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

#[test]
fn native_metadata_ows_is_removed_without_changing_the_payload_or_sensitive_marker() {
    let client = crate::build_codex_http_client().unwrap();
    for input in ["metadata", " metadata", "metadata ", "\tmetadata\t", " \t "] {
        let mut downstream = HeaderMap::new();
        let mut value = http::HeaderValue::from_str(input).unwrap();
        value.set_sensitive(true);
        downstream.insert("x-codex-turn-metadata", value);
        let request = codex_transport::apply_wreq_wire_headers(
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
                    bodies.spawn(async move {
                        let mut body = request.into_body();
                        let mut total = 0;
                        while let Some(chunk) = body.data().await {
                            let chunk = chunk.unwrap();
                            total += chunk.len();
                            body.flow_control().release_capacity(chunk.len()).unwrap();
                        }
                        assert!(body.is_end_stream());
                        assert_eq!(total, declared);
                        let headers = http::Response::builder()
                            .status(200)
                            .header("x-wire-body-bytes", total)
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
async fn strict_h2_peer_reproduces_remote_protocol_error_for_untrimmed_field_values() {
    for use_proxy in [false, true] {
        let (target, proxy, connections, resets, server) = strict_peer(use_proxy).await;
        let client = crate::build_codex_http_client().unwrap();
        let mut request = client
            .post(target)
            .version(http::Version::HTTP_2)
            .body("{}")
            .header("x-codex-turn-metadata", " metadata ");
        if use_proxy {
            request = request.proxy(wreq::Proxy::all(proxy).unwrap());
        }
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), request.send())
            .await
            .unwrap()
            .unwrap_err();
        let http2 = upstream_response::codex_http2_error(&error).unwrap();
        assert!(http2.is_reset());
        assert!(http2.is_remote());
        assert!(!http2.is_library());
        assert!(!http2.is_go_away());
        assert_eq!(http2.reason(), Some(http2::Reason::PROTOCOL_ERROR));
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        assert_eq!(resets.load(Ordering::SeqCst), 1);
        server.abort();
    }
}

#[tokio::test]
async fn native_h2_headers_and_body_length_survive_reused_multiplexed_connection() {
    for use_proxy in [false, true] {
        let (target, proxy, connections, resets, server) = strict_peer(use_proxy).await;
        let client = crate::build_codex_http_client().unwrap();
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
            let proxy = proxy.clone();
            let downstream = downstream.clone();
            async move {
                let mut builder = client
                    .post(target)
                    .version(http::Version::HTTP_2)
                    .body(Bytes::from(vec![b'x'; size]));
                if use_proxy {
                    builder = builder.proxy(wreq::Proxy::all(proxy).unwrap());
                }
                let mut request = codex_transport::apply_wreq_wire_headers(
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
