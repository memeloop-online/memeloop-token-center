use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[tokio::test]
async fn wan_submit_refused_stream_disables_the_library_replay_that_default_client_performs() {
    for no_retry in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/api/v1/services/aigc/video-generation/video-synthesis",
            listener.local_addr().unwrap()
        );
        let received = Arc::new(AtomicUsize::new(0));
        let server_received = received.clone();
        let (shutdown, mut stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let received = server_received.clone();
                        connections.spawn(async move {
                            let mut connection = http2::server::handshake(stream).await.unwrap();
                            let mut held_requests = Vec::new();
                            while let Some(accepted) = connection.accept().await {
                                let Ok((request, mut response)) = accepted else { break };
                                assert_eq!(request.version(), http::Version::HTTP_2);
                                assert_eq!(request.method(), http::Method::POST);
                                assert_eq!(request.uri().path(), "/api/v1/services/aigc/video-generation/video-synthesis");
                                if received.fetch_add(1, Ordering::SeqCst) == 0 {
                                    response.send_reset(http2::Reason::REFUSED_STREAM);
                                } else {
                                    response.send_response(http::Response::builder().status(200).body(()).unwrap(), true).unwrap();
                                }
                                held_requests.push(request);
                            }
                        });
                    }
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        let shared = crate::build_http_client().unwrap();
        let config = json!({"network_scope": "private"});
        let client = if no_retry {
            submit_http(&shared, &endpoint, &config, None, true)
                .await
                .unwrap()
        } else {
            crate::network::client_for_config_url(&shared, &endpoint, &config, None, true)
                .await
                .unwrap()
        };
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client
                .post(endpoint)
                .version(reqwest::Version::HTTP_2)
                .json(&json!({
                    "model": "wan3.0-video", "input": {"prompt": "synthetic retry contract"}
                }))
                .send(),
        )
        .await;
        let _ = shutdown.send(());
        server.await.unwrap();
        let result = result.expect("the HTTP/2 reset must terminate within the fixture deadline");
        if no_retry {
            assert!(result.is_err());
            assert_eq!(received.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(result.unwrap().status(), reqwest::StatusCode::OK);
            assert_eq!(received.load(Ordering::SeqCst), 2);
        }
    }
}
