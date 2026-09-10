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
async fn validated_delivered_probe_opens_concurrent_admission_before_long_stream_eof() {
    let fixture = codex_route_fixture("delivered-long-probe").await;
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
            chunk(&mut first, complete[..output_at].as_bytes()).await;
            output_rx.await.unwrap();
            chunk(&mut first, complete[output_at..terminal_at].as_bytes()).await;
            eof_rx.await.unwrap();
            chunk(&mut first, complete[terminal_at..].as_bytes()).await;
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
