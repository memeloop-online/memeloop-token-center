use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const PREFIX: &[u8] = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-h2-cancelled\"}}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"delivered before cancellation\"}\n\n"
)
.as_bytes();

struct StalledHttp2 {
    endpoint: String,
    posts: Arc<AtomicUsize>,
    resets: tokio::sync::mpsc::UnboundedReceiver<http2::Reason>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for StalledHttp2 {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl StalledHttp2 {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let posts = Arc::new(AtomicUsize::new(0));
        let count = posts.clone();
        let (reset_sender, resets) = tokio::sync::mpsc::unbounded_channel();
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    finished = tasks.join_next(), if !tasks.is_empty() => {
                        finished.unwrap().unwrap();
                    }
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let count = count.clone();
                        let reset_sender = reset_sender.clone();
                        tasks.spawn(async move {
                            let mut connection = http2::server::handshake(stream).await.unwrap();
                            let mut monitors = tokio::task::JoinSet::new();
                            let mut held_requests = Vec::new();
                            loop {
                                tokio::select! {
                                    result = monitors.join_next(), if !monitors.is_empty() => {
                                        result.unwrap().unwrap();
                                    }
                                    accepted = connection.accept() => {
                                        let Some(Ok((request, mut response))) = accepted else { break };
                                        assert_eq!(request.version(), http::Version::HTTP_2);
                                        assert_eq!(request.method(), http::Method::POST);
                                        assert_eq!(request.uri().path(), codex_transport::RESPONSES_PATH);
                                        count.fetch_add(1, Ordering::SeqCst);
                                        let headers = http::Response::builder()
                                            .status(200)
                                            .header(http::header::CONTENT_TYPE, "text/event-stream")
                                            .body(())
                                            .unwrap();
                                        let mut stream = response.send_response(headers, false).unwrap();
                                        stream.send_data(bytes::Bytes::from_static(PREFIX), false).unwrap();
                                        let reset_sender = reset_sender.clone();
                                        monitors.spawn(async move {
                                            let reason = std::future::poll_fn(|context| stream.poll_reset(context))
                                                .await.unwrap();
                                            reset_sender.send(reason).unwrap();
                                        });
                                        held_requests.push(request);
                                    }
                                }
                            }
                            while let Some(result) = monitors.join_next().await {
                                result.unwrap();
                            }
                        });
                    }
                }
            }
            tasks.abort_all();
            while let Some(result) = tasks.join_next().await {
                if let Err(error) = result {
                    assert!(error.is_cancelled(), "HTTP/2 server panicked: {error}");
                }
            }
        });
        Self {
            endpoint,
            posts,
            resets,
            stop: Some(stop),
            server,
        }
    }

    async fn finish(&mut self) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            self.posts.load(Ordering::SeqCst),
            1,
            "cancelled POST must not replay"
        );
        assert!(
            self.resets.try_recv().is_err(),
            "only one upstream stream may be cancelled"
        );
        self.stop.take().unwrap().send(()).unwrap();
        (&mut self.server).await.unwrap();
    }
}

async fn single_dispatch_route(fixture: &CodexRouteFixture) -> ResolvedUpstream {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let mut config: Value = serde_json::from_str(
        &sqlx::query_scalar::<_, String>("SELECT config_json FROM upstream_accounts WHERE id = $1")
            .bind(fixture.upstream_account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    config["transport_policy"] = json!({"dispatch_max_in_flight": 1, "dispatch_max_queued": 0});
    sqlx::query("UPDATE upstream_accounts SET config_json = $1 WHERE id = $2")
        .bind(config.to_string())
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    ResolvedUpstream {
        route_id: fixture.route_id,
        account_id: fixture.upstream_account_id,
        transport_revision: i64::MIN,
        credential_generation: 1,
        driver: codex_transport::DRIVER.to_owned(),
        base_url: codex_transport::BASE_URL.to_owned(),
        config,
        upstream_model: fixture.upstream_model.clone(),
        credential: UpstreamCredential::None,
    }
}

async fn assert_network_cancellation(reset_stream: bool) {
    let label = if reset_stream {
        "h2-downstream-rst"
    } else {
        "h2-downstream-tcp-close"
    };
    let mut fixture = codex_route_fixture(label).await;
    fixture.state.proxy_lifecycle_permits = Arc::new(tokio::sync::Semaphore::new(1));
    let route = single_dispatch_route(&fixture).await;
    fixture.state.codex_clients.install_test_client(
        crate::build_codex_http2_test_client(
            crate::provider::CodexTransportPolicy::default(),
            crate::CODEX_HTTP2_KEEP_ALIVE_INTERVAL,
        )
        .unwrap(),
    );
    let mut upstream = StalledHttp2::start().await;
    let endpoint = upstream.endpoint.clone();
    let app = router_for_role(fixture.state.clone(), RuntimeRole::Gateway).layer(
        axum::middleware::from_fn(
            move |request: Request<Body>, next: axum::middleware::Next| {
                let endpoint = endpoint.clone();
                async move {
                    assert_eq!(request.version(), http::Version::HTTP_2);
                    codex_transport::with_test_endpoint(endpoint, next.run(request)).await
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop_gateway, stopped_gateway) = tokio::sync::oneshot::channel();
    let gateway = tokio::spawn(crate::server::serve(listener, app, async {
        stopped_gateway.await.unwrap();
    }));
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (mut client, connection) = http2::client::handshake(stream).await.unwrap();
    let connection = tokio::spawn(connection);
    let request = Request::post(format!("http://{address}/v1/responses"))
        .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
        .header(header::CONTENT_TYPE, "application/json")
        .body(())
        .unwrap();
    let (response, mut request_stream) = client.send_request(request, false).unwrap();
    request_stream
        .send_data(
            serde_json::to_vec(
                &json!({"model": fixture.model, "input": "cancel over real HTTP2", "stream": true}),
            )
            .unwrap()
            .into(),
            true,
        )
        .unwrap();
    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.version(), http::Version::HTTP_2);
    let request_id = response.headers()[REQUEST_ID_HEADER]
        .to_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let mut body = response.into_body();
    let mut rendered = String::new();
    while !rendered.contains("delivered before cancellation") {
        let frame = body
            .data()
            .await
            .expect("output before cancellation")
            .unwrap();
        body.flow_control().release_capacity(frame.len()).unwrap();
        rendered.push_str(std::str::from_utf8(&frame).unwrap());
    }
    assert!(rendered.contains("response.created"));
    assert!(!rendered.contains("response.completed"));
    assert!(!rendered.contains("[DONE]"));
    assert!(fixture.state.proxy_lifecycle_permits.try_acquire().is_err());
    assert!(matches!(
        fixture
            .state
            .codex_clients
            .acquire_dispatch(&route, &fixture.state.metrics)
            .await,
        Err(crate::codex_clients::DispatchError::Capacity)
    ));
    if reset_stream {
        request_stream.send_reset(http2::Reason::CANCEL);
        drop(body);
        assert!(
            !connection.is_finished(),
            "RST_STREAM must not require dropping the connection"
        );
    } else {
        connection.abort();
        drop(body);
    }
    let reason = tokio::time::timeout(Duration::from_secs(3), upstream.resets.recv())
        .await
        .expect("downstream cancellation must cancel the silent upstream immediately")
        .unwrap();
    assert_eq!(reason, http2::Reason::CANCEL);
    let _lifecycle = tokio::time::timeout(
        Duration::from_secs(3),
        fixture.state.proxy_lifecycle_permits.acquire(),
    )
    .await
    .expect("lifecycle permit must be returned without upstream EOF")
    .unwrap();
    let _dispatch = fixture
        .state
        .codex_clients
        .acquire_dispatch(&route, &fixture.state.metrics)
        .await
        .expect("dispatch permit must be returned without upstream EOF");
    wait_for_request_settlement(&fixture, 1).await;
    upstream.finish().await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].request_id, request_id);
    assert_eq!(rows[0].status_code, Some(499));
    assert_eq!(rows[0].error_code.as_deref(), Some("client_cancelled"));
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (0, 0));
    assert_eq!(rows[0].cost, "0");
    assert_eq!(
        rows[0].usage_basis,
        Some(crate::model::RequestUsageBasis::NotObserved)
    );
    assert_exactly_once_side_effects(&fixture, request_id, None).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let failures: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_account_health WHERE upstream_account_id = $1 AND consecutive_failures > 0",
    ).bind(fixture.upstream_account_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(
        failures, 0,
        "client cancellation must not poison upstream health"
    );
    pool.close().await;
    if reset_stream {
        assert!(
            !connection.is_finished(),
            "cancellation must settle while the downstream HTTP/2 connection is alive"
        );
        connection.abort();
    }
    assert!(connection.await.unwrap_err().is_cancelled());
    drop(client);
    drop(request_stream);
    stop_gateway.send(()).unwrap();
    gateway.await.unwrap().unwrap();
    std::io::Write::write_all(
        &mut std::io::stdout(),
        format!(
            "MTC_CONTROLLED_HTTP2_CANCEL {}\n",
            json!({
                "case": label, "posts": 1, "upstream_reset": "CANCEL", "settlements": 1,
                "status": 499, "error_code": "client_cancelled", "lifecycle_permit_released": true,
                "dispatch_permit_released": true, "upstream_failures": 0
            })
        )
        .as_bytes(),
    )
    .unwrap();
}

#[tokio::test]
async fn real_http2_downstream_reset_and_tcp_close_cancel_once_and_release_permits() {
    for reset_stream in [true, false] {
        tokio::time::timeout(
            Duration::from_secs(15),
            assert_network_cancellation(reset_stream),
        )
        .await
        .expect("real HTTP/2 cancellation must not wait for a provider timeout");
    }
}
