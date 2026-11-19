use super::*;
use std::time::Duration;
struct RelayGuard(tokio::task::JoinHandle<()>);
impl Drop for RelayGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) async fn postgres_return_health_timeout_releases_slot(url: &str) {
    run_return_health_case(url, false, false).await;
    run_return_health_case(url, true, false).await;
    run_return_health_case(url, false, true).await;
}

async fn run_return_health_case(url: &str, cancel_begin: bool, block_close: bool) {
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
            let mut upstream_paused = paused.clone();
            let upstream = async {
                let mut bytes = vec![0; 65536];
                loop {
                    let n = match client_read.read(&mut bytes).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    loop {
                        let waiting = block_close
                            && gate_this_connection
                            && *upstream_paused.borrow_and_update();
                        if !waiting {
                            break;
                        }
                        relay_blocked.notify_one();
                        if upstream_paused.changed().await.is_err() {
                            return;
                        }
                    }
                    if server_write.write_all(&bytes[..n]).await.is_err() {
                        return;
                    }
                }
            };
            let downstream = async {
                let mut buffer = vec![0; 65536];
                loop {
                    let count = match server_read.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(count) => count,
                    };
                    loop {
                        let is_paused =
                            !block_close && gate_this_connection && *paused.borrow_and_update();
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
    if block_close {
        let query_text = format!("SELECT 1 /*{}*/", "x".repeat(32 * 1024 * 1024));
        let mut sending =
            Box::pin(sqlx::raw_sql(sqlx::AssertSqlSafe(query_text)).execute(&mut *connection));
        tokio::select! {
            _ = blocked.notified() => {},
            result = &mut sending => panic!("large query completed before relay gate: {result:?}"),
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), sending.as_mut())
                .await
                .is_err(),
            "client write is blocked behind the real transport gate"
        );
        drop(sending);
        let closing = connection.into_pool().close();
        let result = tokio::time::timeout(Duration::from_secs(5), closing)
            .await
            .expect("close health budget releases a blocked socket");
        assert!(matches!(result, Err(sqlx::Error::Protocol(_))));
        assert_eq!(pool.size(), 0);
        gate.send(false).unwrap();
        pool.close().await;
        drop(relay);
        return;
    }
    let pending_begin = if cancel_begin {
        Some(tokio::spawn(async move {
            Transaction::begin(connection.into_pool(), None).await
        }))
    } else {
        connection.finish();
        None
    };
    tokio::time::timeout(Duration::from_secs(5), blocked.notified())
        .await
        .expect("real PostgreSQL ping response reached the relay gate");
    if let Some(pending) = pending_begin {
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
    } else {
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
    }
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
    let status: String =
        sqlx::query_scalar("SELECT state FROM pg_stat_activity WHERE pid = pg_backend_pid()")
            .fetch_one(&mut *reused)
            .await
            .unwrap();
    assert_eq!(
        status, "active",
        "replacement is outside the cancelled BEGIN transaction"
    );
    let same_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *reused)
        .await
        .unwrap();
    assert_eq!(same_pid, reused_pid);
    reused.finish();
    let mut healthy = pool.acquire().await.unwrap();
    let healthy_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *healthy)
        .await
        .unwrap();
    assert_eq!(
        healthy_pid, reused_pid,
        "healthy maintenance preserves backend reuse"
    );
    healthy.finish();
    if cancel_begin {
        let mut transaction = pool.begin().await.unwrap();
        assert_eq!(pool.active_transactions(), 1);
        let transaction_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
        assert_eq!(transaction_pid, reused_pid);
        transaction.commit().await.unwrap();
        assert_eq!(pool.active_transactions(), 0);
        let mut normal = pool.acquire().await.unwrap();
        let normal_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *normal)
            .await
            .unwrap();
        assert_eq!(normal_pid, reused_pid, "confirmed BEGIN keeps normal reuse");
        {
            let borrowed = sqlx::Connection::begin(&mut *normal).await.unwrap();
            assert_eq!(
                pool.active_transactions(),
                0,
                "borrowed transactions do not inflate pooled ownership"
            );
            borrowed.rollback().await.unwrap();
        }
        normal.close_on_drop();
        let mut marked = Transaction::begin(normal.into_pool(), None).await.unwrap();
        sqlx::query("SELECT 1").execute(&mut *marked).await.unwrap();
        marked.commit().await.unwrap();
        let mut replacement = pool.acquire().await.unwrap();
        let marked_replacement: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *replacement)
            .await
            .unwrap();
        assert_ne!(
            marked_replacement, normal_pid,
            "successful BEGIN preserves caller close-on-drop"
        );
        let old_exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid=$1)")
                .bind(pid)
                .fetch_one(&mut *replacement)
                .await
                .unwrap();
        assert!(!old_exists, "cancelled BEGIN backend exited");
        replacement.finish();
    }
    pool.close().await;
    drop(relay);
}
