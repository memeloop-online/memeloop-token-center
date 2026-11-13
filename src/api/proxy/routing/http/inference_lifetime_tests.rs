use super::*;
use futures_util::StreamExt;
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn route(base_url: String, streaming: bool) -> PreparedProxyRoute {
    PreparedProxyRoute {
        request_workspace: None,
        inspect_stream_rejection: false,
        route: ResolvedUpstream {
            route_id: Uuid::now_v7(),
            account_id: Uuid::now_v7(),
            transport_revision: 1,
            credential_generation: 1,
            driver: "http-json".into(),
            base_url,
            config: serde_json::json!({"timeout_seconds": 600}),
            upstream_model: "long-review".into(),
            credential: UpstreamCredential::None,
        },
        forwarded_body: Bytes::from_static(b"{}"),
        component_input_bytes: 0,
        upstream_stream: streaming,
        codex_downstream_stream: false,
        codex_store_disabled: false,
        codex_session_id: None,
        component_request: None,
        responses_chat: None,
        upstream_path: "/v1/chat/completions",
        wait_for_compaction: false,
        compact_v2_bridge: false,
        wrap_compact_as_sse: false,
        responses_anthropic: None,
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) {
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n{}") {
        let mut bytes = [0; 4096];
        let count = socket.read(&mut bytes).await.unwrap();
        assert!(count > 0);
        request.extend_from_slice(&bytes[..count]);
    }
}

async fn read_body(response: UpstreamResponse) -> Result<Bytes, &'static str> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk?);
    }
    Ok(Bytes::from(bytes))
}

fn settings() -> Arc<crate::http_settings::HttpClientFactory> {
    let factory = Arc::new(crate::http_settings::HttpClientFactory::default());
    factory.publish(
        serde_json::from_value(serde_json::json!({
            "revision": 1, "settings": {"defaults": {"request_timeout_ms": 600000}}
        }))
        .unwrap(),
    );
    factory
}

async fn advance_past_legacy_deadline() {
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(601)).await;
    tokio::task::yield_now().await;
    tokio::time::resume();
}

#[tokio::test]
async fn inference_legacy_600_seconds_never_limits_headers_or_buffered_body() {
    let (mut state, _directory) = crate::api::tests::test_state().await;
    state.http = settings().handle(crate::http_settings::HttpPurpose::General);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let route = route(format!("http://{}", listener.local_addr().unwrap()), false);
    let request = tokio::spawn(async move {
        send_reqwest_proxy_route(
            &state,
            &HeaderMap::new(),
            Protocol::OpenAiChat,
            Uuid::now_v7(),
            &route,
            1,
        )
        .await
    });
    let (mut peer, _) = listener.accept().await.unwrap();
    read_request(&mut peer).await;
    advance_past_legacy_deadline().await;
    assert!(
        !request.is_finished(),
        "neither legacy account nor shared deadline may stop headers"
    );
    peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let response = request.await.unwrap().unwrap();
    let body = tokio::spawn(async move { read_body(response.response).await });
    advance_past_legacy_deadline().await;
    assert!(
        !body.is_finished(),
        "buffered response keeps waiting after the old absolute/read windows"
    );
    peer.write_all(b"ok").await.unwrap();
    assert_eq!(body.await.unwrap().unwrap(), "ok");
}

#[tokio::test]
async fn inference_stream_has_no_absolute_or_inherited_idle_600_second_window() {
    let (mut state, _directory) = crate::api::tests::test_state().await;
    state.http = settings().handle(crate::http_settings::HttpPurpose::General);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let route = route(format!("http://{}", listener.local_addr().unwrap()), true);
    let request = tokio::spawn(async move {
        send_reqwest_proxy_route(
            &state,
            &HeaderMap::new(),
            Protocol::OpenAiChat,
            Uuid::now_v7(),
            &route,
            1,
        )
        .await
    });
    let (mut peer, _) = listener.accept().await.unwrap();
    read_request(&mut peer).await;
    peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n").await.unwrap();
    let response = request.await.unwrap().unwrap();
    let mut stream = response.response.bytes_stream();
    assert_eq!(stream.next().await.unwrap().unwrap(), "a");
    let next = tokio::spawn(async move {
        let item = stream.next().await;
        (stream, item)
    });
    advance_past_legacy_deadline().await;
    assert!(!next.is_finished());
    peer.write_all(b"1\r\nb\r\n").await.unwrap();
    let (mut stream, item) = next.await.unwrap();
    assert_eq!(item.unwrap().unwrap(), "b");
    advance_past_legacy_deadline().await;
    peer.write_all(b"0\r\n\r\n").await.unwrap();
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn inference_caller_cancellation_closes_headers_and_body_sockets_without_replay() {
    for after_headers in [false, true] {
        let (mut state, _directory) = crate::api::tests::test_state().await;
        state.http = settings().handle(crate::http_settings::HttpPurpose::General);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let route = route(format!("http://{}", listener.local_addr().unwrap()), true);
        let request = tokio::spawn(async move {
            send_reqwest_proxy_route(
                &state,
                &HeaderMap::new(),
                Protocol::OpenAiChat,
                Uuid::now_v7(),
                &route,
                1,
            )
            .await
        });
        let (mut peer, _) = listener.accept().await.unwrap();
        read_request(&mut peer).await;
        if after_headers {
            peer.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .unwrap();
            let response = request.await.unwrap().unwrap();
            let (entered, ready) = tokio::sync::oneshot::channel();
            let reader = tokio::spawn(async move {
                let mut stream = response.response.bytes_stream();
                entered.send(()).unwrap();
                stream.next().await
            });
            ready.await.unwrap();
            advance_past_legacy_deadline().await;
            assert!(!reader.is_finished());
            reader.abort();
            assert!(reader.await.unwrap_err().is_cancelled());
        } else {
            advance_past_legacy_deadline().await;
            assert!(!request.is_finished());
            request.abort();
            assert!(request.await.is_err_and(|error| error.is_cancelled()));
        }
        let mut byte = [0];
        assert_eq!(
            peer.read(&mut byte).await.unwrap(),
            0,
            "cancellation closes the actual outbound socket"
        );
        let accept = listener.accept();
        tokio::pin!(accept);
        assert!(futures_util::poll!(accept).is_pending());
    }
}

#[tokio::test]
async fn inference_real_truncated_body_is_still_an_error_and_never_replayed() {
    let (mut state, _directory) = crate::api::tests::test_state().await;
    state.http = settings().handle(crate::http_settings::HttpPurpose::General);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let route = route(format!("http://{}", listener.local_addr().unwrap()), false);
    let request = tokio::spawn(async move {
        send_reqwest_proxy_route(
            &state,
            &HeaderMap::new(),
            Protocol::OpenAiChat,
            Uuid::now_v7(),
            &route,
            1,
        )
        .await
    });
    let (mut peer, _) = listener.accept().await.unwrap();
    read_request(&mut peer).await;
    peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nshort")
        .await
        .unwrap();
    drop(peer);
    let response = request.await.unwrap().unwrap();
    assert!(read_body(response.response).await.is_err());
    let accept = listener.accept();
    tokio::pin!(accept);
    assert!(futures_util::poll!(accept).is_pending());
}
