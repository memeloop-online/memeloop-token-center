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
    sync::oneshot,
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
async fn http1_large_declared_and_chunked_uploads_deliver_413_and_recover() {
    let server = Server::start().await;
    server.normal_request().await;
    for chunked in [false, true] {
        let mut socket = TcpStream::connect(server.address).await.unwrap();
        let size = LIMIT + 1024;
        let framing = if chunked {
            "Transfer-Encoding: chunked\r\n".to_owned()
        } else {
            format!("Content-Length: {size}\r\n")
        };
        socket.write_all(&server.headers(&framing)).await.unwrap();
        let (mut read, mut write) = socket.into_split();
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
            write.shutdown().await
        });
        assert_rejection(&read_response(&mut read).await);
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
