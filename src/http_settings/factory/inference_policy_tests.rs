use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn request_headers(peer: &mut tokio::net::TcpStream) {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut chunk = [0; 4096];
        let count = peer.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
}

#[tokio::test]
async fn inference_total_deadline_policy_is_cache_isolated_from_control_requests() {
    let factory = Arc::new(HttpClientFactory::default());
    factory.publish(
        serde_json::from_value(json!({
            "revision": 1, "settings": {"defaults": {"request_timeout_ms": 600000}}
        }))
        .unwrap(),
    );
    let source = factory.handle(HttpPurpose::General);
    // Populate both cache entries in this order, then read each again. Reusing
    // either client's pool must not leak lifetime policy into the other.
    for inference in [true, false, true, false] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = source
            .http_client(ClientOptions {
                inference,
                no_retry: true,
                ..Default::default()
            })
            .unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let send = tokio::spawn(async move { client.get(url).send().await });
        let (mut peer, _) = listener.accept().await.unwrap();
        request_headers(&mut peer).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(601)).await;
        tokio::task::yield_now().await;
        tokio::time::resume();
        if inference {
            assert!(!send.is_finished());
            send.abort();
            assert!(send.await.unwrap_err().is_cancelled());
        } else {
            assert!(send.await.unwrap().unwrap_err().is_timeout());
        }
        assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
    }
    assert_eq!(factory.snapshot().clients.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn inference_preserves_an_explicit_read_inactivity_policy() {
    let factory = Arc::new(HttpClientFactory::default());
    factory.publish(
        serde_json::from_value(json!({
            "revision": 1, "settings": {"purposes": {"general": {
                "request_timeout_ms": 1000, "read_timeout_ms": 600000
            }}}
        }))
        .unwrap(),
    );
    let source = factory.handle(HttpPurpose::General);
    let client = inference(&source)
        .http_client(ClientOptions {
            no_retry: true,
            ..Default::default()
        })
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let send = tokio::spawn(async move { client.get(url).send().await });
    let (mut peer, _) = listener.accept().await.unwrap();
    request_headers(&mut peer).await;
    peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
        .await
        .unwrap();
    let response = send.await.unwrap().unwrap();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let read = tokio::spawn(async move {
        entered.send(()).unwrap();
        response.bytes().await
    });
    ready.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert!(
        !read.is_finished(),
        "shared total timeout does not override inference policy"
    );
    tokio::time::advance(Duration::from_secs(599)).await;
    tokio::time::resume();
    assert!(
        read.await.unwrap().unwrap_err().is_timeout(),
        "explicit read policy still applies"
    );
}
