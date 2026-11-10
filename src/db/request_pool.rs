//! Cancellation owns the checked-out socket until the complete SQL result.
//!
//! No detached query task or SQL replay: dropping a future/stream drops its
//! RequestConnection, while fully consumed operations use normal pool reuse.
use std::{fmt, future::Future, sync::Arc};

use futures_util::{TryStreamExt, future::BoxFuture, stream::BoxStream};
use sqlx::{
    Any, AnyPool as SqlxPool, Either, Execute, Executor, SqlSafeStr, SqlStr, Transaction,
    any::{AnyConnectOptions, AnyPoolOptions, AnyQueryResult, AnyRow, AnyStatement, AnyTypeInfo},
};

use super::{DatabaseBackend, request_connection::RequestConnection};

#[derive(Clone)]
pub(crate) struct RequestPool {
    inner: SqlxPool,
    backend: DatabaseBackend,
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
        Self { inner, backend }
    }
}

impl RequestPool {
    #[cfg(test)]
    pub(crate) async fn connect(url: &str) -> Result<Self, sqlx::Error> {
        SqlxPool::connect(url).await.map(Self::from)
    }

    pub(crate) async fn acquire(&self) -> Result<RequestConnection, sqlx::Error> {
        self.inner
            .acquire()
            .await
            .map(|connection| RequestConnection::new(connection, self.backend))
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
                let mut connection = self.acquire().await?;
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
            let mut connection = self.acquire().await?;
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
            let mut connection = self.acquire().await?;
            let statement = (&mut *connection).prepare_with(sql, parameters).await?;
            connection.finish();
            Ok(statement)
        })
    }
}
