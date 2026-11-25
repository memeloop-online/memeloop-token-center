use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy)]
enum PeerFailure {
    ConnectRefused,
    FullPostEof,
    FullPostH2Reset,
}

async fn assert_wire_delivery(failure: PeerFailure, label: &str) {
    let fixture = codex_route_fixture(label).await;
    let standby = MockServer::start().await;
    let retryable = matches!(failure, PeerFailure::ConnectRefused);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(successful_chat_response())
        .expect(if retryable { 1 } else { 0 })
        .mount(&standby)
        .await;
    add_native_chat_standby_route(&fixture, &format!("codex-route-{label}"), &standby.uri()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let received = Arc::new(AtomicUsize::new(0));
    let count = received.clone();
    let (release, mut released) = tokio::sync::oneshot::channel();
    let server = if retryable {
        drop(listener);
        None
    } else {
        Some(tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut released => break,
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        let count = count.clone();
                        tasks.spawn(async move {
                            if matches!(failure, PeerFailure::FullPostH2Reset) {
                                let mut connection = http2::server::handshake(socket).await.unwrap();
                                while let Some(Ok((request, mut response))) = connection.accept().await {
                                    let count = count.clone();
                                    tokio::spawn(async move {
                                        let mut body = request.into_body();
                                        let mut bytes = Vec::new();
                                        while let Some(data) = body.data().await {
                                            let data = data.unwrap();
                                            bytes.extend_from_slice(&data);
                                            body.flow_control().release_capacity(data.len()).unwrap();
                                        }
                                        assert!(serde_json::from_slice::<Value>(&bytes).unwrap().get("input").is_some());
                                        count.fetch_add(1, Ordering::SeqCst);
                                        response.send_reset(http2::Reason::CANCEL);
                                    });
                                }
                            } else {
                                use http_body_util::BodyExt;
                                let service = hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                                    let count = count.clone();
                                    async move {
                                        let bytes = request.into_body().collect().await.unwrap().to_bytes();
                                        assert!(serde_json::from_slice::<Value>(&bytes).unwrap().get("input").is_some());
                                        count.fetch_add(1, Ordering::SeqCst);
                                        Err::<hyper::Response<http_body_util::Full<Bytes>>, _>(std::io::Error::other("peer closes after full POST"))
                                    }
                                });
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(hyper_util::rt::TokioIo::new(socket), service).await;
                            }
                        });
                    }
                }
            }
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }))
    };
    let mut client = reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never());
    if matches!(failure, PeerFailure::FullPostH2Reset) {
        client = client.http2_prior_knowledge();
    } else {
        client = client.http1_only();
    }
    fixture
        .state
        .codex_clients
        .install_test_client(client.build().unwrap());
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        send_codex_route_to_endpoint(
            &fixture,
            endpoint,
            "/v1/responses",
            json!({"model": fixture.model,"input":"wire proof","stream":false}),
        ),
    )
    .await
    .expect("wire failure must terminate through the original candidate state machine");
    assert_eq!(
        response.status(),
        if retryable {
            StatusCode::OK
        } else {
            StatusCode::BAD_GATEWAY
        }
    );
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    standby.verify().await;
    assert_eq!(
        received.load(Ordering::SeqCst),
        if retryable { 0 } else { 1 }
    );
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let reservations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations WHERE key_id=$1")
            .bind(fixture.key_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        reservations, 1,
        "one logical request owns exactly one reservation"
    );
    let owners: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_records WHERE id=$1 AND key_id=$2")
            .bind(request_id.to_string())
            .bind(fixture.key_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(owners, 1);
    pool.close().await;
    let _ = release.send(());
    if let Some(server) = server {
        server.await.unwrap();
    }
}

#[tokio::test]
async fn confirmed_connect_refusal_uses_original_candidate_and_one_reservation() {
    assert_wire_delivery(PeerFailure::ConnectRefused, "wire-connect-refused").await;
}
#[tokio::test]
async fn full_post_then_eof_never_replays_or_reserves_twice() {
    assert_wire_delivery(PeerFailure::FullPostEof, "wire-full-eof").await;
}
#[tokio::test]
async fn full_post_then_h2_reset_never_replays_or_reserves_twice() {
    assert_wire_delivery(PeerFailure::FullPostH2Reset, "wire-full-h2-reset").await;
}
