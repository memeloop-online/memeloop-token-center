//! A cancelled read must not wait for SQLx to drain the abandoned query.
use std::ops::{Deref, DerefMut};

use sqlx::{Any, AnyConnection, Connection, pool::PoolConnection};

use super::DatabaseBackend;

pub(crate) struct RequestConnection {
    connection: Option<PoolConnection<Any>>,
    backend: DatabaseBackend,
}

impl RequestConnection {
    pub(super) fn new(connection: PoolConnection<Any>, backend: DatabaseBackend) -> Self {
        Self {
            connection: Some(connection),
            backend,
        }
    }

    /// Only a completed operation takes the normal SQLx return/reuse path.
    pub(super) fn finish(mut self) {
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
        if matches!(self.backend, DatabaseBackend::PostgreSql)
            && let Some(connection) = self.connection.take()
        {
            // Drop the original socket, not a spawned ping/drain future. The
            // pool can replace this connection immediately. PostgreSQL's
            // client_connection_check_interval detects this disconnect while
            // executing/waiting; it never authorizes replay of uncertain work.
            drop(connection.detach());
        }
    }
}

pub(super) fn can_reuse(connection: &AnyConnection, backend: DatabaseBackend) -> bool {
    // SQLx Transaction::drop queues ROLLBACK before returning its connection.
    // Pinging that connection would wait for the abandoned statement first.
    // Reject the queued-protocol connection so SQLx sends Terminate and closes
    // it, instead. Completed commit/rollback paths retain normal pool reuse.
    !matches!(backend, DatabaseBackend::PostgreSql) || !connection.should_flush()
}

#[cfg(test)]
mod tests {
    use super::super::{
        Database, DatabaseReadinessPhase, DatabaseReadinessProgress, DatabaseReadinessStage,
    };
    use super::*;
    use std::time::Duration;

    async fn backend_absent(database: &Database, pid: i32) {
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
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the disconnected backend must exit without unlocking its blocker");
    }

    #[tokio::test]
    async fn postgres_completed_read_commit_and_rollback_keep_the_same_connection() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let database = Database::connect_with_max(&url, 1).await.unwrap();
        let connection = database.pool.acquire().await.unwrap();
        let mut read = RequestConnection::new(connection, DatabaseBackend::PostgreSql);
        let first: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *read)
            .await
            .unwrap();
        read.finish();
        for commit in [true, false] {
            let mut transaction = database.begin_write_transaction().await.unwrap();
            let current: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *transaction)
                .await
                .unwrap();
            assert_eq!(
                current, first,
                "completed operations reuse the original backend"
            );
            if commit {
                transaction.commit().await.unwrap();
            } else {
                transaction.rollback().await.unwrap();
            }
        }
        let last: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&database.pool)
            .await
            .unwrap();
        assert_eq!(last, first);
        database.close().await;
    }

    #[tokio::test]
    async fn postgres_cancelled_locked_write_releases_connection_and_never_commits_or_replays() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let database = Database::connect_with_max(&url, 3).await.unwrap();
        let table = format!("mtc_cancel_{}", uuid::Uuid::now_v7().simple());
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE {table} (value INTEGER NOT NULL); INSERT INTO {table} VALUES (0)"
        )))
        .execute(&database.pool)
        .await
        .unwrap();
        let mut blocker = database.begin_write_transaction().await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE {table} SET value = 1")))
            .execute(&mut *blocker)
            .await
            .unwrap();
        let worker_database = database.clone();
        let worker_table = table.clone();
        let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
        let waiting = tokio::spawn(async move {
            let mut transaction = worker_database.begin_write_transaction().await.unwrap();
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *transaction)
                .await
                .unwrap();
            pid_tx.send(pid).unwrap();
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {worker_table} SET value = value + 100"
            )))
            .execute(&mut *transaction)
            .await
            .unwrap();
            transaction.commit().await.unwrap();
        });
        let pid = pid_rx.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let blocked: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND wait_event_type = 'Lock')")
                    .bind(pid).fetch_one(&database.pool).await.unwrap();
                if blocked { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        backend_absent(&database, pid).await;
        // The blocker is still held until after the abandoned backend exits.
        // Cancellation must not unlock it by replaying/completing the write.
        blocker.commit().await.unwrap();
        let value: i32 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT value FROM {table}")))
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert_eq!(
            value, 1,
            "the cancelled +100 write never committed or replayed"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TABLE {table}")))
            .execute(&database.pool)
            .await
            .unwrap();
        database.close().await;
    }

    #[tokio::test]
    async fn postgres_readiness_cancel_after_checkout_releases_its_only_connection() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let database = Database::connect_with_max(&url, 1).await.unwrap();
        let first: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&database.pool)
            .await
            .unwrap();
        let mut progress = DatabaseReadinessProgress::new();
        progress.stalled_at = Some((DatabaseReadinessStage::Ping, DatabaseReadinessPhase::Query));
        let progress = std::sync::Arc::new(progress);
        let worker_progress = progress.clone();
        let worker_database = database.clone();
        let checking = tokio::spawn(async move {
            worker_database
                .readiness_check_observed(&worker_progress)
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), progress.stalled.notified())
            .await
            .unwrap();
        checking.abort();
        assert!(checking.await.unwrap_err().is_cancelled());
        let replacement: i32 = tokio::time::timeout(
            Duration::from_secs(5),
            sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&database.pool),
        )
        .await
        .unwrap()
        .unwrap();
        assert_ne!(
            replacement, first,
            "cancelled readiness does not return its unfinished connection"
        );
        backend_absent(&database, first).await;
        database.close().await;
    }
}
