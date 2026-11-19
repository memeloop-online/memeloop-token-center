use super::*;
use std::time::Duration;
struct RelayGuard(tokio::task::JoinHandle<()>);
impl Drop for RelayGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) async fn postgres_return_health_timeout_releases_slot(url: &str) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut endpoint = url::Url::parse(url).unwrap();
    let host = endpoint.host_str().unwrap().to_owned();
    let port = endpoint.port().unwrap_or(5432);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_port = listener.local_addr().unwrap().port();
    let (gate, mut paused) = tokio::sync::watch::channel(false);
    let blocked = Arc::new(tokio::sync::Notify::new());
    let relay_blocked = blocked.clone();
    let relay = RelayGuard(tokio::spawn(async move {
        let mut first = true;
        loop {
            let gate_this_connection = first;
            first = false;
            let (client, _) = listener.accept().await.unwrap();
            let server = tokio::net::TcpStream::connect((host.as_str(), port))
                .await
                .unwrap();
            let (mut client_read, mut client_write) = client.into_split();
            let (mut server_read, mut server_write) = server.into_split();
            let upstream = async {
                let _ = tokio::io::copy(&mut client_read, &mut server_write).await;
            };
            let downstream = async {
                let mut buffer = vec![0; 65536];
                loop {
                    let count = match server_read.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(count) => count,
                    };
                    loop {
                        let is_paused = gate_this_connection && *paused.borrow_and_update();
                        if !is_paused {
                            break;
                        }
                        relay_blocked.notify_one();
                        if paused.changed().await.is_err() {
                            return;
                        }
                    }
                    if client_write.write_all(&buffer[..count]).await.is_err() {
                        return;
                    }
                }
            };
            tokio::pin!(upstream, downstream);
            tokio::select! { _ = &mut upstream => {}, _ = &mut downstream => {} }
        }
    }));
    endpoint.set_host(Some("127.0.0.1")).unwrap();
    endpoint.set_port(Some(relay_port)).unwrap();
    let pool = RequestPool::from(
        AnyPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(150))
            .connect(endpoint.as_str())
            .await
            .unwrap(),
    );
    let mut connection = pool.acquire().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    gate.send(true).unwrap();
    connection.finish();
    tokio::time::timeout(Duration::from_secs(5), blocked.notified())
        .await
        .expect("real PostgreSQL ping response reached the relay gate");
    assert_eq!(pool.diagnostics.direct_owned.load(Ordering::Relaxed), 0);
    assert_eq!(pool.diagnostics.return_requested.load(Ordering::Relaxed), 1);
    assert_eq!(pool.size(), 1);
    assert_eq!(pool.num_idle(), 0);
    tokio::time::timeout(Duration::from_secs(5), async {
        while pool.size() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("return health budget releases the stalled slot");
    gate.send(false).unwrap();
    let mut reused = tokio::time::timeout(Duration::from_secs(5), pool.acquire())
        .await
        .unwrap()
        .unwrap();
    let reused_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *reused)
        .await
        .unwrap();
    assert_ne!(
        reused_pid, pid,
        "a fresh backend replaces the stalled returned connection"
    );
    let same_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *reused)
        .await
        .unwrap();
    assert_eq!(same_pid, reused_pid);
    reused.finish();
    pool.close().await;
    drop(relay);
}
