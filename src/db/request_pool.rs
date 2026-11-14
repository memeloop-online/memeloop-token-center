//! Cancellation owns the checked-out socket until the complete SQL result.
//!
//! No detached query task or SQL replay: dropping a future/stream drops its
//! RequestConnection, while fully consumed operations use normal pool reuse.
use std::{
    fmt,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use futures_util::{TryStreamExt, future::BoxFuture, stream::BoxStream};
use sqlx::{
    Any, AnyPool as SqlxPool, Either, Execute, Executor, SqlSafeStr, SqlStr, Transaction,
    any::{AnyConnectOptions, AnyPoolOptions, AnyQueryResult, AnyRow, AnyStatement, AnyTypeInfo},
};

use super::{DatabaseBackend, request_connection::RequestConnection};

/// In-process phase counters; no SQL, body data or connection URL is retained.
#[derive(Default)]
pub(super) struct PoolDiagnostics {
    pub(super) label: &'static str,
    waiters: AtomicUsize,
    initializing: AtomicUsize,
    initialization_cancelled: AtomicU64,
    initialization_failed: AtomicU64,
    returned: AtomicU64,
    discarded_protocol: AtomicU64,
}

impl PoolDiagnostics {
    pub(super) fn new(label: &'static str) -> Arc<Self> {
        Arc::new(Self {
            label,
            ..Self::default()
        })
    }
    pub(super) fn initializing(self: &Arc<Self>) -> InitializationGuard {
        self.initializing.fetch_add(1, Ordering::Relaxed);
        InitializationGuard {
            diagnostics: self.clone(),
            completed: false,
        }
    }
    pub(super) fn released(&self, reusable: bool) {
        self.returned.fetch_add(1, Ordering::Relaxed);
        if !reusable {
            self.discarded_protocol.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub(super) fn snapshot(&self) -> (usize, usize, u64, u64, u64, u64) {
        (
            self.waiters.load(Ordering::Relaxed),
            self.initializing.load(Ordering::Relaxed),
            self.initialization_cancelled.load(Ordering::Relaxed),
            self.initialization_failed.load(Ordering::Relaxed),
            self.returned.load(Ordering::Relaxed),
            self.discarded_protocol.load(Ordering::Relaxed),
        )
    }
}

pub(super) struct InitializationGuard {
    diagnostics: Arc<PoolDiagnostics>,
    completed: bool,
}
impl InitializationGuard {
    pub(super) fn finish(&mut self, success: bool) {
        self.completed = true;
        if !success {
            self.diagnostics
                .initialization_failed
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}
impl Drop for InitializationGuard {
    fn drop(&mut self) {
        self.diagnostics
            .initializing
            .fetch_sub(1, Ordering::Relaxed);
        if !self.completed {
            self.diagnostics
                .initialization_cancelled
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}
struct AcquisitionGuard<'a>(&'a AtomicUsize);
impl Drop for AcquisitionGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub(crate) struct RequestPool {
    inner: SqlxPool,
    backend: DatabaseBackend,
    diagnostics: Arc<PoolDiagnostics>,
}

impl fmt::Debug for RequestPool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(formatter)
    }
}

impl From<SqlxPool> for RequestPool {
    fn from(inner: SqlxPool) -> Self {
        let backend = if inner.connect_options().database_url.scheme() == "sqlite" {
            DatabaseBackend::Sqlite
        } else {
            DatabaseBackend::PostgreSql
        };
        Self {
            inner,
            backend,
            diagnostics: PoolDiagnostics::new("unclassified"),
        }
    }
}

impl RequestPool {
    pub(super) fn with_diagnostics(mut self, diagnostics: Arc<PoolDiagnostics>) -> Self {
        self.diagnostics = diagnostics;
        self
    }
    pub(super) fn diagnostics(&self) -> &PoolDiagnostics {
        &self.diagnostics
    }
    #[cfg(test)]
    pub(crate) async fn connect(url: &str) -> Result<Self, sqlx::Error> {
        SqlxPool::connect(url).await.map(Self::from)
    }

    pub(crate) async fn acquire(&self) -> Result<RequestConnection, sqlx::Error> {
        self.diagnostics.waiters.fetch_add(1, Ordering::Relaxed);
        let _waiting = AcquisitionGuard(&self.diagnostics.waiters);
        self.inner
            .acquire()
            .await
            .map(|connection| RequestConnection::new(connection, self.backend))
    }

    // Pool timeout means no application SQL was sent. Direct Executor callers
    // need the same cancellable PostgreSQL wait as auth/write acquisition;
    // retrying an executing query here would risk duplicate financial writes.
    async fn acquire_for_executor(&self) -> Result<RequestConnection, sqlx::Error> {
        let started = tokio::time::Instant::now();
        let mut timeouts = 0_u64;
        loop {
            match self.acquire().await {
                Err(sqlx::Error::PoolTimedOut)
                    if matches!(self.backend, DatabaseBackend::PostgreSql) =>
                {
                    timeouts += 1;
                    if timeouts == 1 || timeouts.is_multiple_of(6) {
                        let (waiters, initializing, cancelled, failed, returned, discarded) =
                            self.diagnostics.snapshot();
                        tracing::warn!(
                            pool_label = self.diagnostics.label,
                            pool_acquire_waiters = waiters,
                            pool_session_initializing = initializing,
                            pool_session_initialization_cancelled = cancelled,
                            pool_session_initialization_failed = failed,
                            pool_return_callbacks = returned,
                            pool_protocol_discards = discarded,
                            operation = "executor_acquire",
                            request_id = crate::api::current_proxy_request_id()
                                .map(|id| id.to_string())
                                .as_deref()
                                .unwrap_or("absent"),
                            phase = "acquire",
                            error_kind = "pool_timeout",
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            pool_max = self.options().get_max_connections(),
                            pool_size = self.size(),
                            pool_idle = self.num_idle(),
                            "waiting for PostgreSQL executor connection before sending SQL"
                        );
                    }
                }
                result => return result,
            }
        }
    }

    pub(crate) async fn begin(&self) -> Result<Transaction<'static, Any>, sqlx::Error> {
        Transaction::begin(self.acquire().await?.into_pool(), None).await
    }

    pub(crate) async fn begin_with(
        &self,
        statement: impl SqlSafeStr,
    ) -> Result<Transaction<'static, Any>, sqlx::Error> {
        Transaction::begin(
            self.acquire().await?.into_pool(),
            Some(statement.into_sql_str()),
        )
        .await
    }

    pub(crate) fn options(&self) -> &AnyPoolOptions {
        self.inner.options()
    }
    pub(crate) fn connect_options(&self) -> Arc<AnyConnectOptions> {
        self.inner.connect_options()
    }
    pub(crate) fn size(&self) -> u32 {
        self.inner.size()
    }
    pub(crate) fn num_idle(&self) -> usize {
        self.inner.num_idle()
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
    pub(crate) async fn close(&self) {
        self.inner.close().await
    }
}

impl<'pool> Executor<'pool> for &'pool RequestPool {
    type Database = Any;

    fn fetch_many<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxStream<'e, Result<Either<AnyQueryResult, AnyRow>, sqlx::Error>>
    where
        'pool: 'e,
        E: 'q + Execute<'q, Any>,
    {
        // This bounded channel is polled on the consumer's own task. It only
        // bridges SQLx's borrowing stream to its owned connection; no task can
        // keep running after the consumer drops the stream.
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let mut producer = Some(Box::pin(async move {
            let result = async {
                let mut connection = self.acquire_for_executor().await?;
                {
                    let mut rows = (&mut *connection).fetch_many(query);
                    while let Some(row) = rows.try_next().await? {
                        if sender.send(Ok(row)).await.is_err() {
                            return Ok(());
                        }
                    }
                }
                connection.finish();
                Ok::<(), sqlx::Error>(())
            }
            .await;
            if let Err(error) = result {
                let _ = sender.send(Err(error)).await;
            }
        }));
        Box::pin(futures_util::stream::poll_fn(move |context| {
            if let Some(future) = producer.as_mut()
                && future.as_mut().poll(context).is_ready()
            {
                producer = None;
            }
            receiver.poll_recv(context)
        }))
    }

    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxFuture<'e, Result<Option<AnyRow>, sqlx::Error>>
    where
        'pool: 'e,
        E: 'q + Execute<'q, Any>,
    {
        Box::pin(async move {
            let mut connection = self.acquire_for_executor().await?;
            let row = (&mut *connection).fetch_optional(query).await?;
            connection.finish();
            Ok(row)
        })
    }

    fn prepare_with<'e>(
        self,
        sql: SqlStr,
        parameters: &'e [AnyTypeInfo],
    ) -> BoxFuture<'e, Result<AnyStatement, sqlx::Error>>
    where
        'pool: 'e,
    {
        Box::pin(async move {
            let mut connection = self.acquire_for_executor().await?;
            let statement = (&mut *connection).prepare_with(sql, parameters).await?;
            connection.finish();
            Ok(statement)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;
    use std::time::Duration;

    #[tokio::test]
    async fn diagnostics_distinguish_cancelled_session_initialization_from_slot_waiting() {
        sqlx::any::install_default_drivers();
        let diagnostics = PoolDiagnostics::new("raw-persistence");
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let initialization_diagnostics = diagnostics.clone();
        let pause_initialization = paused.clone();
        let inner = AnyPoolOptions::new()
            .min_connections(0)
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(20))
            .after_connect(move |_connection, _| {
                let mut initialization = initialization_diagnostics.initializing();
                let paused = pause_initialization.clone();
                Box::pin(async move {
                    if paused.load(Ordering::Relaxed) {
                        std::future::pending::<()>().await;
                    }
                    initialization.finish(true);
                    Ok(())
                })
            })
            .connect_lazy("sqlite::memory:")
            .unwrap();
        let pool = RequestPool::from(inner).with_diagnostics(diagnostics.clone());
        assert!(matches!(
            pool.acquire().await,
            Err(sqlx::Error::PoolTimedOut)
        ));
        let (waiting, initializing, cancelled, failed, _, _) = diagnostics.snapshot();
        assert_eq!((waiting, initializing, failed), (0, 0, 0));
        assert!(
            cancelled > 0,
            "cancelled after-connect work is distinct from held slots"
        );
        paused.store(false, Ordering::Relaxed);
        let connection = pool.acquire().await.unwrap();
        connection.finish();
        assert_eq!(diagnostics.snapshot().0, 0);
        pool.close().await;
    }

    async fn held_test_pool() -> (RequestPool, RequestConnection) {
        sqlx::any::install_default_drivers();
        let inner = AnyPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(20))
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let mut pool = RequestPool::from(inner);
        sqlx::query("CREATE TABLE executions (n BIGINT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO executions VALUES (0)")
            .execute(&pool)
            .await
            .unwrap();
        // Exercise only the PostgreSQL pre-SQL waiting policy on a real held
        // pool slot; SQL itself stays portable and no PostgreSQL is simulated.
        pool.backend = DatabaseBackend::PostgreSql;
        let held = pool.acquire().await.unwrap();
        (pool, held)
    }

    #[tokio::test]
    async fn executor_waits_past_acquire_timeouts_then_executes_only_once() {
        for operation in ["many", "optional", "prepare"] {
            let (pool, mut held) = held_test_pool().await;
            let client = pool.clone();
            let mut pending = tokio::spawn(async move {
                match operation {
                    "many" => {
                        let rows = sqlx::query("UPDATE executions SET n = n + 1 RETURNING n")
                            .fetch_all(&client)
                            .await
                            .unwrap();
                        assert_eq!(rows.len(), 1);
                        assert_eq!(rows[0].get::<i64, _>("n"), 1);
                    }
                    "optional" => {
                        let row = sqlx::query("UPDATE executions SET n = n + 1 RETURNING n")
                            .fetch_optional(&client)
                            .await
                            .unwrap()
                            .unwrap();
                        assert_eq!(row.get::<i64, _>("n"), 1);
                    }
                    _ => {
                        client
                            .prepare(sqlx::AssertSqlSafe("SELECT n FROM executions").into_sql_str())
                            .await
                            .unwrap();
                    }
                }
            });
            assert!(
                tokio::time::timeout(Duration::from_millis(70), &mut pending)
                    .await
                    .is_err()
            );
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT n FROM executions")
                    .fetch_one(&mut *held)
                    .await
                    .unwrap(),
                0
            );
            held.return_to_pool().await;
            tokio::time::timeout(Duration::from_secs(2), pending)
                .await
                .unwrap()
                .unwrap();
            let actual: i64 = sqlx::query_scalar("SELECT n FROM executions")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(actual, if operation == "prepare" { 0 } else { 1 });
            pool.close().await;
        }
    }

    #[tokio::test]
    async fn cancelled_executor_wait_sends_no_sql_and_closed_pool_is_not_retried() {
        let (pool, mut held) = held_test_pool().await;
        let client = pool.clone();
        let mut pending = tokio::spawn(async move {
            sqlx::query("UPDATE executions SET n = n + 1")
                .execute(&client)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(70), &mut pending)
                .await
                .is_err()
        );
        assert_eq!(
            pool.diagnostics.snapshot().0,
            1,
            "the held-slot wait is observable"
        );
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        assert_eq!(
            pool.diagnostics.snapshot().0,
            0,
            "cancelled waiter is not retained"
        );

        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT n FROM executions")
                .fetch_one(&mut *held)
                .await
                .unwrap(),
            0
        );
        held.return_to_pool().await;
        assert!(matches!(
            sqlx::query("SELECT nonexistent FROM executions")
                .fetch_optional(&pool)
                .await,
            Err(sqlx::Error::Database(_))
        ));
        pool.close().await;
        assert!(matches!(
            sqlx::query("SELECT n FROM executions")
                .fetch_optional(&pool)
                .await,
            Err(sqlx::Error::PoolClosed)
        ));
    }
}
