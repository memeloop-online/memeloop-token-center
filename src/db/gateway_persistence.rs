use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use sqlx::{AnyPool, any::AnyPoolOptions};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::{
    AppError, ArchiveSpoolIdentity, Database, DatabaseBackend, RequestArchiveAdmission,
    StartProxyRequest, requests::StartedProxyRequest,
};
use crate::response_archive_spool::{BufferedArchive, BufferedArchivePurpose};

const TASK_LIMIT: usize = 16;
const BYTE_LIMIT: usize = 32 * 1024 * 1024;
const JOB_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct GatewayPersistence {
    pub(super) pool: AnyPool,
    slots: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    writer: Arc<Semaphore>,
    accepted: AtomicU64,
    capacity: AtomicU64,
    failed: AtomicU64,
    retention_limit: AtomicU64,
}

impl GatewayPersistence {
    pub(super) fn new(url: &str, backend: DatabaseBackend) -> Result<Self, sqlx::Error> {
        let pool = AnyPoolOptions::new()
            .max_connections(1)
            .min_connections(0)
            .acquire_timeout(Duration::from_millis(250))
            .idle_timeout(Some(Duration::from_secs(60)))
            .after_connect(move |connection, _| {
                Box::pin(async move {
                    match backend {
                        DatabaseBackend::Sqlite => {
                            sqlx::query("PRAGMA busy_timeout = 100")
                                .execute(connection)
                                .await?;
                        }
                        DatabaseBackend::PostgreSql => {
                            for statement in [
                                "SET statement_timeout = '250ms'",
                                "SET lock_timeout = '100ms'",
                                "SET idle_in_transaction_session_timeout = '1s'",
                            ] {
                                sqlx::query(statement).execute(&mut *connection).await?;
                            }
                        }
                    }
                    Ok(())
                })
            })
            .connect_lazy(url)?;
        Ok(Self {
            pool,
            slots: Arc::new(Semaphore::new(TASK_LIMIT)),
            bytes: Arc::new(Semaphore::new(BYTE_LIMIT)),
            writer: Arc::new(Semaphore::new(1)),
            accepted: AtomicU64::new(0),
            capacity: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            retention_limit: AtomicU64::new(0),
        })
    }

    fn try_admit(&self, bytes: usize) -> Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
        let slot = self.slots.clone().try_acquire_owned().ok()?;
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(u32::try_from(bytes).ok()?)
            .ok()?;
        Some((slot, bytes))
    }
}

impl Database {
    pub(crate) fn gateway_persistence_metrics(&self) -> String {
        let persistence = &self.gateway_persistence;
        format!(
            "# TYPE memeloop_token_center_request_persistence_jobs gauge\nmemeloop_token_center_request_persistence_jobs {}\n# TYPE memeloop_token_center_request_persistence_bytes gauge\nmemeloop_token_center_request_persistence_bytes {}\n# TYPE memeloop_token_center_request_persistence_total counter\nmemeloop_token_center_request_persistence_total{{outcome=\"accepted\"}} {}\nmemeloop_token_center_request_persistence_total{{outcome=\"capacity\"}} {}\nmemeloop_token_center_request_persistence_total{{outcome=\"failed\"}} {}\nmemeloop_token_center_request_persistence_total{{outcome=\"retention_limit\"}} {}\n",
            TASK_LIMIT - persistence.slots.available_permits(),
            BYTE_LIMIT - persistence.bytes.available_permits(),
            persistence.accepted.load(Ordering::Relaxed),
            persistence.capacity.load(Ordering::Relaxed),
            persistence.failed.load(Ordering::Relaxed),
            persistence.retention_limit.load(Ordering::Relaxed),
        )
    }

    pub(crate) async fn start_proxy_request_with_deferred_archive(
        &self,
        input: StartProxyRequest<'_>,
        body: &Bytes,
        pepper: &[u8],
        compression_enabled: bool,
    ) -> Result<StartedProxyRequest, AppError> {
        let request_id = input.request_id;
        let tenant_id = input.key.tenant_id;
        if input.request_object != format!("gap://{request_id}/request") {
            return Err(AppError::Internal);
        }
        let reservation = self
            .start_proxy_forwarding_request(input)
            .await
            .map_err(|error| match error {
                AppError::Storage(_) | AppError::Internal => AppError::Overloaded,
                other => other,
            })?;
        let identity = ArchiveSpoolIdentity {
            request_id,
            tenant_id,
            reservation_id: reservation.id,
        };
        let nodes = crate::gateway_body::memory::JsonMemoryScanner::default().observe(body);
        let charge = body
            .len()
            .saturating_mul(3)
            .saturating_add(nodes.saturating_mul(256))
            .saturating_add(4 * 1024 * 1024);
        let archive_admission = if body.len() > super::archive_spool::REQUEST_ARCHIVE_PLAIN_LIMIT {
            self.gateway_persistence
                .retention_limit
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(%request_id, reason = "retention_limit", "request archive omitted");
            RequestArchiveAdmission::GapRetentionLimit
        } else if let Some(permits) = self.gateway_persistence.try_admit(charge) {
            self.gateway_persistence
                .accepted
                .fetch_add(1, Ordering::Relaxed);
            let body = Bytes::copy_from_slice(body);
            let pepper = pepper.to_vec();
            let mut database = self.clone();
            database.pool = self.gateway_persistence.pool.clone();
            let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
            tokio::spawn(async move {
                let _permits = permits;
                let result = tokio::time::timeout_at(deadline, async {
                    let _writer = database
                        .gateway_persistence
                        .writer
                        .acquire()
                        .await
                        .map_err(|_| AppError::Internal)?;
                    if let Err(error) = database.publish_proxy_started_event(identity).await {
                        tracing::warn!(%request_id, reason = error.diagnostic_category(), "request started event omitted");
                    }
                    let retained = crate::api::proxy::archive_retention::prepare_json_body_if_valid(&body);
                    let body = retained.as_ref().map_or_else(
                        || body.clone(),
                        |retained| crate::api::proxy::archive_retention::encode_json_body(&body, retained),
                    );
                    drop(retained);
                    database
                        .capture_deferred_request_archive(
                            identity,
                            &body,
                            &pepper,
                            compression_enabled,
                        )
                        .await
                })
                .await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        database
                            .gateway_persistence
                            .failed
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(%request_id, reason = error.diagnostic_category(), "request archive omitted; durable gap retained")
                    }
                    Err(_) => {
                        database
                            .gateway_persistence
                            .failed
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(%request_id, reason = "timeout", "request archive deadline; durable gap retained unless capture committed")
                    }
                }
            });
            RequestArchiveAdmission::Queued
        } else {
            self.gateway_persistence
                .capacity
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(%request_id, reason = "queue_full", "request archive omitted");
            RequestArchiveAdmission::GapCapacity
        };
        Ok(StartedProxyRequest {
            reservation,
            archive_admission,
        })
    }

    async fn capture_deferred_request_archive(
        &self,
        identity: ArchiveSpoolIdentity,
        body: &Bytes,
        pepper: &[u8],
        compression_enabled: bool,
    ) -> Result<(), AppError> {
        let archive = BufferedArchive::new(
            identity,
            BufferedArchivePurpose::Request,
            body,
            pepper,
            compression_enabled,
        )?;
        let prepared = archive.prepare_first_batch().await?;
        let capacity = self.reserve_deferred_archive_capacity(&archive).await?;
        let Some(capacity) = capacity else {
            self.gateway_persistence
                .capacity
                .fetch_add(1, Ordering::Relaxed);
            let mut transaction = self.begin_write_transaction().await?;
            super::archive_spool::insert_request_archive_gap_in_transaction(
                &mut transaction,
                super::unix_millis(),
                identity,
                body,
                "capacity",
            )
            .await?;
            transaction.commit().await?;
            return Ok(());
        };
        let (mut transaction, now, hold) = self
            .reserved_spool_transaction(&capacity, "deferred_request_archive")
            .await?;
        if !self
            .capture_reserved_buffered_archive_body_in_transaction(
                &mut transaction,
                now,
                &archive,
                Some(prepared),
                Some(&capacity),
            )
            .await?
        {
            return Err(AppError::Overloaded);
        }
        hold.commit(transaction).await?;
        capacity.release().await;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn saturate_gateway_persistence_for_test(&self) -> OwnedSemaphorePermit {
        self.gateway_persistence
            .slots
            .clone()
            .acquire_many_owned(TASK_LIMIT as u32)
            .await
            .unwrap()
    }

    #[cfg(test)]
    pub(crate) async fn drain_gateway_persistence_for_test(&self) {
        let _all = tokio::time::timeout(
            Duration::from_secs(5),
            self.gateway_persistence
                .slots
                .acquire_many(TASK_LIMIT as u32),
        )
        .await
        .expect("bounded gateway persistence drain")
        .unwrap();
    }
}

#[cfg(test)]
mod tests;
