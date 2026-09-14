use std::{
    collections::HashMap,
    future::Future,
    io,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::Router;
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto,
    service::TowerToHyperService,
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, watch},
    task::JoinSet,
    time::{Instant, MissedTickBehavior},
};

const MAX_CONNECTIONS: usize = 2_048;
// Behind the cluster ingress, many independent clients share one socket peer.
// Credential policy and ingress protections remain authoritative; this limit
// is only a final per-process connection safety bound.
const MAX_CONNECTIONS_PER_IP: usize = 512;
const MAX_HTTP1_HEADERS: usize = 64;
const MAX_HTTP1_BUFFER_BYTES: usize = 64 * 1024;
const MAX_HTTP2_CONCURRENT_STREAMS: u32 = 256;
const MAX_HTTP2_HEADER_LIST_BYTES: u32 = 64 * 1024;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_DRAIN_LOG_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
struct ServerLimits {
    max_connections: usize,
    max_connections_per_ip: usize,
    max_http1_headers: usize,
    max_http1_buffer_bytes: usize,
    max_http2_concurrent_streams: u32,
    max_http2_header_list_bytes: u32,
    header_read_timeout: Duration,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            max_connections: MAX_CONNECTIONS,
            max_connections_per_ip: MAX_CONNECTIONS_PER_IP,
            max_http1_headers: MAX_HTTP1_HEADERS,
            max_http1_buffer_bytes: MAX_HTTP1_BUFFER_BYTES,
            max_http2_concurrent_streams: MAX_HTTP2_CONCURRENT_STREAMS,
            max_http2_header_list_bytes: MAX_HTTP2_HEADER_LIST_BYTES,
            header_read_timeout: HEADER_READ_TIMEOUT,
        }
    }
}

struct PeerConnectionGuard {
    peer: IpAddr,
    peers: Arc<Mutex<HashMap<IpAddr, usize>>>,
    _global: tokio::sync::OwnedSemaphorePermit,
}

impl PeerConnectionGuard {
    fn try_new(
        peer: IpAddr,
        peers: Arc<Mutex<HashMap<IpAddr, usize>>>,
        global: tokio::sync::OwnedSemaphorePermit,
        per_ip_limit: usize,
    ) -> Option<Self> {
        let admitted = {
            let mut counts = peers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let count = counts.entry(peer).or_default();
            if *count >= per_ip_limit {
                false
            } else {
                *count += 1;
                true
            }
        };
        admitted.then_some(Self {
            peer,
            peers,
            _global: global,
        })
    }
}

impl Drop for PeerConnectionGuard {
    fn drop(&mut self) {
        let mut counts = self
            .peers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = counts.get_mut(&self.peer) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.peer);
            }
        }
    }
}

fn connection_builder(limits: ServerLimits) -> auto::Builder<TokioExecutor> {
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout)
        .max_headers(limits.max_http1_headers)
        .max_buf_size(limits.max_http1_buffer_bytes);
    builder
        .http2()
        .max_concurrent_streams(limits.max_http2_concurrent_streams)
        .max_header_list_size(limits.max_http2_header_list_bytes);
    builder
}

pub async fn serve<F>(listener: TcpListener, app: Router, shutdown: F) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    serve_with_limits(listener, app, shutdown, ServerLimits::default()).await
}

async fn serve_with_limits<F>(
    listener: TcpListener,
    app: Router,
    shutdown: F,
    limits: ServerLimits,
) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    let peers = Arc::new(Mutex::new(HashMap::new()));
    let (shutdown_sender, _) = watch::channel(false);
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if result.is_err() {
                    tracing::warn!(error_code = "connection_task_failed", "HTTP connection task stopped unexpectedly");
                }
            }
            permit = permits.clone().acquire_owned() => {
                let permit = permit.map_err(|_| io::Error::other("connection limiter closed"))?;
                let accepted = tokio::select! {
                    biased;
                    _ = &mut shutdown => {
                        drop(permit);
                        break;
                    }
                    accepted = listener.accept() => accepted,
                };
                let (stream, peer) = accepted?;
                let Some(guard) = PeerConnectionGuard::try_new(
                    peer.ip(),
                    peers.clone(),
                    permit,
                    limits.max_connections_per_ip,
                ) else {
                    drop(stream);
                    continue;
                };
                let service = TowerToHyperService::new(app.clone());
                let builder = connection_builder(limits);
                let mut connection_shutdown = shutdown_sender.subscribe();
                connections.spawn(async move {
                    let connection = builder.serve_connection_with_upgrades(
                        TokioIo::new(stream),
                        service,
                    );
                    tokio::pin!(connection);
                    let result = tokio::select! {
                        result = &mut connection => result,
                        changed = connection_shutdown.changed() => {
                            if changed.is_ok() {
                                connection.as_mut().graceful_shutdown();
                            }
                            connection.await
                        }
                    };
                    drop(guard);
                    if result.is_err() {
                        tracing::debug!(error_code = "connection_protocol_error", "HTTP connection closed");
                    }
                });
            }
        }
    }

    drain_connections(&mut connections, &shutdown_sender).await;
    Ok(())
}

async fn drain_connections(connections: &mut JoinSet<()>, shutdown: &watch::Sender<bool>) {
    let started = Instant::now();
    log_shutdown_drain("started", started, connections.len());
    let _ = shutdown.send(true);
    let mut progress = tokio::time::interval_at(
        started + SHUTDOWN_DRAIN_LOG_INTERVAL,
        SHUTDOWN_DRAIN_LOG_INTERVAL,
    );
    progress.set_missed_tick_behavior(MissedTickBehavior::Skip);
    while !connections.is_empty() {
        tokio::select! {
            biased;
            Some(result) = connections.join_next() => {
                if result.is_err() {
                    tracing::warn!(
                        error_code = "connection_task_failed",
                        "HTTP connection task stopped unexpectedly"
                    );
                }
            }
            _ = progress.tick() => {
                log_shutdown_drain("progress", started, connections.len());
            }
        }
    }
    log_shutdown_drain("completed", started, connections.len());
}

fn log_shutdown_drain(phase: &'static str, started: Instant, active_connections: usize) {
    // JoinSet counts remaining connection tasks, including completed tasks not
    // yet joined. It is not a count of model requests, HTTP/2 streams or database
    // sessions. No peer, request, credential or payload identifiers are logged.
    tracing::info!(
        event = "http_shutdown_drain",
        phase,
        active_connections,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "HTTP graceful shutdown drain"
    );
}

#[cfg(test)]
mod tests {
    use std::{io::Write, net::Ipv4Addr, sync::Arc};

    use axum::{Router, routing::get};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::{Semaphore, oneshot},
    };

    use super::*;

    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl Write for LogCapture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl LogCapture {
        fn fields(&self) -> Vec<serde_json::Value> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|line| {
                    serde_json::from_str::<serde_json::Value>(line).unwrap()["fields"].clone()
                })
                .collect()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_drain_reports_safe_counts_without_interrupting_connections() {
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(capture.clone())
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);
        let (shutdown, shutdown_received) = watch::channel(false);
        let (release, held_connection) = oneshot::channel();
        let mut connections = JoinSet::new();
        connections.spawn(async move {
            held_connection.await.unwrap();
        });
        let mut draining = Box::pin(drain_connections(&mut connections, &shutdown));
        assert!(futures_util::poll!(&mut draining).is_pending());
        assert!(*shutdown_received.borrow());
        assert_eq!(capture.fields().len(), 1);

        tokio::time::advance(Duration::from_secs(29)).await;
        assert!(futures_util::poll!(&mut draining).is_pending());
        assert_eq!(
            capture.fields().len(),
            1,
            "no immediate or early progress tick"
        );
        for advance in [1, 30, 120] {
            tokio::time::advance(Duration::from_secs(advance)).await;
            assert!(futures_util::poll!(&mut draining).is_pending());
        }
        assert_eq!(
            capture.fields().len(),
            4,
            "missed ticks must not create a log burst"
        );
        release.send(()).unwrap();
        draining.await;
        let expected = [
            ("started", 1, 0),
            ("progress", 1, 30_000),
            ("progress", 1, 60_000),
            ("progress", 1, 180_000),
            ("completed", 0, 180_000),
        ]
        .map(|(phase, active_connections, elapsed_ms)| {
            serde_json::json!({
                "message": "HTTP graceful shutdown drain",
                "event": "http_shutdown_drain",
                "phase": phase,
                "active_connections": active_connections,
                "elapsed_ms": elapsed_ms,
            })
        });
        assert_eq!(
            capture.fields(),
            expected,
            "only fixed safe fields may be emitted"
        );
    }

    fn test_limits() -> ServerLimits {
        ServerLimits {
            header_read_timeout: Duration::from_millis(100),
            ..ServerLimits::default()
        }
    }

    async fn start_server(
        limits: ServerLimits,
    ) -> (
        std::net::SocketAddr,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            serve_with_limits(
                listener,
                Router::new().route("/", get(|| async { "ok" })),
                async {
                    let _ = shutdown_receiver.await;
                },
                limits,
            )
            .await
            .unwrap();
        });
        (address, shutdown_sender, task)
    }

    #[tokio::test]
    async fn incomplete_headers_are_closed_at_the_read_deadline() {
        let (address, shutdown, task) = start_server(test_limits()).await;
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nX-Slow:")
            .await
            .unwrap();

        let mut byte = [0_u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(1), client.read(&mut byte))
            .await
            .expect("slow header connection must be closed");
        assert_eq!(read.unwrap(), 0);
        let _ = shutdown.send(());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn excessive_http1_header_count_is_rejected() {
        let (address, shutdown, task) = start_server(test_limits()).await;
        let mut client = TcpStream::connect(address).await.unwrap();
        let mut request = b"GET / HTTP/1.1\r\nHost: localhost\r\n".to_vec();
        for index in 0..MAX_HTTP1_HEADERS {
            request.extend_from_slice(format!("X-{index}: value\r\n").as_bytes());
        }
        request.extend_from_slice(b"\r\n");
        client.write_all(&request).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(
            response.starts_with(b"HTTP/1.1 431"),
            "unexpected response: {}",
            String::from_utf8_lossy(&response)
        );
        let _ = shutdown.send(());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn per_ip_connection_limit_releases_on_drop() {
        let peers = Arc::new(Mutex::new(HashMap::new()));
        let permits = Arc::new(Semaphore::new(3));
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let first = PeerConnectionGuard::try_new(
            peer,
            peers.clone(),
            permits.clone().acquire_owned().await.unwrap(),
            1,
        )
        .unwrap();
        assert!(
            PeerConnectionGuard::try_new(
                peer,
                peers.clone(),
                permits.clone().acquire_owned().await.unwrap(),
                1,
            )
            .is_none()
        );
        drop(first);
        assert!(
            PeerConnectionGuard::try_new(peer, peers, permits.acquire_owned().await.unwrap(), 1,)
                .is_some()
        );
    }
}
