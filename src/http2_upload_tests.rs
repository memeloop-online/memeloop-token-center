use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, Write},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, ready},
    time::Duration,
};

use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinSet,
    time::{Instant, sleep, sleep_until, timeout},
};

use super::{build_codex_http_client_with_policy, provider::CodexTransportPolicy};

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const HEADERS_DELAY: Duration = Duration::from_secs(42);
const REUSE_IDLE: Duration = Duration::from_secs(25);
const WINDOW: i64 = 65_535;
const PRIME: &[u8] = b"prime";
const HOST: &str = "upload.example.test";

#[path = "http2_upload_tests/multiplex.rs"]
mod multiplex;

struct Frame {
    from_client: bool,
    kind: u8,
    flags: u8,
    stream_id: u32,
    length: usize,
    control: Vec<u8>,
    elapsed: Duration,
}

struct Wire {
    started: Instant,
    frames: Vec<Frame>,
}

struct Decoder {
    from_client: bool,
    preface_pending: bool,
    buffered: Vec<u8>,
    wire: Arc<Mutex<Wire>>,
}

impl Decoder {
    fn record(&mut self, bytes: &[u8]) {
        self.buffered.extend_from_slice(bytes);
        if self.preface_pending {
            if self.buffered.len() < PREFACE.len() {
                return;
            }
            assert_eq!(&self.buffered[..PREFACE.len()], PREFACE);
            self.buffered.drain(..PREFACE.len());
            self.preface_pending = false;
        }
        while self.buffered.len() >= 9 {
            let length = usize::from(self.buffered[0]) << 16
                | usize::from(self.buffered[1]) << 8
                | usize::from(self.buffered[2]);
            assert!(length <= 16_384, "frame exceeds the advertised maximum");
            if self.buffered.len() < 9 + length {
                return;
            }
            let kind = self.buffered[3];
            let stream_id =
                u32::from_be_bytes(self.buffered[5..9].try_into().unwrap()) & 0x7fff_ffff;
            let mut wire = self.wire.lock().unwrap();
            let elapsed = wire.started.elapsed();
            wire.frames.push(Frame {
                from_client: self.from_client,
                kind,
                flags: self.buffered[4],
                stream_id,
                length,
                control: if matches!(kind, 0 | 1 | 9) {
                    Vec::new()
                } else {
                    self.buffered[9..9 + length].to_vec()
                },
                elapsed,
            });
            self.buffered.drain(..9 + length);
        }
    }
}

struct ObservedIo {
    socket: TcpStream,
    incoming: Decoder,
    outgoing: Decoder,
}

impl ObservedIo {
    fn new(socket: TcpStream, wire: Arc<Mutex<Wire>>) -> Self {
        Self {
            socket,
            incoming: Decoder {
                from_client: true,
                preface_pending: true,
                buffered: Vec::new(),
                wire: wire.clone(),
            },
            outgoing: Decoder {
                from_client: false,
                preface_pending: false,
                buffered: Vec::new(),
                wire,
            },
        }
    }
}

impl AsyncRead for ObservedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let previous = buffer.filled().len();
        ready!(Pin::new(&mut this.socket).poll_read(context, buffer))?;
        this.incoming.record(&buffer.filled()[previous..]);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for ObservedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let written = ready!(Pin::new(&mut this.socket).poll_write(context, buffer))?;
        this.outgoing.record(&buffer[..written]);
        Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_shutdown(context)
    }
}

async fn accept_socks(socket: &mut TcpStream) {
    let mut greeting = [0; 2];
    socket.read_exact(&mut greeting).await.unwrap();
    assert_eq!(greeting[0], 5);
    let mut methods = vec![0; usize::from(greeting[1])];
    socket.read_exact(&mut methods).await.unwrap();
    assert!(methods.contains(&0));
    socket.write_all(&[5, 0]).await.unwrap();
    let mut connect = [0; 4];
    socket.read_exact(&mut connect).await.unwrap();
    assert_eq!(connect, [5, 1, 0, 3], "SOCKS5h must send the domain");
    let length = socket.read_u8().await.unwrap();
    let mut destination = vec![0; usize::from(length)];
    socket.read_exact(&mut destination).await.unwrap();
    assert_eq!(destination, HOST.as_bytes());
    assert_eq!(socket.read_u16().await.unwrap(), 80);
    socket
        .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
        .await
        .unwrap();
}

struct Receipt {
    length: usize,
    digest: String,
    releases: usize,
}

async fn receive_upload(
    request: http::Request<http2::RecvStream>,
    mut respond: http2::server::SendResponse<Bytes>,
    expected: Bytes,
    prime: bool,
) -> Receipt {
    let received_headers = Instant::now();
    assert_eq!(request.method(), http::Method::POST);
    assert_eq!(request.uri().authority().unwrap().as_str(), HOST);
    assert_eq!(request.uri().path(), "/v1/responses");
    assert_eq!(
        request.headers()[http::header::CONTENT_TYPE],
        "application/json"
    );
    assert_eq!(
        request.headers()[http::header::CONTENT_LENGTH]
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        expected.len()
    );
    for forbidden in ["host", "connection", "transfer-encoding", "upgrade"] {
        assert!(!request.headers().contains_key(forbidden));
    }
    let mut body = request.into_body();
    let mut length = 0;
    let mut digest = Sha256::new();
    let mut releases = 0;
    while let Some(chunk) = body.data().await {
        let chunk = chunk.unwrap();
        length += chunk.len();
        digest.update(&chunk);
        if !prime {
            sleep(Duration::from_millis(100)).await;
        }
        body.flow_control().release_capacity(chunk.len()).unwrap();
        releases += 1;
    }
    assert!(body.trailers().await.unwrap().is_none());
    assert!(body.is_end_stream());
    let digest = format!("{:x}", digest.finalize());
    assert_eq!(length, expected.len());
    assert_eq!(digest, format!("{:x}", Sha256::digest(&expected)));
    if !prime {
        sleep_until(received_headers + HEADERS_DELAY).await;
    }
    respond
        .send_response(http::Response::new(()), true)
        .unwrap();
    Receipt {
        length,
        digest,
        releases,
    }
}

#[derive(Default)]
struct UploadState {
    bytes: usize,
    data_frames: usize,
    end_streams: usize,
    window: i64,
    window_updates: usize,
}

fn assert_wire(wire: &Wire, length: usize, reused: bool) -> serde_json::Value {
    let target_stream = if reused { 3 } else { 1 };
    let mut uploads = BTreeMap::<u32, UploadState>::new();
    let mut connection_window = WINDOW;
    let mut connection_updates = 0;
    let mut continuation = [None, None];
    let mut settings_sent = [0, 0];
    let mut settings_acked = [0, 0];
    let mut pending_pings = VecDeque::new();
    let mut ping_times = Vec::new();
    let mut ack_times = Vec::new();
    let mut request_at = None;
    let mut response_at = None;
    let mut response_streams = Vec::new();
    let mut profile_settings = BTreeMap::new();
    for frame in &wire.frames {
        let side = usize::from(frame.from_client);
        if let Some(stream_id) = continuation[side] {
            assert_eq!(frame.kind, 9, "interleaved an unfinished header block");
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
                    assert_eq!(frame.stream_id, uploads.len() as u32 * 2 + 1);
                    assert!(
                        uploads
                            .insert(
                                frame.stream_id,
                                UploadState {
                                    window: WINDOW,
                                    ..Default::default()
                                }
                            )
                            .is_none()
                    );
                    if frame.stream_id == target_stream {
                        request_at = Some(frame.elapsed);
                    }
                } else {
                    assert_eq!(frame.flags & 1, 1);
                    assert!(!response_streams.contains(&frame.stream_id));
                    response_streams.push(frame.stream_id);
                    assert_eq!(uploads[&frame.stream_id].end_streams, 1);
                    if frame.stream_id == target_stream {
                        response_at = Some(frame.elapsed);
                    }
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
                        match identifier {
                            2 | 8 | 9 => assert!(value <= 1),
                            4 => assert!(value <= 0x7fff_ffff),
                            5 => assert!((16_384..=16_777_215).contains(&value)),
                            _ => {}
                        }
                        if frame.from_client {
                            assert!(profile_settings.insert(identifier, value).is_none());
                        } else if identifier == 4 {
                            assert_eq!(i64::from(value), WINDOW);
                        }
                    }
                }
            }
            6 => {
                assert_eq!(frame.stream_id, 0);
                assert_eq!(frame.length, 8);
                if frame.from_client {
                    assert_eq!(frame.flags, 0);
                    assert!(request_at.is_some(), "PING before the active upload");
                    assert!(response_at.is_none(), "PING outside the preheaders window");
                    pending_pings.push_back(frame.control.clone());
                    ping_times.push(frame.elapsed);
                } else {
                    assert_eq!(frame.flags, 1);
                    assert_eq!(pending_pings.pop_front().as_ref(), Some(&frame.control));
                    ack_times.push(frame.elapsed);
                }
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
            _ => panic!(
                "unexpected frame kind {} (including RST/GOAWAY)",
                frame.kind
            ),
        }
    }
    assert_eq!(continuation, [None, None]);
    assert_eq!(settings_sent, [1, 1]);
    assert_eq!(settings_acked, settings_sent);
    assert_eq!(profile_settings.get(&1), Some(&65_536));
    assert_eq!(profile_settings.get(&2), Some(&0));
    assert_eq!(profile_settings.get(&4), Some(&6_291_456));
    assert_eq!(profile_settings.get(&6), Some(&262_144));
    assert_eq!(uploads.len(), if reused { 2 } else { 1 });
    assert_eq!(response_streams.len(), uploads.len());
    if reused {
        assert_eq!(uploads[&1].bytes, PRIME.len());
        assert_eq!(uploads[&1].end_streams, 1);
    }
    let upload = &uploads[&target_stream];
    assert_eq!(upload.bytes, length);
    assert_eq!(upload.end_streams, 1);
    assert!(upload.data_frames >= length.div_ceil(16_384));
    assert!(upload.window_updates >= 2);
    assert!(connection_updates >= 2);
    assert!(
        !ping_times.is_empty(),
        "30s production keepalive was not exercised"
    );
    assert!(pending_pings.is_empty());
    assert_eq!(ping_times.len(), ack_times.len());
    let request_at = request_at.unwrap();
    let response_at = response_at.unwrap();
    assert!(response_at - request_at >= HEADERS_DELAY);
    let first_ping = ping_times[0] - request_at;
    if reused {
        assert!(
            first_ping < Duration::from_secs(20),
            "connection timer restarted per request"
        );
    } else {
        assert!(first_ping >= Duration::from_secs(29));
        assert!(first_ping < Duration::from_secs(40));
    }
    for (ping, ack) in ping_times.iter().zip(&ack_times) {
        assert!(ack >= ping);
        assert!(*ack < response_at);
    }
    serde_json::json!({
        "data_frames": upload.data_frames,
        "end_streams": upload.end_streams,
        "stream_window_updates": upload.window_updates,
        "connection_window_updates": connection_updates,
        "headers_ms": (response_at - request_at).as_millis(),
        "ping_ms": ping_times.iter().map(|instant| (*instant - request_at).as_millis()).collect::<Vec<_>>(),
        "ping_ack_ms": ack_times.iter().map(|instant| (*instant - request_at).as_millis()).collect::<Vec<_>>(),
    })
}

fn emit_evidence(evidence: serde_json::Value) {
    let record = format!("MTC_HTTP2_UPLOAD_EVIDENCE {evidence}\n");
    io::stderr().lock().write_all(record.as_bytes()).unwrap();
}

async fn upload_case(length: usize, reused: bool) {
    timeout(Duration::from_secs(90), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = wreq::Proxy::all(format!("socks5h://{}", listener.local_addr().unwrap())).unwrap();
        let body = Bytes::from(format!("{{\"input\":\"{}\"}}", "x".repeat(length - 12)));
        assert_eq!(body.len(), length);
        let expected = body.clone();
        let wire = Arc::new(Mutex::new(Wire { started: Instant::now(), frames: Vec::new() }));
        let observed = wire.clone();
        let (shutdown, stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            accept_socks(&mut socket).await;
            let mut connection = http2::server::Builder::new()
                .initial_window_size(WINDOW as u32)
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
                        let prime = reused && requests == 0;
                        requests += 1;
                        assert!(requests <= if reused { 2 } else { 1 }, "POST replayed");
                        let expected = if prime { Bytes::from_static(PRIME) } else { expected.clone() };
                        handlers.spawn(receive_upload(request, respond, expected, prime));
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
            assert_eq!(requests, if reused { 2 } else { 1 });
            receipts
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
        if reused {
            let response = send(Bytes::from_static(PRIME)).await.unwrap();
            assert_eq!(response.version(), http::Version::HTTP_2);
            assert!(response.bytes().await.unwrap().is_empty());
            sleep(REUSE_IDLE).await;
        }
        let started = Instant::now();
        let response = send(body).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(response.version(), http::Version::HTTP_2);
        assert!(response.bytes().await.unwrap().is_empty());
        let elapsed = started.elapsed();
        shutdown.send(()).unwrap();
        let receipts = server.await.unwrap();
        let receipt = receipts.iter().find(|receipt| receipt.length == length).unwrap();
        let frames = assert_wire(&wire.lock().unwrap(), length, reused);
        emit_evidence(serde_json::json!({
            "boundary": "socks5h-cleartext-h2-component-not-tls",
            "body_bytes": length,
            "sha256": receipt.digest,
            "release_calls": receipt.releases,
            "reused": reused,
            "idle_seconds": if reused { REUSE_IDLE.as_secs() } else { 0 },
            "socks_handshakes": 1,
            "posts": receipts.len(),
            "send_ms": elapsed.as_millis(),
            "wire": frames,
        }));
    })
    .await
    .expect("bounded 42s preheaders / 25s idle component scenario");
}

#[tokio::test]
async fn socks_cleartext_h2_fresh_243_kib() {
    upload_case(243 * 1024, false).await;
}

#[tokio::test]
async fn socks_cleartext_h2_fresh_961_kib() {
    upload_case(961 * 1024, false).await;
}

#[tokio::test]
async fn socks_cleartext_h2_idle25_243_kib() {
    upload_case(243 * 1024, true).await;
}

#[tokio::test]
async fn socks_cleartext_h2_idle25_961_kib() {
    upload_case(961 * 1024, true).await;
}

async fn write_fixture_frame(
    peer: &mut tokio::io::DuplexStream,
    kind: u8,
    flags: u8,
    stream_id: u32,
    payload: &[u8],
) {
    let mut header = [0; 9];
    header[..3].copy_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
    header[3] = kind;
    header[4] = flags;
    header[5..].copy_from_slice(&stream_id.to_be_bytes());
    peer.write_all(&header).await.unwrap();
    peer.write_all(payload).await.unwrap();
}

#[tokio::test]
async fn pinned_http2_recv_drop_930_characterization_not_upload_root_fix() {
    timeout(Duration::from_secs(5), async {
        let mut cases = Vec::new();
        for read_frames in 0_usize..=1 {
            for released_frames in 0_usize..=1 {
                let (mut peer, socket) = tokio::io::duplex(65_536);
                peer.write_all(PREFACE).await.unwrap();
                write_fixture_frame(&mut peer, 4, 0, 0, &[]).await;
                let mut headers = vec![0x83, 0x86, 0x84, 0x01, HOST.len() as u8];
                headers.extend_from_slice(HOST.as_bytes());
                write_fixture_frame(&mut peer, 1, 4, 1, &headers).await;
                for _ in 0..2 {
                    write_fixture_frame(&mut peer, 0, 0, 1, &[0; 16_384]).await;
                }
                let mut connection = http2::server::handshake(socket).await.unwrap();
                let (request, respond) = connection.accept().await.unwrap().unwrap();
                let mut body = request.into_body();
                let mut flow = body.flow_control().clone();
                std::future::poll_fn(|context| {
                    assert!(connection.poll_accept(context).is_pending());
                    if flow.used_capacity() == 32_768 {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
                for _ in 0..read_frames {
                    assert_eq!(body.data().await.unwrap().unwrap().len(), 16_384);
                }
                for _ in 0..released_frames {
                    flow.release_capacity(16_384).unwrap();
                }
                drop(body);
                let retained = flow.used_capacity();
                let fixed_expected = read_frames.saturating_sub(released_frames) * 16_384;
                assert_eq!(retained, (2 - released_frames) * 16_384);
                assert!(
                    retained > fixed_expected,
                    "pinned dependency behavior changed; reassess #930"
                );
                cases.push(serde_json::json!({
                    "read_frames": read_frames,
                    "released_frames": released_frames,
                    "retained_after_drop": retained,
                    "expected_after_upstream_fix": fixed_expected,
                }));
                drop(flow);
                drop(respond);
                drop(connection);
                drop(peer);
            }
        }
        emit_evidence(serde_json::json!({
            "boundary": "http2-0.5.20-recv-drop-characterization-not-upload-root-cause",
            "upstream": "hyperium/h2#930",
            "cases": cases,
        }));
    })
    .await
    .expect("bounded isolated receive-capacity characterization");
}
