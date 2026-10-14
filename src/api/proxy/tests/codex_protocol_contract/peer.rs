use super::*;
use crate::http2_upload_tests::{accept_socks_for_host, tls_fixture::TlsFixture};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::convert::Infallible;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const HOST: &str = "localhost";

struct OutputStream {
    receiver: ReceiverStream<Result<bytes::Bytes, Infallible>>,
    cancelled: mpsc::UnboundedSender<()>,
}

impl futures_util::Stream for OutputStream {
    type Item = Result<bytes::Bytes, Infallible>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.receiver).poll_next(context)
    }
}

impl Drop for OutputStream {
    fn drop(&mut self) {
        let _ = self.cancelled.send(());
    }
}

pub(super) struct Post {
    pub(super) body: Value,
    pub(super) output: mpsc::Sender<Result<bytes::Bytes, Infallible>>,
}

pub(super) struct Peer {
    pub(super) client: wreq::Client,
    pub(super) accepted: mpsc::UnboundedReceiver<Post>,
    pub(super) cancelled: mpsc::UnboundedReceiver<()>,
    pub(super) posts: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Peer {
    pub(super) async fn start(
        version: http::Version,
        policy: crate::provider::CodexTransportPolicy,
    ) -> Self {
        Self::with_builder(
            version,
            crate::codex_http_protocol_test_builder(
                policy,
                crate::CODEX_HTTP2_KEEP_ALIVE_INTERVAL,
                version,
            ),
        )
        .await
    }

    pub(super) async fn for_account(fixture: &CodexRouteFixture, version: http::Version) -> Self {
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        let config: Value = serde_json::from_str(
            &sqlx::query_scalar::<_, String>(
                "SELECT config_json FROM upstream_accounts WHERE id = $1",
            )
            .bind(fixture.upstream_account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap(),
        )
        .unwrap();
        pool.close().await;
        let policy =
            crate::provider::CodexTransportPolicy::parse(config.get("transport_policy")).unwrap();
        Self::with_builder(
            version,
            crate::codex_http_client_builder(policy, crate::CODEX_HTTP2_KEEP_ALIVE_INTERVAL),
        )
        .await
    }

    async fn with_builder(version: http::Version, builder: wreq::ClientBuilder) -> Self {
        let fixture = TlsFixture::for_host(HOST).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy =
            wreq::Proxy::all(format!("socks5h://{}", listener.local_addr().unwrap())).unwrap();
        let client = builder
            .tls_cert_store(fixture.roots)
            .proxy(proxy)
            .build()
            .unwrap();
        let (accepted_sender, accepted) = mpsc::unbounded_channel();
        let (cancel_sender, cancelled) = mpsc::unbounded_channel();
        let posts = Arc::new(AtomicUsize::new(0));
        let count = posts.clone();
        let server = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    result = connections.join_next(), if !connections.is_empty() => { result.unwrap().unwrap(); }
                    accepted = listener.accept() => {
                        let (mut socket, _) = accepted.unwrap();
                        let acceptor = fixture.acceptor.clone();
                        let count = count.clone();
                        let accepted_sender = accepted_sender.clone();
                        let cancel_sender = cancel_sender.clone();
                        connections.spawn(async move {
                            accept_socks_for_host(&mut socket, 443, HOST).await;
                            let encrypted = acceptor.accept(socket).await.unwrap();
                            let expected_alpn: &[u8] = if version == http::Version::HTTP_2 { b"h2" } else { b"http/1.1" };
                            assert_eq!(encrypted.get_ref().1.alpn_protocol(), Some(expected_alpn));
                            assert_eq!(encrypted.get_ref().1.server_name(), Some(HOST));
                            let service = service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                                let count = count.clone();
                                let accepted_sender = accepted_sender.clone();
                                let cancel_sender = cancel_sender.clone();
                                async move {
                                    assert_eq!(request.version(), version);
                                    assert_eq!(request.method(), http::Method::POST);
                                    assert_eq!(request.uri().path(), codex_transport::RESPONSES_PATH);
                                    assert_eq!(request.headers()["authorization"], "Bearer upstream-access-secret");
                                    let body = to_bytes(Body::new(request.into_body()), MAX_PROXY_RESPONSE_BODY).await.unwrap();
                                    let body = serde_json::from_slice(&body).unwrap();
                                    count.fetch_add(1, Ordering::SeqCst);
                                    let (output, receiver) = mpsc::channel(8);
                                    accepted_sender.send(Post { body, output }).unwrap_or_else(|_| panic!("test receiver dropped"));
                                    Ok::<_, Infallible>(http::Response::builder().status(200)
                                        .header(header::CONTENT_TYPE, "text/event-stream")
                                        .body(Body::from_stream(OutputStream { receiver: ReceiverStream::new(receiver), cancelled: cancel_sender })).unwrap())
                                }
                            });
                            if version == http::Version::HTTP_2 {
                                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                                    .serve_connection(TokioIo::new(encrypted), service).await;
                            } else {
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(TokioIo::new(encrypted), service).await;
                            }
                        });
                    }
                }
            }
        });
        Self {
            client,
            accepted,
            cancelled,
            posts,
            server,
        }
    }

    pub(super) async fn post(&mut self) -> Post {
        tokio::time::timeout(Duration::from_secs(3), self.accepted.recv())
            .await
            .expect("native POST must reach TLS/SOCKS peer")
            .unwrap()
    }

    pub(super) fn endpoint(&self) -> String {
        format!("https://{HOST}")
    }
}
