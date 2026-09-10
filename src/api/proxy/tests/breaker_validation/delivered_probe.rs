use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn begin_response(socket: &mut tokio::net::TcpStream) {
    let mut request = [0_u8; 4096];
    assert!(socket.read(&mut request).await.unwrap() > 0);
    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
}

async fn chunk(socket: &mut tokio::net::TcpStream, bytes: &[u8]) {
    socket
        .write_all(format!("{:X}\r\n", bytes.len()).as_bytes())
        .await
        .unwrap();
    socket.write_all(bytes).await.unwrap();
    socket.write_all(b"\r\n").await.unwrap();
    socket.flush().await.unwrap();
}

#[tokio::test]
async fn malformed_strict_chat_output_never_releases_half_open_admission() {
    for payload in [
        b"data: not-json\n\n".as_slice(),
        b"data: {\"id\":\"chatcmpl-invalid\",\"object\":\"wrong-envelope\",\"model\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"bad\"}}]}\n\n".as_slice(),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            begin_response(&mut socket).await;
            chunk(&mut socket, payload).await;
            released.await.unwrap();
            socket.write_all(b"0\r\n\r\n").await.unwrap();
        });
        let mut fixture =
            response_usage_fixture_with_uri("invalid-probe-prefix", endpoint, 0).await;
        std::sync::Arc::make_mut(&mut fixture.state.config)
            .upstream_health
            .shared_probe_attempts = 0;
        make_account_half_open_probe(&fixture).await;
        let body = json!({"model": fixture.model, "messages": [{"role":"user","content":"fixture"}],
            "stream": true, "stream_options": {"include_usage": true}, "max_tokens":16});
        let first = send_chat_usage_request(&fixture, &body).await;
        assert_eq!(first.status(), StatusCode::OK);
        let mut stream = first.into_body().into_data_stream();
        let frame = tokio::time::timeout(Duration::from_secs(3), futures_util::StreamExt::next(&mut stream))
            .await.unwrap().unwrap().unwrap();
        assert!(!frame.is_empty());
        let second = tokio::time::timeout(Duration::from_secs(3), send_chat_usage_request(&fixture, &body))
            .await.expect("invalid prefix must not dispatch another upstream POST");
        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
        let _ = to_bytes(second.into_body(), MAX_PROXY_RESPONSE_BODY).await.unwrap();
        assert!(!fixture.state.metrics.render(&crate::metrics::RuntimeMetrics::default()).contains("event=\"recovered\""));
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(frame) = futures_util::StreamExt::next(&mut stream).await {
                frame.unwrap();
            }
        }).await.expect("malformed strict Chat EOF must close the downstream body");
        server.await.unwrap();
        // The rejected probe contender is admitted to the request ledger before
        // account-lease selection, so its 503 is a second terminal record.
        wait_for_request_settlement(&fixture, 2).await;
        let rows = fixture.state.db.list_requests(fixture.key_id, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.completed_at.is_some()));
        let malformed = rows.iter().find(|row| row.status_code == Some(502))
            .expect("malformed stream must finish with a protocol failure");
        assert_eq!(malformed.error_code.as_deref(), Some("upstream_incomplete_response"));
        let rejected = rows.iter().find(|row| row.status_code == Some(503))
            .expect("the concurrent request must remain rejected");
        assert_eq!(rejected.error_code.as_deref(), Some("upstream_unavailable"));
        assert_eq!((rejected.input_tokens, rejected.output_tokens), (0, 0));
        wait_for_account_failure_count(&fixture, 2).await;
    }
}

#[tokio::test]
async fn completed_only_codex_with_invalid_usage_never_records_recovery() {
    for usage in [
        None,
        Some(json!(null)),
        Some(json!({"input_tokens": -1, "output_tokens": 2, "total_tokens": 1})),
    ] {
        let fixture = codex_route_fixture("invalid-terminal-probe").await;
        make_account_half_open_probe(&fixture).await;
        let upstream = MockServer::start().await;
        let mut response = json!({"id":"resp-invalid-usage","object":"response","output":[
            {"id":"item","type":"message","role":"assistant","content":[{"type":"output_text","text":"terminal only"}]}
        ]});
        if let Some(usage) = usage {
            response["usage"] = usage;
        }
        let payload = format!(
            "event: response.completed\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"type":"response.completed", "response":response})
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(payload, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;
        let first = send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model":fixture.model,"input":"invalid terminal usage","stream":true}),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let _ = to_bytes(first.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        wait_for_request_settlement(&fixture, 1).await;
        wait_for_account_failure_count(&fixture, 2).await;
        assert!(
            !fixture
                .state
                .metrics
                .render(&crate::metrics::RuntimeMetrics::default())
                .contains("event=\"recovered\"")
        );
        let second = send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model":fixture.model,"input":"still unhealthy","stream":true}),
        )
        .await;
        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
        let _ = to_bytes(second.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        upstream.verify().await;
    }
}

#[tokio::test]
async fn sole_half_open_account_retains_bounded_capacity_while_primary_probe_is_slow() {
    let mut fixture = codex_route_fixture("bounded-shared-probe").await;
    std::sync::Arc::make_mut(&mut fixture.state.config)
        .upstream_health
        .shared_probe_attempts = 1;
    make_account_half_open_probe_with_kind(&fixture, UpstreamFailureKind::Connection).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (release, released) = tokio::sync::watch::channel(false);
    let upstream = tokio::spawn(async move {
        let mut handlers = Vec::new();
        for index in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut released = released.clone();
            handlers.push(tokio::spawn(async move {
                begin_response(&mut socket).await;
                chunk(
                    &mut socket,
                    format!(
                        "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-shared-{index}\"}}}}\n\n"
                    )
                    .as_bytes(),
                )
                .await;
                while !*released.borrow() {
                    released.changed().await.unwrap();
                }
                chunk(
                    &mut socket,
                    completed_codex_sse(&format!("shared probe {index}")).as_bytes(),
                )
                .await;
                socket.write_all(b"0\r\n\r\n").await.unwrap();
            }));
        }
        for handler in handlers {
            handler.await.unwrap();
        }
    });

    let first = send_codex_route_to_endpoint(
        &fixture,
        endpoint.clone(),
        "/v1/responses",
        json!({"model": fixture.model, "input": "primary slow probe", "stream": true}),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let second = send_codex_route_to_endpoint(
        &fixture,
        endpoint.clone(),
        "/v1/responses",
        json!({"model": fixture.model, "input": "bounded shared probe", "stream": true}),
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let denied = send_codex_route_to_endpoint(
        &fixture,
        endpoint,
        "/v1/responses",
        json!({"model": fixture.model, "input": "beyond shared bound", "stream": true}),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::SERVICE_UNAVAILABLE);
    let _ = to_bytes(denied.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();

    release.send(true).unwrap();
    let (first_body, second_body) = tokio::join!(
        to_bytes(first.into_body(), MAX_PROXY_RESPONSE_BODY),
        to_bytes(second.into_body(), MAX_PROXY_RESPONSE_BODY),
    );
    assert!(first_body.is_ok());
    assert!(second_body.is_ok());
    tokio::time::timeout(Duration::from_secs(3), upstream)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn validated_delivered_probe_opens_concurrent_admission_before_long_stream_eof() {
    let mut fixture = codex_route_fixture("delivered-long-probe").await;
    std::sync::Arc::make_mut(&mut fixture.state.config)
        .upstream_health
        .shared_probe_attempts = 0;
    make_account_half_open_probe(&fixture).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (output_tx, output_rx) = tokio::sync::oneshot::channel();
    let (eof_tx, eof_rx) = tokio::sync::oneshot::channel();
    let upstream = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let first_task = tokio::spawn(async move {
            begin_response(&mut first).await;
            let complete = completed_codex_sse("long probe output");
            let output_at = complete.find("event: response.output_item.done").unwrap();
            let terminal_at = complete.find("event: response.completed").unwrap();
            chunk(&mut first, &complete.as_bytes()[..output_at]).await;
            output_rx.await.unwrap();
            chunk(&mut first, &complete.as_bytes()[output_at..terminal_at]).await;
            eof_rx.await.unwrap();
            chunk(&mut first, &complete.as_bytes()[terminal_at..]).await;
            first.write_all(b"0\r\n\r\n").await.unwrap();
        });
        // Only two actual POSTs are permitted. The rejected request below must
        // never reach this listener while headers/control are the only proof.
        let (mut second, _) = listener.accept().await.unwrap();
        begin_response(&mut second).await;
        chunk(
            &mut second,
            completed_codex_sse("parallel output").as_bytes(),
        )
        .await;
        second.write_all(b"0\r\n\r\n").await.unwrap();
        first_task.await.unwrap();
    });
    let first = send_codex_route_to_endpoint(
        &fixture,
        endpoint.clone(),
        "/v1/responses",
        json!({"model": fixture.model, "input": "long probe", "stream": true}),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let mut first_body = first.into_body().into_data_stream();
    let initial = tokio::time::timeout(
        Duration::from_secs(3),
        futures_util::StreamExt::next(&mut first_body),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert!(!String::from_utf8_lossy(&initial).contains("long probe output"));
    let denied = send_codex_route_to_endpoint(
        &fixture,
        endpoint.clone(),
        "/v1/responses",
        json!({"model": fixture.model, "input": "headers are not recovery", "stream": true}),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::SERVICE_UNAVAILABLE);
    let _ = to_bytes(denied.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    output_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let frame = futures_util::StreamExt::next(&mut first_body)
                .await
                .unwrap()
                .unwrap();
            if String::from_utf8_lossy(&frame).contains("long probe output") {
                break;
            }
        }
    })
    .await
    .unwrap();
    wait_for_account_failure_count(&fixture, 0).await;
    assert!(
        !upstream.is_finished(),
        "first upstream is still waiting for its EOF gate"
    );
    let parallel = send_codex_route_to_endpoint(
        &fixture,
        endpoint,
        "/v1/responses",
        json!({"model": fixture.model, "input": "new independent request", "stream": true}),
    )
    .await;
    assert_eq!(parallel.status(), StatusCode::OK);
    let parallel_body = tokio::time::timeout(
        Duration::from_secs(3),
        to_bytes(parallel.into_body(), MAX_PROXY_RESPONSE_BODY),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(String::from_utf8_lossy(&parallel_body).contains("parallel output"));
    assert!(
        !upstream.is_finished(),
        "concurrent request completes before first stream terminal"
    );
    eof_tx.send(()).unwrap();
    while let Some(frame) = futures_util::StreamExt::next(&mut first_body).await {
        frame.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(3), upstream)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let rows = fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap();
            if rows
                .iter()
                .filter(|row| row.status_code == Some(200))
                .count()
                == 2
            {
                for row in rows.iter().filter(|row| row.status_code == Some(200)) {
                    assert_eq!((row.input_tokens, row.output_tokens), (3, 2));
                    assert_eq!(row.cost, "0.000005");
                    assert_exactly_once_side_effects(&fixture, row.request_id, Some("resp-codex"))
                        .await;
                }
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
