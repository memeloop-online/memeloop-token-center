use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use super::{build_codex_http2_test_client, provider::CodexTransportPolicy};

const CONNECTION_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const FRAME_HEADERS: u8 = 0x1;
const FRAME_SETTINGS: u8 = 0x4;
const FRAME_PING: u8 = 0x6;
const FLAG_ACK: u8 = 0x1;
const FLAG_END_STREAM: u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;

struct Frame {
    kind: u8,
    flags: u8,
    stream_id: u32,
    payload: Vec<u8>,
}

async fn read_frame(stream: &mut TcpStream) -> Frame {
    let mut header = [0_u8; 9];
    stream.read_exact(&mut header).await.unwrap();
    let length =
        usize::from(header[0]) << 16 | usize::from(header[1]) << 8 | usize::from(header[2]);
    assert!(length <= 16_384, "unexpected HTTP/2 frame length");
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await.unwrap();
    Frame {
        kind: header[3],
        flags: header[4],
        stream_id: u32::from_be_bytes([header[5], header[6], header[7], header[8]]) & 0x7fff_ffff,
        payload,
    }
}

async fn write_frame(stream: &mut TcpStream, kind: u8, flags: u8, stream_id: u32, payload: &[u8]) {
    let length = payload.len();
    assert!(length <= 0x00ff_ffff);
    let mut header = [0_u8; 9];
    header[..3].copy_from_slice(&(length as u32).to_be_bytes()[1..]);
    header[3] = kind;
    header[4] = flags;
    header[5..].copy_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    stream.write_all(&header).await.unwrap();
    stream.write_all(payload).await.unwrap();
}

async fn wait_for_request_and_ping(stream: &mut TcpStream, posts: &AtomicUsize) -> (u32, [u8; 8]) {
    let mut preface = [0_u8; CONNECTION_PREFACE.len()];
    stream.read_exact(&mut preface).await.unwrap();
    assert_eq!(&preface, CONNECTION_PREFACE);

    let mut settings_seen = false;
    let mut request_stream = None;
    loop {
        let frame = read_frame(stream).await;
        match (frame.kind, frame.flags) {
            (FRAME_SETTINGS, flags) if flags & FLAG_ACK == 0 && !settings_seen => {
                settings_seen = true;
                write_frame(stream, FRAME_SETTINGS, 0, 0, &[]).await;
                write_frame(stream, FRAME_SETTINGS, FLAG_ACK, 0, &[]).await;
            }
            (FRAME_HEADERS, _) if frame.stream_id != 0 => {
                posts.fetch_add(1, Ordering::SeqCst);
                assert!(
                    request_stream.replace(frame.stream_id).is_none(),
                    "POST was replayed"
                );
            }
            (FRAME_PING, flags) if flags & FLAG_ACK == 0 => {
                assert!(
                    settings_seen,
                    "client sent a keepalive PING before HTTP/2 setup"
                );
                assert!(
                    request_stream.is_some(),
                    "client sent a keepalive PING without a POST"
                );
                return (
                    request_stream.expect("POST stream ID"),
                    frame.payload.try_into().expect("HTTP/2 PING payload"),
                );
            }
            _ => {}
        }
    }
}

async fn reject_replay_connections(listener: &TcpListener, duration: Duration) {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return,
            accepted = listener.accept() => {
                let _ = accepted.unwrap();
                panic!("POST was replayed on another HTTP/2 connection");
            }
        }
    }
}

async fn delayed_ping_server(
    ack_delay: Option<Duration>,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let posts = Arc::new(AtomicUsize::new(0));
    let count = posts.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let (request_stream, ping) = wait_for_request_and_ping(&mut stream, &count).await;
        let Some(delay) = ack_delay else {
            reject_replay_connections(&listener, Duration::from_millis(1_300)).await;
            return;
        };
        reject_replay_connections(&listener, delay).await;
        write_frame(&mut stream, FRAME_PING, FLAG_ACK, 0, &ping).await;
        write_frame(
            &mut stream,
            FRAME_HEADERS,
            FLAG_END_STREAM | FLAG_END_HEADERS,
            request_stream,
            &[0x88],
        )
        .await;
    });
    (endpoint, posts, server)
}

fn policy(read_timeout: Duration, request_timeout: Duration) -> CodexTransportPolicy {
    let mut policy = CodexTransportPolicy::default();
    policy.read_timeout_millis = read_timeout.as_millis() as u64;
    policy.request_timeout_millis = request_timeout.as_millis() as u64;
    policy
}

#[tokio::test]
async fn delayed_http2_ping_ack_completes_one_quiet_post_within_request_budget() {
    let request_timeout = Duration::from_millis(1_100);
    let (endpoint, posts, server) = delayed_ping_server(Some(Duration::from_millis(70))).await;
    let client = build_codex_http2_test_client(
        policy(Duration::from_millis(1_000), request_timeout),
        Duration::from_millis(10),
    )
    .unwrap();

    let response = client
        .post(endpoint)
        .body("one quiet POST")
        .send()
        .await
        .expect("a delayed PING ACK within the request budget must complete");

    assert_eq!(response.version(), http::Version::HTTP_2);
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(posts.load(Ordering::SeqCst), 1);
    server.await.unwrap();
}

#[tokio::test]
async fn unacknowledged_http2_ping_stops_at_request_budget_without_replaying_post() {
    let request_timeout = Duration::from_millis(1_000);
    let (endpoint, posts, server) = delayed_ping_server(None).await;
    let client = build_codex_http2_test_client(
        policy(Duration::from_millis(1_000), request_timeout),
        Duration::from_millis(10),
    )
    .unwrap();

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        client.post(endpoint).body("one bounded POST").send(),
    )
    .await
    .expect("keepalive ACK wait must use the configured request timeout");

    assert!(
        result.is_err(),
        "unacknowledged PING must close the connection"
    );
    assert_eq!(posts.load(Ordering::SeqCst), 1, "POST must not be replayed");
    server.await.unwrap();
}
