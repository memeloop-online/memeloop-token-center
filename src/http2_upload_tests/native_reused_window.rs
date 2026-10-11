//! Reused-connection upload at the production-observed payload magnitude with
//! paced peer WINDOW_UPDATE emission and one concurrent sibling stream.
//! Synthetic bytes only; a pass narrows the candidate space and does not
//! reproduce or prove the production remote reset.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

const PROD_MAGNITUDE: usize = 595_435;
const SIBLING: usize = 4_096;
const STREAM_WINDOW: u32 = 16_384;
const PACE: Duration = Duration::from_millis(50);

struct PacedReceipt {
    stream_id: u32,
    expected_bytes: usize,
    received_bytes: usize,
    digest: String,
}

async fn receive_paced_upload(
    request: http::Request<http2::RecvStream>,
    mut respond: http2::server::SendResponse<Bytes>,
    big_started: Arc<Notify>,
    big_received: Arc<AtomicUsize>,
) -> PacedReceipt {
    assert_eq!(request.method(), http::Method::POST);
    assert_eq!(request.uri().authority().unwrap().as_str(), HOST);
    assert_eq!(request.uri().path(), "/v1/responses");
    assert_eq!(request.version(), http::Version::HTTP_2);
    assert_eq!(
        request.headers()[http::header::CONTENT_TYPE],
        "application/json"
    );
    let expected = request.headers()[http::header::CONTENT_LENGTH]
        .to_str()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(
        matches!(expected, PROD_MAGNITUDE | SIBLING) || expected == PRIME.len(),
        "unexpected synthetic upload length"
    );
    for forbidden in [
        "host",
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "upgrade",
        "te",
    ] {
        assert!(!request.headers().contains_key(forbidden));
    }
    let big = expected == PROD_MAGNITUDE;
    let sibling = expected == SIBLING;
    let mut body = request.into_body();
    let stream_id = body.stream_id().as_u32();
    let mut received_bytes = 0;
    let mut digest = Sha256::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.unwrap();
        received_bytes += chunk.len();
        digest.update(&chunk);
        if big {
            if received_bytes == chunk.len() {
                big_started.notify_one();
            }
            big_received.store(received_bytes, Ordering::SeqCst);
            // Drip capacity back so the client stays window-limited and each
            // peer WINDOW_UPDATE is paced instead of bursting on loopback.
            sleep(PACE).await;
        }
        body.flow_control().release_capacity(chunk.len()).unwrap();
    }
    assert!(body.trailers().await.unwrap().is_none());
    assert!(body.is_end_stream());
    assert_eq!(received_bytes, expected);
    if sibling {
        // Only the concurrent sibling waits for overlap; the prime request
        // opens the connection before the big upload exists and must respond
        // immediately.
        big_started.notified().await;
        sleep(Duration::from_millis(200)).await;
        assert!(
            big_received.load(Ordering::SeqCst) < PROD_MAGNITUDE,
            "sibling did not overlap the paced upload"
        );
    }
    respond
        .send_response(http::Response::new(()), true)
        .unwrap();
    PacedReceipt {
        stream_id,
        expected_bytes: expected,
        received_bytes,
        digest: format!("{:x}", digest.finalize()),
    }
}

fn assert_paced_wire(wire: &Wire) -> serde_json::Value {
    let mut uploads = BTreeMap::<u32, UploadState>::new();
    let mut connection_window = WINDOW;
    let mut connection_updates = 0;
    let mut continuation = [None, None];
    let mut settings_sent = [0, 0];
    let mut settings_acked = [0, 0];
    let mut response_streams = Vec::new();
    let mut update_times = BTreeMap::<u32, Vec<Duration>>::new();
    for frame in &wire.frames {
        let side = usize::from(frame.from_client);
        if let Some(stream_id) = continuation[side] {
            assert_eq!(frame.kind, 9, "interleaved unfinished header block");
            assert_eq!(frame.stream_id, stream_id);
        } else {
            assert_ne!(frame.kind, 9, "orphan CONTINUATION");
        }
        match frame.kind {
            0 => {
                assert!(frame.from_client, "fixture sends headers-only responses");
                assert_eq!(frame.flags & !1, 0, "unexpected DATA flags/padding");
                let upload = uploads
                    .get_mut(&frame.stream_id)
                    .expect("DATA before HEADERS");
                assert_eq!(upload.end_streams, 0, "DATA after END_STREAM");
                connection_window -= frame.length as i64;
                upload.window -= frame.length as i64;
                assert!(connection_window >= 0, "exceeded connection send window");
                assert!(upload.window >= 0, "exceeded stream send window");
                upload.bytes += frame.length;
                upload.data_frames += 1;
                upload.end_streams += usize::from(frame.flags & 1 != 0);
            }
            1 => {
                assert_ne!(frame.stream_id, 0);
                assert_eq!(frame.stream_id % 2, 1);
                if frame.from_client {
                    assert_eq!(frame.flags & 1, 0, "nonempty POST ended at HEADERS");
                    assert!(
                        !uploads.contains_key(&frame.stream_id),
                        "stream identifier reused"
                    );
                    assert!(
                        frame.stream_id == 1 || uploads.contains_key(&1),
                        "prime stream must open the reused connection first"
                    );
                    uploads.insert(
                        frame.stream_id,
                        UploadState {
                            window: i64::from(STREAM_WINDOW),
                            ..Default::default()
                        },
                    );
                } else {
                    assert_eq!(frame.flags & 1, 1);
                    assert_eq!(
                        uploads[&frame.stream_id].end_streams, 1,
                        "response before complete request stream"
                    );
                    response_streams.push(frame.stream_id);
                }
                if frame.flags & 4 == 0 {
                    continuation[side] = Some(frame.stream_id);
                }
            }
            4 => {
                assert_eq!(frame.stream_id, 0);
                if frame.flags & 1 != 0 {
                    assert_eq!(frame.length, 0);
                    settings_acked[1 - side] += 1;
                    assert!(settings_acked[1 - side] <= settings_sent[1 - side]);
                } else {
                    assert_eq!(frame.length % 6, 0);
                    settings_sent[side] += 1;
                    for setting in frame.control.chunks_exact(6) {
                        let identifier = u16::from_be_bytes(setting[..2].try_into().unwrap());
                        let value = u32::from_be_bytes(setting[2..].try_into().unwrap());
                        if !frame.from_client && identifier == 4 {
                            assert_eq!(value, STREAM_WINDOW);
                        }
                    }
                }
            }
            6 => {
                assert_eq!(frame.stream_id, 0);
                assert_eq!(frame.length, 8);
            }
            8 => {
                assert_eq!(frame.length, 4);
                let increment =
                    u32::from_be_bytes(frame.control[..].try_into().unwrap()) & 0x7fff_ffff;
                assert!(increment > 0);
                if !frame.from_client {
                    if frame.stream_id == 0 {
                        connection_window += i64::from(increment);
                        assert!(connection_window <= 0x7fff_ffff);
                        connection_updates += 1;
                    } else {
                        let upload = uploads.get_mut(&frame.stream_id).unwrap();
                        upload.window += i64::from(increment);
                        assert!(upload.window <= 0x7fff_ffff);
                        upload.window_updates += 1;
                        update_times
                            .entry(frame.stream_id)
                            .or_default()
                            .push(frame.elapsed);
                    }
                }
            }
            9 => {
                if frame.flags & 4 != 0 {
                    continuation[side] = None;
                }
            }
            _ => panic!(
                "unexpected frame kind {} (including RST/GOAWAY)",
                frame.kind
            ),
        }
    }
    assert_eq!(continuation, [None, None]);
    assert_eq!(settings_sent, [1, 1]);
    assert_eq!(settings_acked, settings_sent);
    assert_eq!(uploads.len(), 3, "prime + paced upload + sibling");
    let mut ids: Vec<u32> = uploads.keys().copied().collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![1, 3, 5],
        "one reused connection, sequential streams"
    );
    assert_eq!(response_streams.len(), 3);
    assert_eq!(uploads[&1].bytes, PRIME.len());
    assert_eq!(uploads[&1].end_streams, 1);
    let (big_id, big) = uploads
        .iter()
        .find(|(id, upload)| **id != 1 && upload.bytes == PROD_MAGNITUDE)
        .expect("production-magnitude stream");
    assert_eq!(big.end_streams, 1);
    assert!(big.data_frames >= PROD_MAGNITUDE.div_ceil(16_384));
    let sibling = uploads
        .values()
        .find(|upload| upload.bytes == SIBLING)
        .expect("concurrent sibling stream");
    assert_eq!(sibling.end_streams, 1);
    assert!(
        big.window_updates >= 10,
        "paced stream credit updates not exercised"
    );
    assert!(
        connection_updates >= 10,
        "paced connection credit updates not exercised"
    );
    let big_times = &update_times[big_id];
    assert_eq!(big_times.len(), big.window_updates);
    let spread = big_times.last().copied().unwrap() - big_times[0];
    assert!(
        spread >= Duration::from_secs(1),
        "window updates were not paced over time"
    );
    serde_json::json!({
        "streams": ids,
        "big_stream_id": big_id,
        "big_data_frames": big.data_frames,
        "big_stream_window_updates": big.window_updates,
        "connection_window_updates": connection_updates,
        "window_update_spread_ms": spread.as_millis(),
    })
}

#[tokio::test]
async fn socks_cleartext_h2_reused_paced_window_595435_with_concurrent_sibling() {
    timeout(Duration::from_secs(60), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = wreq::Proxy::all(format!("socks5h://{}", listener.local_addr().unwrap())).unwrap();
        let synthetic = |length: usize| {
            let body = Bytes::from(format!("{{\"input\":\"{}\"}}", "x".repeat(length - 12)));
            assert_eq!(body.len(), length);
            body
        };
        let big_body = synthetic(PROD_MAGNITUDE);
        let sibling_body = synthetic(SIBLING);
        let big_digest = format!("{:x}", Sha256::digest(&big_body));
        let sibling_digest = format!("{:x}", Sha256::digest(&sibling_body));
        let wire = Arc::new(Mutex::new(Wire { started: Instant::now(), frames: Vec::new() }));
        let observed = wire.clone();
        let big_started = Arc::new(Notify::new());
        let big_received = Arc::new(AtomicUsize::new(0));
        let (shutdown, stopped) = oneshot::channel();
        let server = tokio::spawn({
            let big_started = big_started.clone();
            let big_received = big_received.clone();
            async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                accept_socks(&mut socket).await;
                let mut connection = http2::server::Builder::new()
                    .initial_window_size(STREAM_WINDOW)
                    .initial_connection_window_size(WINDOW as u32)
                    .handshake::<_, Bytes>(ObservedIo::new(socket, observed))
                    .await
                    .unwrap();
                let mut handlers = JoinSet::new();
                let mut stopped = stopped;
                let mut requests = 0;
                let mut receipts = Vec::new();
                loop {
                    tokio::select! {
                        accepted = connection.accept() => {
                            let (request, respond) = accepted.expect("connection ended before test shutdown").unwrap();
                            requests += 1;
                            assert!(requests <= 3, "unexpected extra request/replay");
                            handlers.spawn(receive_paced_upload(
                                request,
                                respond,
                                big_started.clone(),
                                big_received.clone(),
                            ));
                        }
                        accepted = listener.accept() => {
                            accepted.unwrap();
                            panic!("unexpected second SOCKS handshake: retry or lost pool reuse");
                        }
                        completed = handlers.join_next(), if !handlers.is_empty() => {
                            receipts.push(completed.unwrap().unwrap());
                        }
                        _ = &mut stopped => break,
                    }
                }
                while let Some(completed) = handlers.join_next().await {
                    receipts.push(completed.unwrap());
                }
                assert_eq!(requests, 3);
                receipts
            }
        });
        let client = build_codex_http_client_with_policy(CodexTransportPolicy::default()).unwrap();
        let send = |payload: Bytes| {
            client
                .post(format!("http://{HOST}/v1/responses"))
                .version(http::Version::HTTP_2)
                .proxy(proxy.clone())
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(payload)
                .send()
        };
        let prime = send(Bytes::from_static(PRIME)).await.unwrap();
        assert_eq!(prime.version(), http::Version::HTTP_2);
        assert!(prime.bytes().await.unwrap().is_empty());
        let started = Instant::now();
        let (big, sibling) = tokio::join!(send(big_body), send(sibling_body));
        for response in [big, sibling] {
            let response = response.unwrap();
            assert_eq!(response.status(), http::StatusCode::OK);
            assert_eq!(response.version(), http::Version::HTTP_2);
            assert!(response.bytes().await.unwrap().is_empty());
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_secs(1),
            "paced upload completed implausibly fast"
        );
        shutdown.send(()).unwrap();
        let receipts = server.await.unwrap();
        let big_receipt = receipts
            .iter()
            .find(|receipt| receipt.expected_bytes == PROD_MAGNITUDE)
            .unwrap();
        assert_eq!(big_receipt.received_bytes, PROD_MAGNITUDE);
        assert_eq!(big_receipt.digest, big_digest);
        let sibling_receipt = receipts
            .iter()
            .find(|receipt| receipt.expected_bytes == SIBLING)
            .unwrap();
        assert_eq!(sibling_receipt.digest, sibling_digest);
        let frames = assert_paced_wire(&wire.lock().unwrap());
        emit_evidence(serde_json::json!({
            "boundary": "socks5h-cleartext-h2-reused-paced-window-not-tls-not-production-root-proof",
            "big_stream_id": big_receipt.stream_id,
            "sibling_stream_id": sibling_receipt.stream_id,
            "body_bytes": PROD_MAGNITUDE,
            "sibling_bytes": SIBLING,
            "socks_handshakes": 1,
            "posts": receipts.len(),
            "send_ms": elapsed.as_millis(),
            "wire": frames,
            "production_root_proven": false,
        }));
    })
    .await
    .expect("bounded reused-connection paced-window scenario");
}
