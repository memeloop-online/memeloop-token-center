use std::ops::{Deref, DerefMut};

use sqlx::{Any, AnyConnection, pool::PoolConnection, postgres::PgConnectOptions};

/// Keeps a request's checked-out connection until its SQL has completed.
/// If its future is dropped, the connection is detached and closed rather
/// than returned to the pool; a protocol CancelRequest targets that exact
/// backend using its server-issued secret, so PID reuse cannot cancel a
/// different connection.
pub(super) struct RequestConnection {
    connection: Option<PoolConnection<Any>>,
    cancellation: Option<(PgConnectOptions, (u32, u32))>,
}

impl RequestConnection {
    pub(super) fn new(connection: PoolConnection<Any>, options: Option<PgConnectOptions>) -> Self {
        let cancellation =
            options.and_then(|options| connection.postgres_cancel_key().map(|key| (options, key)));
        Self {
            connection: Some(connection),
            cancellation,
        }
    }

    /// The query has completed (including an error), so normal pool reuse is safe.
    pub(super) fn finish(mut self) {
        self.cancellation = None;
        drop(self.connection.take());
    }
}

impl Deref for RequestConnection {
    type Target = AnyConnection;

    fn deref(&self) -> &Self::Target {
        self.connection
            .as_ref()
            .expect("request connection present")
    }
}

impl DerefMut for RequestConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection
            .as_mut()
            .expect("request connection present")
    }
}

impl Drop for RequestConnection {
    fn drop(&mut self) {
        let Some((options, (process_id, secret_key))) = self.cancellation.take() else {
            return;
        };
        // Detach first, so the pool cannot reuse the connection while the
        // cancellation is in flight. Dropping the raw connection closes it.
        if let Some(connection) = self.connection.take() {
            drop(connection.detach());
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if options
                    .cancel_backend_with_key(process_id, secret_key)
                    .await
                    .is_err()
                {
                    tracing::warn!("postgres request cancellation could not be delivered");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn postgres_dropped_request_cancels_locked_statement_and_keeps_pool_usable() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let database = super::super::Database::connect_with_max(&url, 3)
            .await
            .unwrap();
        let mut holder = database.pool.acquire().await.unwrap();
        let lock_id = uuid::Uuid::new_v4().as_u128() as i64;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(lock_id)
            .execute(&mut *holder)
            .await
            .unwrap();
        let mut original = database.pool.acquire().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *original)
            .await
            .unwrap();
        let mut guarded = RequestConnection::new(original, database.request_cancel_options.clone());
        let waiting = tokio::spawn(async move {
            let result = sqlx::query("SELECT pg_advisory_lock($1)")
                .bind(lock_id)
                .execute(&mut *guarded)
                .await;
            guarded.finish();
            result
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let blocked: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND wait_event_type = 'Lock')")
                    .bind(pid).fetch_one(&database.pool).await.unwrap();
                if blocked {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        waiting.abort();
        let _ = waiting.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let present: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)",
                )
                .bind(pid)
                .fetch_one(&database.pool)
                .await
                .unwrap();
                if !present {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled backend must exit while the lock remains held");
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(lock_id)
            .execute(&mut *holder)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn postgres_completed_request_returns_connection_to_pool() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let database = super::super::Database::connect_with_max(&url, 1)
            .await
            .unwrap();
        let connection = database.pool.acquire().await.unwrap();
        let mut guarded =
            RequestConnection::new(connection, database.request_cancel_options.clone());
        let first: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *guarded)
            .await
            .unwrap();
        guarded.finish();
        let second: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&database.pool)
            .await
            .unwrap();
        assert_eq!(first, second);
    }
}
