//! Real sockets exercise Hyper and TCP teardown; in-memory Body tests cannot.
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{Router, middleware, routing::post};
use rust_decimal::Decimal;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Notify, oneshot},
    task::JoinHandle,
};

use super::*;
use crate::{db::CreateKeyInput, model::KeyPolicy};

const LIMIT: usize = 4 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(5);

struct Server {
    address: SocketAddr,
    key: String,
    reached_handler: Arc<AtomicUsize>,
    state: AppState,
    _directory: tempfile::TempDir,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Server {
    async fn start() -> Self {
        Self::start_with_response_gate(None).await
    }

    async fn start_with_response_gate(gate: Option<(Arc<Notify>, Arc<Notify>)>) -> Self {
        let (state, directory) = crate::api::tests::test_state().await;
        let issued = state
            .db
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "http1-rejection".into(),
                    principal_external_id: "socket-test".into(),
                    alias: "socket-test".into(),
                    currency: "USD".into(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ZERO,
                    idempotency_key: None,
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let reached_handler = Arc::new(AtomicUsize::new(0));
        let calls = reached_handler.clone();
        // Use the production authentication/body admission and request-ID
        // middleware, with a counted upstream substitute after admission.
        let app = Router::new()
            .route(
                "/v1/chat/completions",
                post(move || {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::Relaxed);
                        "ok"
                    }
                }),
            )
            .layer(middleware::from_fn_with_state(
                state.clone(),
                authenticate_gateway_before_body,
            ))
            .layer(middleware::from_fn(
                move |request: axum::extract::Request, next: middleware::Next| {
                    let gate = gate.clone();
                    async move {
                        let response = next.run(request).await;
                        if let Some((ready, release)) = gate {
                            ready.notify_one();
                            release.notified().await;
                        }
                        response
                    }
                },
            ))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                crate::api::health::observe_http,
            ));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receive) = oneshot::channel();
        let task = tokio::spawn(async move {
            crate::server::serve(listener, app, async {
                let _ = receive.await;
            })
            .await
            .unwrap();
        });
        Self {
            address,
            key: issued.key,
            reached_handler,
            state,
            _directory: directory,
            shutdown,
            task,
        }
    }

    fn headers(&self, framing: &str) -> Vec<u8> {
        format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\n{framing}\r\n",
            self.key
        )
        .into_bytes()
    }

    async fn normal_request(&self) {
        let mut socket = TcpStream::connect(self.address).await.unwrap();
        let mut request = self.headers("Content-Length: 2\r\nConnection: close\r\n");
        request.extend_from_slice(b"{}");
        socket.write_all(&request).await.unwrap();
        let response = read_response(&mut socket).await;
        assert!(response.starts_with(b"HTTP/1.1 200"));
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        tokio::time::timeout(DEADLINE, self.task)
            .await
            .expect("rejection cleanup must release its connection")
            .unwrap();
    }
}

async fn read_response(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
    let mut response = Vec::new();
    tokio::time::timeout(DEADLINE, reader.read_to_end(&mut response))
        .await
        .expect("final response and write FIN must arrive without awaiting upload")
        .expect("response must survive unread upload without TCP reset");
    response
}

fn response_body(response: &[u8]) -> Vec<u8> {
    let header_end = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .expect("complete response headers");
    let headers = std::str::from_utf8(&response[..header_end]).unwrap();
    let mut body = &response[header_end + 4..];
    let mut content_length = None;
    let mut chunked = false;
    for line in headers.lines().skip(1) {
        let (name, value) = line.split_once(':').unwrap();
        if name.eq_ignore_ascii_case("content-length") {
            assert!(content_length.is_none(), "duplicate Content-Length");
            content_length = Some(value.trim().parse::<usize>().unwrap());
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            assert!(!chunked, "duplicate Transfer-Encoding");
            assert!(value.trim().eq_ignore_ascii_case("chunked"));
            chunked = true;
        }
    }
    if !chunked {
        assert_eq!(body.len(), content_length.expect("framed response body"));
        return body.to_vec();
    }
    assert!(content_length.is_none(), "ambiguous response framing");
    let mut decoded = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|bytes| bytes == b"\r\n")
            .expect("complete chunk size");
        let size = std::str::from_utf8(&body[..line_end]).unwrap();
        let size = usize::from_str_radix(size.split(';').next().unwrap(), 16).unwrap();
        body = &body[line_end + 2..];
        if size == 0 {
            // This handler emits no trailers. Require the terminating blank
            // line and reject any bytes beyond the complete response.
            assert_eq!(body, b"\r\n", "complete chunk terminator");
            return decoded;
        }
        assert!(body.len() >= 2 && size <= body.len() - 2, "complete chunk");
        decoded.extend_from_slice(&body[..size]);
        assert_eq!(&body[size..size + 2], b"\r\n");
        body = &body[size + 2..];
    }
}

fn assert_rejection(response: &[u8]) {
    let response = std::str::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 413"), "{response}");
    assert!(!response.contains("100 Continue"), "{response}");
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    let id = headers
        .lines()
        .find_map(|line| line.strip_prefix("x-mtc-request-id: "))
        .expect("production request ID survives transport teardown");
    uuid::Uuid::parse_str(id).unwrap();
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(json["error"]["code"], "request_body_too_large");
    assert!(headers.contains("connection: close"));
}

#[tokio::test]
async fn http1_completed_upload_half_close_preserves_pending_response() {
    let ready = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let server = Server::start_with_response_gate(Some((ready.clone(), release.clone()))).await;
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    // Keep-alive framing lets Hyper probe for EOF while the response is pending.
    let mut request = server.headers("Content-Length: 2\r\n");
    request.extend_from_slice(b"{}");
    socket.write_all(&request).await.unwrap();
    tokio::time::timeout(DEADLINE, ready.notified())
        .await
        .expect("production authentication and body admission must complete");
    socket.shutdown().await.unwrap();
    let mut byte = [0_u8; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(100), socket.read(&mut byte))
            .await
            .is_err(),
        "a write FIN must not close the pending response or cancel its service"
    );
    release.notify_one();
    let response = read_response(&mut socket).await;
    assert!(response.starts_with(b"HTTP/1.1 200"), "{response:?}");
    assert_eq!(response_body(&response), b"ok");
    assert_eq!(server.reached_handler.load(Ordering::Relaxed), 1);
    assert_eq!(server.state.gateway_body_reads.load(Ordering::Acquire), 0);
    server.stop().await;
}

#[tokio::test]
async fn http1_large_declared_and_chunked_uploads_deliver_413_and_recover() {
    let server = Server::start().await;
    server.normal_request().await;
    for (chunked, half_close) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut socket = TcpStream::connect(server.address).await.unwrap();
        let size = LIMIT + 1024;
        let framing = if chunked {
            "Transfer-Encoding: chunked\r\n".to_owned()
        } else {
            format!("Content-Length: {size}\r\n")
        };
        socket.write_all(&server.headers(&framing)).await.unwrap();
        let (mut read, mut write) = socket.into_split();
        let (response_received, wait_for_response) = oneshot::channel();
        let writer = tokio::spawn(async move {
            if chunked {
                write.write_all(format!("{size:x}\r\n").as_bytes()).await?;
            }
            // Continue uploading while the reader receives the rejection,
            // matching a proxy with data already in flight.
            let bytes = [b'x'; 16 * 1024];
            let mut remaining = size;
            while remaining != 0 {
                let count = remaining.min(bytes.len());
                write.write_all(&bytes[..count]).await?;
                remaining -= count;
                tokio::task::yield_now().await;
            }
            if chunked {
                write.write_all(b"\r\n0\r\n\r\n").await?;
            }
            if !half_close {
                // Exercise response delivery without relying on the client's
                // write FIN to finish the rejected request's cleanup.
                let _ = wait_for_response.await;
            }
            write.shutdown().await
        });
        let response = read_response(&mut read).await;
        assert!(
            !response.is_empty(),
            "missing rejection: chunked={chunked}, half_close={half_close}"
        );
        assert_rejection(&response);
        let _ = response_received.send(());
        tokio::time::timeout(DEADLINE, writer)
            .await
            .unwrap()
            .unwrap()
            .expect("finite rejected upload is discarded while the response is delivered");
        assert_eq!(server.reached_handler.load(Ordering::Relaxed), 1);
        assert_eq!(server.state.gateway_body_reads.load(Ordering::Acquire), 0);
    }
    server.normal_request().await;
    assert_eq!(server.reached_handler.load(Ordering::Relaxed), 2);
    server.stop().await;
}

#[tokio::test]
async fn http1_declared_oversize_rejects_headers_only_with_and_without_expect() {
    let server = Server::start().await;
    for expect in ["", "Expect: 100-continue\r\n"] {
        let mut socket = TcpStream::connect(server.address).await.unwrap();
        socket
            .write_all(&server.headers(&format!("Content-Length: {}\r\n{expect}", LIMIT + 1024)))
            .await
            .unwrap();
        // No upload bytes: this would deadlock if cleanup preceded the 413.
        let response = tokio::time::timeout(Duration::from_millis(750), read_response(&mut socket))
            .await
            .expect("413 and FIN precede the one-second cleanup deadline");
        assert_rejection(&response);
    }
    assert_eq!(server.reached_handler.load(Ordering::Relaxed), 0);
    server.normal_request().await;
    server.stop().await;
}

#[tokio::test]
async fn http1_never_ending_rejected_upload_does_not_block_shutdown() {
    let server = Server::start().await;
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    socket
        // Keep the declared length representable by Hyper's internal decoded
        // length (u64::MAX is a sentinel and is rejected before admission).
        .write_all(&server.headers("Content-Length: 1099511627776\r\n"))
        .await
        .unwrap();
    let (mut read, mut write) = socket.into_split();
    assert_rejection(&read_response(&mut read).await);
    // Keep the upload side open and active after the final response/FIN.
    let writer = tokio::spawn(async move {
        let bytes = [b'x'; 16 * 1024];
        while write.write_all(&bytes).await.is_ok() {
            tokio::task::yield_now().await;
        }
    });
    server.stop().await;
    tokio::time::timeout(DEADLINE, writer)
        .await
        .unwrap()
        .unwrap();
}
