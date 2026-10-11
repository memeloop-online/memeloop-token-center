use super::*;
use tokio::sync::Notify;

const PAYLOAD_BYTES: usize = 595_435;
const STREAM_CREDIT: u32 = 16_384;

struct AbortTask(tokio::task::AbortHandle);

impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Receipt {
    stream_id: u32,
    bytes: usize,
    end_stream: bool,
    cancelled: bool,
    digest: String,
}

async fn receive(
    request: http::Request<http2::RecvStream>,
    mut respond: http2::server::SendResponse<Bytes>,
    big_started: Arc<Notify>,
    sibling_started: Arc<Notify>,
    cancel: bool,
) -> Receipt {
    let declared = request.headers()[http::header::CONTENT_LENGTH]
        .to_str()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert_eq!(request.method(), http::Method::POST);
    assert_eq!(request.uri().path(), "/v1/responses");
    let mut body = request.into_body();
    let stream_id = body.stream_id().as_u32();
    assert_eq!(
        declared,
        if stream_id == 1 {
            PRIME.len()
        } else {
            PAYLOAD_BYTES
        }
    );
    let mut bytes = 0;
    let mut digest = Sha256::new();
    let mut cancelled = false;
    while let Some(chunk) = body.data().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                assert!(cancel && stream_id == 5);
                assert_eq!(error.reason(), Some(http2::Reason::CANCEL));
                assert!(error.is_remote());
                cancelled = true;
                break;
            }
        };
        bytes += chunk.len();
        digest.update(&chunk);
        if stream_id == 3 {
            if bytes == chunk.len() {
                big_started.notify_one();
            }
            sleep(Duration::from_millis(20)).await;
        }
        if stream_id == 5 && bytes == chunk.len() {
            sibling_started.notify_one();
        }
        if !(cancel && stream_id == 5) {
            body.flow_control().release_capacity(chunk.len()).unwrap();
        }
    }
    let end_stream = body.is_end_stream();
    if cancel && stream_id == 5 {
        assert!(cancelled, "client cancellation must reach the peer");
        assert!(bytes > 0 && bytes <= STREAM_CREDIT as usize);
        assert!(bytes < declared);
    } else {
        assert!(!cancelled);
        assert!(body.trailers().await.unwrap().is_none());
        assert!(end_stream);
        assert_eq!(bytes, declared);
        respond
            .send_response(http::Response::new(()), true)
            .unwrap();
    }
    Receipt {
        stream_id,
        bytes,
        end_stream,
        cancelled,
        digest: format!("{:x}", digest.finalize()),
    }
}

fn verify_wire(wire: &Wire, cancel: bool) -> serde_json::Value {
    let mut uploads = BTreeMap::<u32, UploadState>::new();
    let mut connection_credit = WINDOW;
    let mut resets = 0;
    let mut responses = Vec::new();
    let mut reset_receipt = None;
    for frame in &wire.frames {
        match frame.kind {
            1 if frame.from_client => {
                assert_eq!(frame.flags & 1, 0);
                assert!(
                    uploads
                        .insert(
                            frame.stream_id,
                            UploadState {
                                window: i64::from(STREAM_CREDIT),
                                ..Default::default()
                            }
                        )
                        .is_none(),
                    "no stream replay"
                );
            }
            0 if frame.from_client => {
                assert_eq!(frame.flags & !1, 0);
                let upload = uploads.get_mut(&frame.stream_id).unwrap();
                assert_eq!(upload.end_streams, 0, "DATA after END_STREAM");
                connection_credit -= frame.length as i64;
                upload.window -= frame.length as i64;
                assert!(
                    connection_credit >= 0,
                    "connection credit exceeded after sibling cancel"
                );
                assert!(
                    upload.window >= 0,
                    "stream credit exceeded after sibling cancel"
                );
                upload.bytes += frame.length;
                upload.end_streams += usize::from(frame.flags & 1 != 0);
            }
            8 if !frame.from_client => {
                let increment =
                    u32::from_be_bytes(frame.control[..].try_into().unwrap()) & 0x7fff_ffff;
                assert!(increment > 0);
                if frame.stream_id == 0 {
                    connection_credit += i64::from(increment);
                } else {
                    uploads.get_mut(&frame.stream_id).unwrap().window += i64::from(increment);
                }
            }
            3 => {
                assert!(cancel && frame.from_client);
                assert_eq!(frame.stream_id, 5, "only sibling can be cancelled");
                assert_eq!(u32::from_be_bytes(frame.control[..].try_into().unwrap()), 8);
                resets += 1;
                let sibling = &uploads[&5];
                let big = &uploads[&3];
                assert!(sibling.bytes > 0 && sibling.bytes < PAYLOAD_BYTES);
                assert_eq!(sibling.end_streams, 0);
                assert!(
                    big.bytes > 0 && big.bytes < PAYLOAD_BYTES,
                    "big upload must overlap cancel"
                );
                assert_eq!(big.end_streams, 0);
                reset_receipt = Some(serde_json::json!({
                    "big_data_bytes": big.bytes, "big_end_streams": big.end_streams,
                    "sibling_data_bytes": sibling.bytes, "sibling_end_streams": sibling.end_streams,
                }));
            }
            1 => {
                assert_eq!(uploads[&frame.stream_id].end_streams, 1);
                responses.push(frame.stream_id);
            }
            7 => panic!("unexpected GOAWAY"),
            _ => {}
        }
    }
    assert_eq!(uploads.keys().copied().collect::<Vec<_>>(), vec![1, 3, 5]);
    assert_eq!(uploads[&1].bytes, PRIME.len());
    assert_eq!(uploads[&3].bytes, PAYLOAD_BYTES);
    assert_eq!(uploads[&3].end_streams, 1);
    assert_eq!(resets, usize::from(cancel));
    responses.sort_unstable();
    assert_eq!(responses, if cancel { vec![1, 3] } else { vec![1, 3, 5] });
    if !cancel {
        assert_eq!(uploads[&5].bytes, PAYLOAD_BYTES);
        assert_eq!(uploads[&5].end_streams, 1);
    }
    serde_json::json!({ "reset_observation": reset_receipt, "big_data_bytes": uploads[&3].bytes,
        "big_end_streams": uploads[&3].end_streams, "client_cancel_frames": resets })
}

async fn send(client: wreq::Client, proxy: wreq::Proxy, payload: Bytes) -> wreq::Response {
    let response = client
        .post(format!("https://{HOST}/v1/responses"))
        .proxy(proxy)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(payload)
        .send()
        .await
        .unwrap();
    assert_eq!(response.version(), http::Version::HTTP_2);
    assert_eq!(response.status(), http::StatusCode::OK);
    response
}

async fn scenario(cancel: bool) {
    timeout(Duration::from_secs(30), async {
        let fixture = tls_fixture::TlsFixture::new().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = wreq::Proxy::all(format!("socks5h://{}", listener.local_addr().unwrap())).unwrap();
        let client = super::super::codex_http_client_builder(
            CodexTransportPolicy::default(), super::super::CODEX_HTTP2_KEEP_ALIVE_INTERVAL,
        ).tls_cert_store(fixture.roots).build().unwrap();
        let wire = Arc::new(Mutex::new(Wire { started: Instant::now(), frames: Vec::new() }));
        let big_started = Arc::new(Notify::new());
        let sibling_started = Arc::new(Notify::new());
        let (shutdown, mut stopped) = oneshot::channel();
        let server = tokio::spawn({
            let wire = wire.clone();
            let big_started = big_started.clone();
            let sibling_started = sibling_started.clone();
            async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                accept_socks_at_port(&mut socket, 443).await;
                let encrypted = fixture.acceptor.accept(socket).await.unwrap();
                assert_eq!(encrypted.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
                let mut connection = http2::server::Builder::new()
                    .initial_window_size(STREAM_CREDIT)
                    .handshake::<_, Bytes>(ObservedIo::new(encrypted, wire)).await.unwrap();
                let mut handlers = JoinSet::new();
                let mut receipts = Vec::new();
                let mut requests = 0;
                loop {
                    tokio::select! {
                        accepted = connection.accept() => {
                            let (request, respond) = accepted.unwrap().unwrap();
                            requests += 1;
                            assert!(requests <= 3, "no extra POST or retry");
                            handlers.spawn(receive(request, respond, big_started.clone(), sibling_started.clone(), cancel));
                        }
                        extra = listener.accept() => { extra.unwrap(); panic!("no second SOCKS connection"); }
                        completed = handlers.join_next(), if !handlers.is_empty() => {
                            receipts.push(completed.unwrap().unwrap());
                        }
                        _ = &mut stopped => break,
                    }
                }
                while let Some(completed) = handlers.join_next().await { receipts.push(completed.unwrap()); }
                assert_eq!(requests, 3);
                receipts
            }
        });
        let _server_guard = AbortTask(server.abort_handle());
        let prime = send(client.clone(), proxy.clone(), Bytes::from_static(PRIME)).await;
        assert!(prime.bytes().await.unwrap().is_empty());
        let payload = Bytes::from(vec![b'x'; PAYLOAD_BYTES]);
        let expected_digest = format!("{:x}", Sha256::digest(&payload));
        let big = tokio::spawn(send(client.clone(), proxy.clone(), payload.clone()));
        let _big_guard = AbortTask(big.abort_handle());
        big_started.notified().await;
        let sibling = tokio::spawn(send(client.clone(), proxy.clone(), payload));
        let _sibling_guard = AbortTask(sibling.abort_handle());
        sibling_started.notified().await;
        if cancel {
            sibling.abort();
            assert!(sibling.await.unwrap_err().is_cancelled());
        } else {
            assert!(sibling.await.unwrap().bytes().await.unwrap().is_empty());
        }
        assert!(big.await.unwrap().bytes().await.unwrap().is_empty());
        shutdown.send(()).unwrap();
        let receipts = server.await.unwrap();
        let big_receipt = receipts.iter().find(|receipt| receipt.stream_id == 3).unwrap();
        assert_eq!(big_receipt.bytes, PAYLOAD_BYTES);
        assert!(big_receipt.end_stream && !big_receipt.cancelled);
        assert_eq!(big_receipt.digest, expected_digest);
        let sibling_receipt = receipts.iter().find(|receipt| receipt.stream_id == 5).unwrap();
        assert_eq!(sibling_receipt.cancelled, cancel);
        let evidence = verify_wire(&wire.lock().unwrap(), cancel);
        emit_evidence(serde_json::json!({
            "boundary": "synthetic-socks-tls-h2-client-cancel-not-production-root-proof",
            "cancel_arm": cancel, "posts": receipts.len(), "socks_connections": 1,
            "sibling_peer_bytes": sibling_receipt.bytes, "wire": evidence,
            "production_root_proven": false,
        }));
    }).await.expect("bounded client-cancel window experiment");
}

#[tokio::test]
async fn reused_tls_upload_credit_survives_sibling_client_cancel() {
    scenario(false).await;
    scenario(true).await;
}
