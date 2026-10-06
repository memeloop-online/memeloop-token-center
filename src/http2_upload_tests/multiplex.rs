use super::*;
use std::collections::BTreeSet;
use tokio::sync::Barrier;

const STREAM_WINDOW: u32 = 16_384;
const LENGTHS: [usize; 3] = [1024, 243 * 1024, 961 * 1024];

struct UploadReceipt {
    stream_id: u32,
    expected_bytes: usize,
    received_bytes: usize,
    reset: bool,
}

async fn receive_mixed_upload(
    request: http::Request<http2::RecvStream>,
    mut respond: http2::server::SendResponse<Bytes>,
    expected: Bytes,
    barrier: Arc<Barrier>,
    reset: bool,
) -> UploadReceipt {
    assert_eq!(request.method(), http::Method::POST);
    assert_eq!(request.uri().authority().unwrap().as_str(), HOST);
    assert_eq!(request.uri().path(), "/v1/responses");
    assert_eq!(request.version(), http::Version::HTTP_2);
    assert_eq!(
        request.headers()[http::header::CONTENT_TYPE],
        "application/json"
    );
    assert_eq!(
        request
            .headers()
            .get_all(http::header::CONTENT_LENGTH)
            .iter()
            .count(),
        1
    );
    assert_eq!(
        request.headers()[http::header::CONTENT_LENGTH]
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        expected.len()
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
    let mut body = request.into_body();
    let stream_id = body.stream_id().as_u32();
    let first = body.data().await.unwrap().unwrap();
    let mut received_bytes = first.len();
    let mut digest = Sha256::new();
    digest.update(&first);
    if expected.len() != PRIME.len() {
        assert!(first.len() <= STREAM_WINDOW as usize);
        barrier.wait().await;
    }
    body.flow_control().release_capacity(first.len()).unwrap();
    if reset {
        respond.send_reset(http2::Reason::PROTOCOL_ERROR);
        return UploadReceipt {
            stream_id,
            expected_bytes: expected.len(),
            received_bytes,
            reset,
        };
    }
    while let Some(chunk) = body.data().await {
        let chunk = chunk.unwrap();
        received_bytes += chunk.len();
        digest.update(&chunk);
        sleep(Duration::from_millis(1)).await;
        body.flow_control().release_capacity(chunk.len()).unwrap();
    }
    assert!(body.trailers().await.unwrap().is_none());
    assert!(body.is_end_stream());
    assert_eq!(received_bytes, expected.len());
    assert_eq!(digest.finalize(), Sha256::digest(&expected));
    respond
        .send_response(http::Response::new(()), true)
        .unwrap();
    UploadReceipt {
        stream_id,
        expected_bytes: expected.len(),
        received_bytes,
        reset,
    }
}

fn assert_mixed_wire(wire: &Wire, receipts: &[UploadReceipt], reset: bool) {
    let mut uploads = BTreeMap::<u32, UploadState>::new();
    let mut connection_window = WINDOW;
    let mut connection_updates = 0;
    let mut continuation = [None, None];
    let mut settings_sent = [0, 0];
    let mut settings_acked = [0, 0];
    let mut first_data = BTreeMap::new();
    let mut last_data = BTreeMap::new();
    let mut headers_at = BTreeMap::new();
    let mut response_streams = BTreeSet::new();
    let mut resets = Vec::new();
    let mut reset_at = BTreeMap::new();
    for (position, frame) in wire.frames.iter().enumerate() {
        let side = usize::from(frame.from_client);
        if let Some(stream_id) = continuation[side] {
            assert_eq!(frame.kind, 9, "interleaved unfinished header block");
            assert_eq!(frame.stream_id, stream_id);
        } else {
            assert_ne!(frame.kind, 9, "orphan CONTINUATION");
        }
        match frame.kind {
            0 => {
                assert!(frame.from_client);
                assert_eq!(frame.flags & !1, 0);
                let upload = uploads
                    .get_mut(&frame.stream_id)
                    .expect("DATA before HEADERS");
                assert_eq!(upload.end_streams, 0, "DATA after END_STREAM");
                connection_window -= frame.length as i64;
                upload.window -= frame.length as i64;
                assert!(connection_window >= 0, "connection upload window exceeded");
                assert!(upload.window >= 0, "stream upload window exceeded");
                upload.bytes += frame.length;
                upload.data_frames += 1;
                upload.end_streams += usize::from(frame.flags & 1 != 0);
                first_data.entry(frame.stream_id).or_insert(position);
                last_data.insert(frame.stream_id, position);
            }
            1 => {
                assert_ne!(frame.stream_id, 0);
                assert_eq!(frame.stream_id % 2, 1);
                if frame.from_client {
                    assert_eq!(frame.flags & 1, 0);
                    assert_eq!(frame.stream_id, uploads.len() as u32 * 2 + 1);
                    assert!(
                        uploads
                            .insert(
                                frame.stream_id,
                                UploadState {
                                    window: i64::from(STREAM_WINDOW),
                                    ..Default::default()
                                }
                            )
                            .is_none()
                    );
                    headers_at.insert(frame.stream_id, position);
                } else {
                    assert_eq!(frame.flags & 1, 1);
                    assert_eq!(uploads[&frame.stream_id].end_streams, 1);
                    assert!(response_streams.insert(frame.stream_id));
                }
                if frame.flags & 4 == 0 {
                    continuation[side] = Some(frame.stream_id);
                }
            }
            3 => {
                assert_eq!(frame.length, 4);
                let reason = u32::from_be_bytes(frame.control[..].try_into().unwrap());
                resets.push((frame.from_client, frame.stream_id, reason));
                reset_at.insert(frame.stream_id, position);
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
            7 => {
                let reason = u32::from_be_bytes(frame.control[4..8].try_into().unwrap());
                panic!(
                    "unexpected GOAWAY: from_client={} reason={reason}",
                    frame.from_client
                );
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
                    }
                }
            }
            9 => {
                if frame.flags & 4 != 0 {
                    continuation[side] = None;
                }
            }
            _ => panic!("unexpected frame kind {}", frame.kind),
        }
    }
    let streams: Vec<_> = receipts
        .iter()
        .map(|receipt| {
            let upload = &uploads[&receipt.stream_id];
            serde_json::json!({
                "stream_id": receipt.stream_id,
                "expected_bytes": receipt.expected_bytes,
                "received_bytes": receipt.received_bytes,
                "data_bytes": upload.bytes,
                "data_frames": upload.data_frames,
                "end_streams": upload.end_streams,
                "stream_window_updates": upload.window_updates,
                "response_headers": response_streams.contains(&receipt.stream_id),
            })
        })
        .collect();
    emit_evidence(serde_json::json!({
        "boundary": "socks5h-cleartext-h2-multiplex-not-tls-not-production-root-proof",
        "phase": "observed_before_final_assertions",
        "connections": 1,
        "socks_handshakes": 1,
        "posts": receipts.len(),
        "injected_protocol_error": reset,
        "connection_window_updates": connection_updates,
        "resets_from_client_stream_reason": resets,
        "streams": streams,
    }));
    assert_eq!(continuation, [None, None]);
    assert_eq!(settings_sent, [1, 1]);
    assert_eq!(settings_acked, settings_sent);
    assert_eq!(uploads.len(), 4);
    assert_eq!(response_streams.len(), if reset { 3 } else { 4 });
    assert!(connection_updates > 1);
    for receipt in receipts {
        let upload = &uploads[&receipt.stream_id];
        if receipt.reset {
            assert_eq!(resets, [(false, receipt.stream_id, 1)]);
            assert!(!response_streams.contains(&receipt.stream_id));
            assert!(receipt.received_bytes < receipt.expected_bytes);
            assert!(upload.bytes < receipt.expected_bytes);
            assert_eq!(upload.end_streams, 0);
        } else {
            assert_eq!(upload.bytes, receipt.expected_bytes);
            assert_eq!(upload.end_streams, 1);
            assert!(response_streams.contains(&receipt.stream_id));
            if receipt.expected_bytes > STREAM_WINDOW as usize {
                assert!(upload.window_updates > 1);
            }
        }
    }
    if !reset {
        assert!(resets.is_empty());
    }
    let large: Vec<_> = receipts
        .iter()
        .filter(|receipt| receipt.expected_bytes > STREAM_WINDOW as usize)
        .collect();
    assert_eq!(large.len(), 2);
    for upload in &large {
        let terminal = if upload.reset {
            reset_at[&upload.stream_id]
        } else {
            last_data[&upload.stream_id]
        };
        for peer in receipts
            .iter()
            .filter(|receipt| receipt.expected_bytes != PRIME.len())
        {
            assert!(headers_at[&peer.stream_id] < terminal);
            assert!(
                first_data[&peer.stream_id] < terminal,
                "mixed streams did not overlap on the observed connection"
            );
        }
    }
    if !reset {
        assert!(first_data[&large[0].stream_id] < last_data[&large[1].stream_id]);
        assert!(first_data[&large[1].stream_id] < last_data[&large[0].stream_id]);
    }
}

async fn mixed_upload_case(reset: bool) {
    timeout(Duration::from_secs(15), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = wreq::Proxy::all(format!("socks5h://{}", listener.local_addr().unwrap())).unwrap();
        let bodies: BTreeMap<_, _> = LENGTHS.into_iter().map(|length| {
            let body = Bytes::from(format!("{{\"input\":\"{}\"}}", "x".repeat(length - 12)));
            assert_eq!(body.len(), length);
            (length, body)
        }).chain([(PRIME.len(), Bytes::from_static(PRIME))]).collect();
        let expected = bodies.clone();
        let barrier = Arc::new(Barrier::new(3));
        let wire = Arc::new(Mutex::new(Wire { started: Instant::now(), frames: Vec::new() }));
        let observed = wire.clone();
        let (shutdown, mut stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            accept_socks(&mut socket).await;
            let mut connection = http2::server::Builder::new()
                .initial_window_size(STREAM_WINDOW)
                .initial_connection_window_size(WINDOW as u32)
                .handshake::<_, Bytes>(ObservedIo::new(socket, observed))
                .await.unwrap();
            let mut handlers = JoinSet::new();
            let mut lengths = BTreeSet::new();
            let mut receipts = Vec::new();
            loop {
                tokio::select! {
                    accepted = connection.accept() => {
                        let (request, respond) = accepted.expect("connection ended before fixture shutdown").unwrap();
                        let length = request.headers()[http::header::CONTENT_LENGTH].to_str().unwrap().parse::<usize>().unwrap();
                        assert!(lengths.insert(length), "POST replayed");
                        let payload = expected.get(&length).expect("unexpected POST").clone();
                        handlers.spawn(receive_mixed_upload(request, respond, payload, barrier.clone(), reset && length == LENGTHS[1]));
                    }
                    accepted = listener.accept() => {
                        accepted.unwrap();
                        panic!("second connection: multiplex/pool reuse requirement failed");
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
            assert_eq!(receipts.len(), 4);
            receipts
        });
        let client = build_codex_http_client_with_policy(CodexTransportPolicy::default()).unwrap();
        let send = |payload: Bytes| {
            client.post(format!("http://{HOST}/v1/responses"))
                .version(http::Version::HTTP_2)
                .proxy(proxy.clone())
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(payload).send()
        };
        let prime = send(bodies[&PRIME.len()].clone()).await.unwrap();
        assert_eq!(prime.status(), http::StatusCode::OK);
        assert_eq!(prime.version(), http::Version::HTTP_2);
        assert!(prime.bytes().await.unwrap().is_empty());
        let results = futures_util::future::join_all(LENGTHS.map(|length| send(bodies[&length].clone()))).await;
        for (length, result) in LENGTHS.into_iter().zip(results) {
            if reset && length == LENGTHS[1] {
                let error = result.unwrap_err();
                let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
                let mut evidence = None;
                while let Some(current) = source {
                    if let Some(http2) = current.downcast_ref::<http2::Error>() {
                        evidence = Some((http2.is_reset(), http2.is_remote(), http2.is_library(), http2.is_go_away(), http2.reason()));
                        break;
                    }
                    source = current.source();
                }
                assert_eq!(evidence, Some((true, true, false, false, Some(http2::Reason::PROTOCOL_ERROR))));
                emit_evidence(serde_json::json!({"phase": "injected_peer_reset_before_response_headers", "reason": 1, "remote": true, "library": false, "goaway": false, "delivery": "fixture_confirmed_partial_upload_not_production_inference"}));
            } else {
                let response = result.unwrap();
                assert_eq!(response.status(), http::StatusCode::OK);
                assert_eq!(response.version(), http::Version::HTTP_2);
                assert!(response.bytes().await.unwrap().is_empty());
            }
        }
        shutdown.send(()).unwrap();
        let receipts = server.await.unwrap();
        assert_mixed_wire(&wire.lock().unwrap(), &receipts, reset);
        emit_evidence(serde_json::json!({
            "boundary": "socks5h-cleartext-h2-multiplex-not-tls-not-production-root-proof",
            "phase": "assertions_passed",
            "injected_protocol_error": reset,
            "successful_posts": if reset { 3 } else { 4 },
            "total_posts_including_prime": 4,
            "replays": 0,
        }));
    }).await.expect("bounded single-connection mixed upload fixture");
}

#[tokio::test]
async fn socks_single_client_same_connection_mixed_uploads_overlap() {
    mixed_upload_case(false).await;
}

#[tokio::test]
async fn socks_mixed_upload_remote_protocol_error_preserves_siblings_without_replay() {
    mixed_upload_case(true).await;
}
