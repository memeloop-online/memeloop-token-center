use super::*;
use futures_util::StreamExt;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

const CREATED: &[u8] = b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-usage-contract\"}}\n\n";
const PROGRESS: &str = "event: response.in_progress";
const HEARTBEAT_BOUND: Duration = Duration::from_secs(25);
const SILENCE: Duration = Duration::from_secs(310);

struct AcceptedPost {
    connection_id: usize,
    stream_id: u32,
    created_at: Instant,
    stream: http2::SendStream<bytes::Bytes>,
}

impl AcceptedPost {
    fn complete(&mut self) {
        let completed = completed_response_with_usage(3, 7);
        let terminal = format!(
            "event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{completed}}}\n\ndata: [DONE]\n\n"
        );
        self.stream.send_data(terminal.into(), true).unwrap();
    }
}

struct QuietUpstream {
    endpoint: String,
    connections: Arc<AtomicUsize>,
    posts: Arc<AtomicUsize>,
    accepted: tokio::sync::mpsc::UnboundedReceiver<AcceptedPost>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for QuietUpstream {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl QuietUpstream {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let posts = Arc::new(AtomicUsize::new(0));
        let (sender, accepted) = tokio::sync::mpsc::unbounded_channel();
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let connection_count = connections.clone();
        let post_count = posts.clone();
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
                        let connection_id = connection_count.fetch_add(1, Ordering::SeqCst) + 1;
                        let post_count = post_count.clone();
                        let sender = sender.clone();
                        tasks.spawn(async move {
                            let mut connection = http2::server::handshake(stream).await.unwrap();
                            let mut held_requests = Vec::new();
                            while let Some(accepted) = connection.accept().await {
                                let Ok((request, mut response)) = accepted else { break };
                                assert_eq!(request.version(), http::Version::HTTP_2);
                                assert_eq!(request.method(), http::Method::POST);
                                assert_eq!(request.uri().path(), codex_transport::RESPONSES_PATH);
                                post_count.fetch_add(1, Ordering::SeqCst);
                                let headers = http::Response::builder()
                                    .status(200)
                                    .header(http::header::CONTENT_TYPE, "text/event-stream")
                                    .body(())
                                    .unwrap();
                                let mut stream = response.send_response(headers, false).unwrap();
                                stream.send_data(bytes::Bytes::from_static(CREATED), false).unwrap();
                                let post = AcceptedPost {
                                    connection_id,
                                    stream_id: stream.stream_id().as_u32(),
                                    created_at: Instant::now(),
                                    stream,
                                };
                                assert!(sender.send(post).is_ok());
                                held_requests.push(request);
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
            connections,
            posts,
            accepted,
            stop: Some(stop),
            server,
        }
    }

    async fn finish(&mut self, expected_posts: usize, expected_connections: usize) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            self.posts.load(Ordering::SeqCst),
            expected_posts,
            "POST replay"
        );
        assert_eq!(
            self.connections.load(Ordering::SeqCst),
            expected_connections
        );
        assert!(self.accepted.try_recv().is_err(), "unexpected extra POST");
        self.stop.take().unwrap().send(()).unwrap();
        (&mut self.server).await.unwrap();
    }
}

async fn fixture(label: &str) -> CodexRouteFixture {
    let fixture = codex_route_fixture(label).await;
    fixture.state.codex_clients.install_test_client(
        crate::build_codex_http2_test_client(
            crate::provider::CodexTransportPolicy::default(),
            crate::CODEX_HTTP2_KEEP_ALIVE_INTERVAL,
        )
        .unwrap(),
    );
    fixture
}

async fn start_post(
    fixture: &CodexRouteFixture,
    upstream: &mut QuietUpstream,
) -> (Body, AcceptedPost) {
    let response = send_codex_route_to_endpoint(
        fixture,
        upstream.endpoint.clone(),
        "/v1/responses",
        json!({"model": fixture.model, "input": "controlled HTTP/2 silence", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let accepted = upstream.accepted.recv().await.unwrap();
    (response.into_body(), accepted)
}

fn assert_terminal(rendered: &str) {
    assert_eq!(rendered.matches("event: response.created").count(), 1);
    assert_eq!(rendered.matches("event: response.completed").count(), 1);
    assert_eq!(rendered.matches("data: [DONE]").count(), 1);
    assert!(!rendered.contains("response.failed"));
    assert!(!rendered.contains("response.incomplete"));
    assert!(!rendered.contains("event: error"));
}

async fn assert_settled(fixture: &CodexRouteFixture, count: usize) {
    wait_for_request_settlement(fixture, count).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), count);
    for row in rows {
        assert_eq!(row.status_code, Some(200));
        assert_eq!(row.error_code, None);
        assert_eq!((row.input_tokens, row.output_tokens), (3, 7));
        assert_exactly_once_side_effects(fixture, row.request_id, Some("resp-usage-contract"))
            .await;
    }
}

#[tokio::test]
async fn created_then_310_seconds_of_real_http2_silence_keeps_progress_and_settles_once() {
    tokio::time::timeout(Duration::from_secs(360), async {
        let fixture = fixture("h2-310-second-silence").await;
        let mut upstream = QuietUpstream::start().await;
        let (body, mut post) = start_post(&fixture, &mut upstream).await;
        let mut body = body.into_data_stream();
        let mut rendered = String::new();
        tokio::time::timeout(HEARTBEAT_BOUND, async {
            while !rendered.contains("event: response.created\n") || !rendered.ends_with("\n\n") {
                let frame = body.next().await.expect("created before EOF").unwrap();
                rendered.push_str(std::str::from_utf8(&frame).unwrap());
            }
        }).await.expect("created must be delivered before the silence window");
        let observed_created = Instant::now();
        let end = tokio::time::Instant::now() + SILENCE;
        let mut last_progress = Instant::now();
        let mut progress_count = rendered.matches(PROGRESS).count();
        let initial_progress = progress_count;
        let mut max_gap = Duration::ZERO;
        loop {
            let next_progress = async {
                loop {
                    let frame = body.next().await.expect("quiet stream must stay open").unwrap();
                    rendered.push_str(std::str::from_utf8(&frame).unwrap());
                    assert!(!rendered.contains("response.completed"));
                    assert!(!rendered.contains("response.failed"));
                    assert!(!rendered.contains("data: [DONE]"));
                    let count = rendered.matches(PROGRESS).count();
                    if count > progress_count { return count; }
                }
            };
            tokio::select! {
                _ = tokio::time::sleep_until(end) => break,
                count = tokio::time::timeout(HEARTBEAT_BOUND, next_progress) => {
                    progress_count = count.expect("continuous downstream progress during upstream DATA silence");
                    let gap = last_progress.elapsed();
                    assert!(gap <= HEARTBEAT_BOUND, "progress gap: {gap:?}");
                    max_gap = max_gap.max(gap);
                    last_progress = Instant::now();
                    let rows = fixture.state.db.list_requests(fixture.key_id, 10).await.unwrap();
                    assert_eq!(rows.len(), 1);
                    assert_eq!(rows[0].status_code, None, "must not settle during silence");
                    assert_eq!(upstream.posts.load(Ordering::SeqCst), 1);
                }
            }
        }
        assert!(observed_created.elapsed() >= SILENCE);
        assert!(post.created_at.elapsed() >= SILENCE);
        assert!(last_progress.elapsed() <= HEARTBEAT_BOUND);
        assert!(progress_count - initial_progress >= 18, "heartbeats must span the whole five minutes");
        post.complete();
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(frame) = body.next().await {
                rendered.push_str(std::str::from_utf8(&frame.unwrap()).unwrap());
            }
        }).await.expect("one terminal followed by EOF");
        assert_terminal(&rendered);
        upstream.finish(1, 1).await;
        assert_settled(&fixture, 1).await;
        println!("controlled_h2_silence wall_seconds={} progress_events={} max_progress_gap_ms={} posts=1 connections=1 terminals=1 settlements=1", post.created_at.elapsed().as_secs_f64(), progress_count - initial_progress, max_gap.as_millis());
    }).await.expect("real five-minute HTTP/2 acceptance must finish within six minutes");
}

async fn complete_post(fixture: &CodexRouteFixture, upstream: &mut QuietUpstream) -> (usize, u32) {
    let (body, mut post) = start_post(fixture, upstream).await;
    post.complete();
    let bytes = to_bytes(body, MAX_PROXY_RESPONSE_BODY).await.unwrap();
    assert_terminal(std::str::from_utf8(&bytes).unwrap());
    (post.connection_id, post.stream_id)
}

#[tokio::test]
async fn same_http2_client_reuses_short_idle_pool_and_reconnects_after_95_seconds() {
    tokio::time::timeout(Duration::from_secs(130), async {
        let fixture = fixture("h2-pool-real-idle").await;
        let mut upstream = QuietUpstream::start().await;
        let first = complete_post(&fixture, &mut upstream).await;
        assert_settled(&fixture, 1).await;
        let short_idle = Instant::now();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let second = complete_post(&fixture, &mut upstream).await;
        assert!(short_idle.elapsed() >= Duration::from_secs(2));
        assert_eq!(first.0, second.0, "same client must reuse its short-idle connection");
        assert!(second.1 > first.1, "reuse must open a new HTTP/2 stream");
        assert_eq!(upstream.connections.load(Ordering::SeqCst), 1);
        assert_settled(&fixture, 2).await;
        let long_idle = Instant::now();
        tokio::time::sleep(Duration::from_secs(95)).await;
        assert!(long_idle.elapsed() > Duration::from_secs(90));
        let third = complete_post(&fixture, &mut upstream).await;
        assert_ne!(second.0, third.0, "expired pool entry must create a fresh TCP/HTTP2 connection");
        upstream.finish(3, 2).await;
        assert_settled(&fixture, 3).await;
        println!("controlled_h2_pool long_idle_seconds={} connection_stream_ids={first:?},{second:?},{third:?} posts=3 connections=2 terminals=3 settlements=3", long_idle.elapsed().as_secs_f64());
    }).await.expect("real idle-pool acceptance must finish within 130 seconds");
}
