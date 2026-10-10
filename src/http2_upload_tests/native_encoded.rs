//! A synthetic peer uses upstream Hyper/h2 rather than the client's http2 fork.
//! All payload/header comparisons stay in RAM; evidence contains only counters.

use super::*;
use std::{
    convert::Infallible,
    sync::atomic::{AtomicUsize, Ordering},
};

use hyper::{body::Body, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};

struct AbortPeer(tokio::task::AbortHandle);

impl Drop for AbortPeer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) fn emit(evidence: serde_json::Value) {
    super::emit_evidence(evidence);
}

fn assert_decoded_headers(
    request: &hyper::Request<hyper::body::Incoming>,
    expected: &http::HeaderMap,
    length: usize,
) -> Option<usize> {
    assert_eq!(request.method(), http::Method::POST);
    assert_eq!(request.version(), http::Version::HTTP_2);
    assert_eq!(request.uri().scheme_str(), Some("https"));
    assert_eq!(request.uri().authority().unwrap().as_str(), HOST);
    assert_eq!(request.uri().path(), "/v1/responses");
    for (name, value) in expected {
        assert!(
            request.headers().get(name) == Some(value),
            "native header changed at peer"
        );
        assert_eq!(
            request.headers().get_all(name).iter().count(),
            1,
            "duplicate native header"
        );
    }
    for (name, value) in request.headers() {
        assert!(name.as_str().bytes().all(|byte| !byte.is_ascii_uppercase()));
        let bytes = value.as_bytes();
        assert!(
            !bytes.iter().any(|byte| matches!(byte, 0 | b'\r' | b'\n')),
            "invalid decoded field value"
        );
        assert!(
            !bytes
                .first()
                .is_some_and(|byte| matches!(byte, b' ' | b'\t')),
            "leading field whitespace"
        );
        assert!(
            !bytes
                .last()
                .is_some_and(|byte| matches!(byte, b' ' | b'\t')),
            "trailing field whitespace"
        );
        assert!(
            expected.contains_key(name) || name == http::header::CONTENT_LENGTH,
            "unexpected decoded header"
        );
    }
    for name in [
        "host",
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "upgrade",
    ] {
        assert!(
            !request.headers().contains_key(name),
            "forbidden HTTP/2 header"
        );
    }
    assert!(
        request
            .headers()
            .get_all(http::header::TE)
            .iter()
            .all(|value| value == "trailers")
    );
    let lengths: Vec<_> = request
        .headers()
        .get_all(http::header::CONTENT_LENGTH)
        .iter()
        .collect();
    assert!(lengths.len() <= 1, "duplicate Content-Length");
    lengths.first().map(|value| {
        let declared = value
            .to_str()
            .expect("ASCII Content-Length")
            .parse::<usize>()
            .expect("numeric Content-Length");
        assert_eq!(
            declared, length,
            "decoded Content-Length differs from expected bytes"
        );
        declared
    })
}

fn assert_encoded_upload(wire: &Wire, length: usize) -> serde_json::Value {
    let mut headers = 0;
    let mut data_bytes = 0;
    let mut data_frames = 0;
    let mut eos = 0;
    let mut continuation = [None, None];
    let mut connection_window = WINDOW;
    let mut stream_window = WINDOW;
    let mut window_updates = 0;
    let mut responses = 0;
    for frame in &wire.frames {
        let side = usize::from(frame.from_client);
        if let Some(stream) = continuation[side] {
            assert_eq!(frame.kind, 9, "interleaved unfinished header block");
            assert_eq!(frame.stream_id, stream);
        } else {
            assert_ne!(frame.kind, 9, "orphan CONTINUATION");
        }
        match frame.kind {
            0 if frame.from_client => {
                assert_eq!(headers, 1, "DATA before request HEADERS");
                assert_eq!(frame.stream_id, 1);
                assert_eq!(frame.flags & !1, 0, "unexpected DATA flags/padding");
                assert_eq!(eos, 0, "DATA after request END_STREAM");
                connection_window -= frame.length as i64;
                stream_window -= frame.length as i64;
                assert!(connection_window >= 0, "connection upload credit exceeded");
                assert!(stream_window >= 0, "stream upload credit exceeded");
                data_frames += 1;
                data_bytes += frame.length;
                eos += usize::from(frame.flags & 1 != 0);
            }
            1 => {
                assert_eq!(frame.stream_id, 1, "extra request or response stream");
                if frame.from_client {
                    headers += 1;
                    assert_eq!(headers, 1, "extra request/trailer HEADERS");
                    assert_eq!(frame.flags & 1, 0, "nonempty request ended in HEADERS");
                } else {
                    responses += 1;
                    assert_eq!(eos, 1, "response before complete request stream");
                    assert_eq!(data_bytes, length);
                }
                if frame.flags & 4 == 0 {
                    continuation[side] = Some(frame.stream_id);
                }
            }
            9 => {
                if frame.flags & 4 != 0 {
                    continuation[side] = None;
                }
            }
            8 if !frame.from_client => {
                assert_eq!(frame.length, 4);
                let increment =
                    u32::from_be_bytes(frame.control[..].try_into().unwrap()) & 0x7fff_ffff;
                assert!(increment > 0);
                if frame.stream_id == 0 {
                    connection_window += i64::from(increment);
                } else {
                    assert_eq!(frame.stream_id, 1);
                    stream_window += i64::from(increment);
                }
                assert!(connection_window <= 0x7fff_ffff && stream_window <= 0x7fff_ffff);
                window_updates += 1;
            }
            3 => panic!("unexpected synthetic RST_STREAM"),
            7 => {
                assert!(frame.control.len() >= 8);
                assert_eq!(
                    u32::from_be_bytes(frame.control[4..8].try_into().unwrap()),
                    0,
                    "non-graceful synthetic GOAWAY"
                );
            }
            _ => {}
        }
    }
    assert_eq!(continuation, [None, None]);
    assert_eq!(headers, 1);
    assert_eq!(responses, 1);
    assert_eq!(data_bytes, length);
    assert_eq!(eos, 1);
    assert!(data_frames >= length.div_ceil(16_384));
    assert!(
        window_updates > 0,
        "large upload did not exercise credit updates"
    );
    serde_json::json!({"request_headers": headers, "data_bytes": data_bytes, "data_frames": data_frames, "request_end_streams": eos, "upload_window_updates": window_updates})
}

pub(crate) async fn verify<F>(expected: Bytes, build_request: F) -> serde_json::Value
where
    F: FnOnce(&wreq::Client, &str, wreq::Proxy) -> wreq::Request,
{
    timeout(Duration::from_secs(10), async {
        let fixture = tls_fixture::TlsFixture::new().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = wreq::Proxy::all(format!("socks5h://{}", listener.local_addr().unwrap())).unwrap();
        let client = crate::codex_http_client_builder(CodexTransportPolicy::default(), crate::CODEX_HTTP2_KEEP_ALIVE_INTERVAL)
            .tls_cert_store(fixture.roots).build().unwrap();
        let request = build_request(&client, &format!("https://{HOST}/v1/responses"), proxy);
        assert_eq!(request.version(), None, "preserve default negotiation");
        let expected_headers = request.headers().clone();
        let length = expected.len();
        let wire = Arc::new(Mutex::new(Wire { started: Instant::now(), frames: Vec::new() }));
        let observed = wire.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let received = requests.clone();
        let content_length = Arc::new(Mutex::new(None));
        let decoded_length = content_length.clone();
        let (shutdown, stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            accept_socks_at_port(&mut socket, 443).await;
            let encrypted = fixture.acceptor.accept(socket).await.unwrap();
            assert_eq!(encrypted.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
            assert_eq!(encrypted.get_ref().1.server_name(), Some(HOST));
            let service = service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                let expected = expected.clone();
                let expected_headers = expected_headers.clone();
                let decoded_length = decoded_length.clone();
                assert_eq!(received.fetch_add(1, Ordering::SeqCst), 0, "extra synthetic request/replay");
                async move {
                    *decoded_length.lock().unwrap() = assert_decoded_headers(&request, &expected_headers, expected.len());
                    let mut body = request.into_body();
                    let mut offset = 0;
                    while let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
                        let frame = frame.expect("upstream h2 decoded request frame");
                        let data = frame.into_data().unwrap_or_else(|_| panic!("unexpected synthetic request trailers"));
                        let next = offset + data.len();
                        assert!(next <= expected.len(), "extra request bytes");
                        assert!(data.as_ref() == &expected[offset..next], "request bytes changed at peer");
                        offset = next;
                    }
                    assert_eq!(offset, expected.len(), "incomplete peer request body");
                    assert!(body.is_end_stream(), "peer stream not terminated");
                    Ok::<_, Infallible>(http::Response::new(axum::body::Body::empty()))
                }
            });
            let mut builder = hyper::server::conn::http2::Builder::new(TokioExecutor::new());
            builder.initial_stream_window_size(WINDOW as u32)
                .initial_connection_window_size(WINDOW as u32)
                .max_frame_size(16_384).max_header_list_size(262_144);
            let connection = builder.serve_connection(TokioIo::new(ObservedIo::new(encrypted, observed)), service);
            tokio::pin!(connection);
            tokio::select! {
                result = &mut connection => result.expect("synthetic Hyper connection"),
                accepted = listener.accept() => { accepted.unwrap(); panic!("extra SOCKS connection/replay"); }
                _ = stopped => {
                    connection.as_mut().graceful_shutdown();
                    tokio::select! {
                        result = &mut connection => result.expect("synthetic Hyper shutdown"),
                        accepted = listener.accept() => { accepted.unwrap(); panic!("extra SOCKS connection during shutdown"); }
                    }
                }
            }
        });
        let _peer_guard = AbortPeer(server.abort_handle());
        let response = client.execute(request).await.expect("one native synthetic upload");
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(response.version(), http::Version::HTTP_2);
        assert!(response.bytes().await.unwrap().is_empty());
        let _ = shutdown.send(());
        drop(client);
        server.await.expect("synthetic peer invariants");
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        let frames = assert_encoded_upload(&wire.lock().unwrap(), length);
        let decoded_content_length = *content_length.lock().unwrap();
        serde_json::json!({
            "boundary": "synthetic-peer-tls-plaintext-not-production-ack",
            "decoder": "Hyper/upstream-h2-not-client-http2-fork",
            "alpn_h2": true, "requests": 1, "socks_connections": 1,
            "decoded_content_length": decoded_content_length,
            "received_bytes": length, "wire": frames,
            "production_root_proven": false
        })
    }).await.expect("bounded single synthetic encoded upload")
}
