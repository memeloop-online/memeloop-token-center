use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

async fn open_stream(address: std::net::SocketAddr, token: &str) -> TcpStream {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(format!(
        "GET /internal/v1/request-events?tenant_external_id=sse-transport&after_event_at=0 HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n\r\n"
    ).as_bytes()).await.unwrap();
    let mut headers = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !headers.ends_with(b"\r\n\r\n") {
            let byte = stream.read_u8().await.unwrap();
            headers.push(byte);
        }
    })
    .await
    .expect("HTTP1 event stream response headers");
    assert!(
        headers.starts_with(b"HTTP/1.1 200"),
        "event stream must open without a connection-count rejection"
    );
    stream
}

async fn observe_event(stream: &mut TcpStream, request_id: Uuid) {
    let id = request_id.to_string();
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !received.windows(id.len()).any(|part| part == id.as_bytes()) {
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0, "write FIN must preserve the event response");
            received.extend_from_slice(&chunk[..count]);
        }
    })
    .await
    .expect("committed event remains readable after client write FIN");
}

#[tokio::test]
async fn http1_event_streams_reopen_after_abort_and_continue_after_write_fin() {
    let (state, _directory) = test_state().await;
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "sse-transport".into(),
                principal_external_id: "sse-transport".into(),
                alias: "sse-transport".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let key = state
        .db
        .authenticate_key(&issued.key, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = router_for_role(state.clone(), RuntimeRole::Control);
    let server = tokio::spawn(async move {
        crate::server::serve(listener, app, std::future::pending())
            .await
            .unwrap();
    });
    let mut aborted = Vec::new();
    for _ in 0..8 {
        aborted.push(open_stream(address, &state.config.service_token).await);
    }
    assert_eq!(state.request_event_streams.active_count(), 8);
    for stream in aborted {
        stream.set_zero_linger().unwrap();
        drop(stream);
    }
    let mut live = open_stream(address, &state.config.service_token).await;
    live.shutdown().await.unwrap();
    // RST closes both sides of each aborted connection. The live subscriber
    // sends only a write FIN and must continue receiving committed events.
    for _ in 0..2 {
        let request_id = Uuid::now_v7();
        let now = crate::db::unix_millis();
        state
            .db
            .record_early_request_terminal(&crate::db::EarlyRequestTerminal {
                request_id,
                tenant_id: key.tenant_id,
                key_id: key.key_id,
                created_at: now,
                completed_at: now,
                duration_ms: 0,
                protocol: "openai".into(),
                model: "sse-transport".into(),
                status_code: 400,
                error_code: "invalid_request".into(),
            })
            .await
            .unwrap();
        observe_event(&mut live, request_id).await;
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.request_event_streams.active_count() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reset HTTP1 subscriptions release their producers");
    live.set_zero_linger().unwrap();
    drop(live);
    // After read EOF, HTTP half-close detects the peer's later reset on a
    // response write. Commit a real event to exercise that write immediately.
    let now = crate::db::unix_millis();
    state
        .db
        .record_early_request_terminal(&crate::db::EarlyRequestTerminal {
            request_id: Uuid::now_v7(),
            tenant_id: key.tenant_id,
            key_id: key.key_id,
            created_at: now,
            completed_at: now,
            duration_ms: 0,
            protocol: "openai".into(),
            model: "sse-transport".into(),
            status_code: 400,
            error_code: "invalid_request".into(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.request_event_streams.active_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("connection teardown drops every stream body and producer");
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}
