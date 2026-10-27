use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

#[cfg(test)]
use bytes::Bytes;
use sqlx::{AnyPool, Row, any::AnyPoolOptions};
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

use super::{
    AppError, ArchiveSpoolIdentity, Database, DatabaseBackend, RequestArchiveAdmission,
    StartProxyRequest, requests::StartedProxyRequest,
};
#[cfg(test)]
use crate::response_archive_spool::{BufferedArchive, BufferedArchivePurpose};

const TASK_LIMIT: usize = 16;
const BYTE_LIMIT: usize = 32 * 1024 * 1024;
#[cfg(test)]
const JOB_TIMEOUT: Duration = Duration::from_secs(2);

tokio::task_local! {
    static ARCHIVE_DEADLINE: (Arc<GatewayPersistence>, tokio::time::Instant);
}

#[cfg(test)]
tokio::task_local! {
    static TEST_CAPTURE_BEFORE_DISPATCH: Arc<GatewayPersistence>;
}

pub(super) struct GatewayPersistence {
    pub(super) pool: AnyPool,
    slots: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    #[cfg(test)]
    writer: Arc<Semaphore>,
    accepted: AtomicU64,
    capacity: AtomicU64,
    failed: AtomicU64,
    retention_limit: AtomicU64,
    #[cfg(test)]
    busy_retries: AtomicU64,
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
            #[cfg(test)]
            writer: Arc::new(Semaphore::new(1)),
            accepted: AtomicU64::new(0),
            capacity: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            retention_limit: AtomicU64::new(0),
            #[cfg(test)]
            busy_retries: AtomicU64::new(0),
        })
    }

    #[cfg(test)]
    fn fitting_byte_charge(bytes: usize) -> Option<u32> {
        if bytes > BYTE_LIMIT {
            return None;
        }
        u32::try_from(bytes).ok()
    }

    #[cfg(test)]
    fn try_admit(&self, bytes: usize) -> Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
        let charge = Self::fitting_byte_charge(bytes)?;
        let slot = self.slots.clone().try_acquire_owned().ok()?;
        let bytes = self.bytes.clone().try_acquire_many_owned(charge).ok()?;
        Some((slot, bytes))
    }

    /// Waits for a task slot, and for byte permits when the charge fits.
    /// A raw request JSON larger than the byte semaphore is still archived:
    /// those permits are process memory, not object storage, and waiting for
    /// more permits than the semaphore holds would never complete.
    #[cfg(test)]
    async fn acquire_archive_permits(
        &self,
        bytes: usize,
    ) -> (OwnedSemaphorePermit, Option<OwnedSemaphorePermit>) {
        let slot = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .expect("gateway archive slot semaphore stays open");
        let bytes = match Self::fitting_byte_charge(bytes) {
            Some(charge) => Some(
                self.bytes
                    .clone()
                    .acquire_many_owned(charge)
                    .await
                    .expect("gateway archive byte semaphore stays open"),
            ),
            None => None,
        };
        (slot, bytes)
    }
}

impl Database {
    pub(crate) async fn start_proxy_request_with_source(
        &self,
        input: StartProxyRequest<'_>,
        source: &Arc<crate::gateway_body::request_spool::RequestSpool>,
        upstream_model: Option<&str>,
        output_token_limit: Option<i64>,
    ) -> Result<StartedProxyRequest, AppError> {
        let request_id = input.request_id;
        let tenant_id = input.key.tenant_id;
        if crate::gateway_body::request_spool::source_request_id(input.request_object)
            != Some(request_id)
            || source.object_locator() != Some(input.request_object)
        {
            return Err(AppError::internal("gateway_request_source_identity"));
        }
        let reservation = self
            .start_proxy_forwarding_request_with_output_limit(
                input,
                upstream_model,
                output_token_limit,
            )
            .await?;
        self.gateway_persistence
            .accepted
            .fetch_add(1, Ordering::Relaxed);
        let identity = ArchiveSpoolIdentity {
            request_id,
            tenant_id,
            reservation_id: reservation.id,
        };
        source.retain_for_archive(self.clone(), identity);
        self.persist_request_source(
            identity,
            source.object_locator().unwrap(),
            source.len(),
            source.digest(),
            None,
        )
        .await;
        #[cfg(test)]
        if TEST_CAPTURE_BEFORE_DISPATCH
            .try_with(|persistence| Arc::ptr_eq(persistence, &self.gateway_persistence))
            .unwrap_or(false)
        {
            let success = source.upload_outcome().await;
            self.complete_request_source(
                identity,
                source.object_locator().unwrap(),
                source.len(),
                source.digest(),
                success,
            )
            .await;
        }
        Ok(StartedProxyRequest {
            reservation,
            archive_admission: RequestArchiveAdmission::Queued,
        })
    }

    pub(crate) async fn complete_request_source(
        &self,
        identity: ArchiveSpoolIdentity,
        locator: &str,
        length: usize,
        digest: [u8; 32],
        success: bool,
    ) {
        self.persist_request_source(identity, locator, length, digest, Some(success))
            .await;
    }

    async fn persist_request_source(
        &self,
        identity: ArchiveSpoolIdentity,
        locator: &str,
        length: usize,
        digest: [u8; 32],
        success: Option<bool>,
    ) {
        let mut database = self.clone();
        database.pool = self.gateway_persistence.pool.clone();
        database.pool_role = "gateway_archive";
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let result = tokio::time::timeout_at(
            deadline,
            ARCHIVE_DEADLINE.scope(
                (self.gateway_persistence.clone(), deadline),
                database.record_request_source_state(identity, locator, length, digest, success),
            ),
        )
        .await;
        if !matches!(result, Ok(Ok(()))) {
            self.gateway_persistence
                .failed
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(request_id = %identity.request_id, "request source state awaits worker recovery");
        }
    }

    pub(crate) fn try_request_source_completion(&self) -> Option<OwnedSemaphorePermit> {
        self.gateway_persistence
            .slots
            .clone()
            .try_acquire_owned()
            .ok()
    }

    pub(crate) async fn request_source_completion(&self) -> OwnedSemaphorePermit {
        self.gateway_persistence
            .slots
            .clone()
            .acquire_owned()
            .await
            .expect("gateway source completion semaphore stays open")
    }

    pub(crate) fn schedule_request_source_completion(
        &self,
        identity: ArchiveSpoolIdentity,
        locator: String,
        length: usize,
        digest: [u8; 32],
        success: bool,
    ) {
        let Some(permit) = self.try_request_source_completion() else {
            return;
        };
        let database = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            database
                .complete_request_source(identity, &locator, length, digest, success)
                .await;
        });
    }

    pub(crate) async fn record_request_source(
        &self,
        identity: ArchiveSpoolIdentity,
        locator: &str,
        length: usize,
        digest: [u8; 32],
        success: bool,
    ) -> Result<(), AppError> {
        self.record_request_source_state(identity, locator, length, digest, Some(success))
            .await
    }

    async fn record_request_source_state(
        &self,
        identity: ArchiveSpoolIdentity,
        locator: &str,
        length: usize,
        digest: [u8; 32],
        success: Option<bool>,
    ) -> Result<(), AppError> {
        if crate::gateway_body::request_spool::source_request_id(locator)
            != Some(identity.request_id)
        {
            return Err(AppError::internal("gateway_request_source_identity"));
        }
        let mut transaction = self.begin_write_transaction().await?;
        let now = super::unix_millis();
        let owned = sqlx::query("UPDATE request_records SET request_object = request_object WHERE id = $1 AND tenant_id = $2 AND reservation_id = $3 AND request_object = $4 RETURNING key_id")
            .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(identity.reservation_id.to_string()).bind(locator)
            .fetch_optional(&mut *transaction).await?;
        let Some(owned) = owned else {
            return Ok(());
        };
        let changed = sqlx::query("INSERT INTO request_archive_spools (request_id, tenant_id, reservation_id, state, chunk_count, byte_count, cipher_bytes, attempts, next_attempt_at, created_at, updated_at, expires_at, cleaned_at, last_error_code, bound_locator, body_byte_count, body_blake3) VALUES ($1, $2, $3, $4, 0, 0, 0, 0, $5, $5, $5, $10, $5, $6, $7, $8, $9) ON CONFLICT(request_id) DO UPDATE SET state = excluded.state, bound_locator = excluded.bound_locator, last_error_code = excluded.last_error_code, updated_at = excluded.updated_at WHERE request_archive_spools.tenant_id = excluded.tenant_id AND request_archive_spools.reservation_id = excluded.reservation_id AND request_archive_spools.cipher_bytes = 0 AND ((request_archive_spools.state = 'capturing' AND excluded.state IN ('bound', 'gap')) OR (request_archive_spools.state = 'gap' AND excluded.state = 'bound'))")
            .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(identity.reservation_id.to_string())
            .bind(match success { Some(true) => "bound", Some(false) => "gap", None => "capturing" }).bind(now)
            .bind((success == Some(false)).then_some("capture_failed")).bind((success == Some(true)).then_some(locator))
            .bind(i64::try_from(length).map_err(|_| AppError::internal("gateway_request_source_length"))?)
            .bind(blake3::Hash::from_bytes(digest).to_hex().to_string()).bind(i64::MAX).execute(&mut *transaction).await?;
        let key = crate::archive_staging::ArchiveStagingKey::new(
            crate::archive_staging::ArchiveStagingOwner::ProxyRequest(identity.request_id),
            crate::archive_staging::ArchiveStagingPurpose::Request,
            identity.request_id,
        )?;
        sqlx::query("INSERT INTO archive_staging_attempts (attempt_id, owner_kind, owner_id, purpose, intent_digest, state, writer_owner, writer_token, bound_at, next_cleanup_at, created_at, updated_at) VALUES ($1, 'proxy_request', $1, 'request', $2, 'cleanup_pending', 'gateway-source', $1, $3, $3, $3, $3) ON CONFLICT(attempt_id) DO NOTHING")
            .bind(key.attempt_id.to_string()).bind(blake3::Hash::from_bytes(digest).to_hex().to_string()).bind(now).execute(&mut *transaction).await?;
        if changed.rows_affected() == 1
            && let Some(success) = success
        {
            let tenant_id = identity.tenant_id.to_string();
            let request_id = identity.request_id.to_string();
            let key_id: String = owned.try_get("key_id")?;
            let cursor = super::allocate_request_event_cursor(
                &mut transaction,
                now,
                &tenant_id,
                &key_id,
                &request_id,
            )
            .await?;
            sqlx::query("INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) SELECT $1, tenant_id, key_id, id, $2, $3, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_records WHERE id = $4 AND tenant_id = $5 AND key_id = $6")
                .bind(cursor.event_id).bind(cursor.event_at).bind(if success { "archive_bound" } else { "archive_gap" })
                .bind(request_id).bind(tenant_id).bind(key_id).execute(&mut *transaction).await?;
        }
        transaction.commit().await?;
        if changed.rows_affected() == 1
            && success.is_some()
            && let Err(error) = self.publish_proxy_started_event(identity).await
        {
            tracing::warn!(request_id = %identity.request_id, reason = error.diagnostic_category(), "request started event omitted");
        }
        Ok(())
    }

    pub(super) fn gateway_archive_retry_deadline(&self) -> Option<tokio::time::Instant> {
        if self.pool_role != "gateway_archive" || !matches!(self.backend, DatabaseBackend::Sqlite) {
            return None;
        }
        ARCHIVE_DEADLINE
            .try_with(|(owner, deadline)| {
                Arc::ptr_eq(owner, &self.gateway_persistence).then_some(*deadline)
            })
            .ok()
            .flatten()
    }

    pub(super) async fn begin_gateway_archive_transaction(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<sqlx::Transaction<'static, sqlx::Any>, sqlx::Error> {
        if tokio::time::Instant::now() >= deadline {
            return Err(sqlx::Error::PoolTimedOut);
        }
        loop {
            let error = match self.pool.begin_with("BEGIN IMMEDIATE").await {
                Ok(transaction) => return Ok(transaction),
                Err(error) => error,
            };
            if !matches!(&error, sqlx::Error::Database(error) if error.code().as_deref() == Some("5"))
                || tokio::time::Instant::now() >= deadline
            {
                return Err(error);
            }
            #[cfg(test)]
            self.gateway_persistence
                .busy_retries
                .fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + Duration::from_millis(10)),
            )
            .await;
            if tokio::time::Instant::now() >= deadline {
                return Err(error);
            }
        }
    }

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

    #[cfg(test)]
    pub(crate) async fn start_proxy_request_with_deferred_archive(
        &self,
        input: StartProxyRequest<'_>,
        body: &Bytes,
        pepper: &[u8],
        compression_enabled: bool,
        upstream_model: Option<&str>,
        output_token_limit: Option<i64>,
    ) -> Result<StartedProxyRequest, AppError> {
        let request_id = input.request_id;
        let tenant_id = input.key.tenant_id;
        if input.request_object != format!("gap://{request_id}/request") {
            return Err(AppError::internal(
                "gateway_persistence_start_proxy_request_with_deferred_archive_1",
            ));
        }
        let reservation = self
            .start_proxy_forwarding_request_with_output_limit(
                input,
                upstream_model,
                output_token_limit,
            )
            .await
            .map_err(|error| match error {
                error @ AppError::Capacity { .. } => error,
                error @ (AppError::Storage { .. } | AppError::Internal { .. }) => error
                    .retarget_capacity(
                        "request_record_persist_failed",
                        crate::error::ErrorFacts::new().phase("request_admission"),
                    ),
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
        // Memory permits bound concurrent archive work. They are not object
        // storage being full, and they never omit the raw request JSON or
        // fail admission because of a hardcoded byte limit.
        let persistence = self.gateway_persistence.clone();
        let immediate = persistence.try_admit(charge);
        persistence.accepted.fetch_add(1, Ordering::Relaxed);
        let body: Bytes = body.clone();
        let pepper = pepper.to_vec();
        let mut database = self.clone();
        database.pool = persistence.pool.clone();
        database.pool_role = "gateway_archive";
        tokio::spawn(async move {
            // A permit wait is backpressure, not a failure: the queued job keeps
            // only a refcount, and dropping the capture here would leave the
            // durable `gap://` locator behind for good.
            let _permits = match immediate {
                Some((slot, bytes)) => (slot, Some(bytes)),
                None => persistence.acquire_archive_permits(charge).await,
            };
            // Permit wait is outside the capture deadline. A full memory
            // semaphore must not expire the job and drop the raw request JSON.
            let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
            let capture = async {
                let _writer = database
                    .gateway_persistence
                    .writer
                    .acquire()
                    .await
                    .map_err(|_| {
                        AppError::internal(
                            "gateway_persistence_start_proxy_request_with_deferred_archive_1",
                        )
                    })?;
                if let Err(error) = database.publish_proxy_started_event(identity).await {
                    tracing::warn!(%request_id, reason = error.diagnostic_category(), "request started event omitted");
                }
                let retained =
                    crate::api::proxy::archive_retention::prepare_json_body_if_valid(&body);
                let body = retained.as_ref().map_or_else(
                    || body.clone(),
                    |retained| {
                        crate::api::proxy::archive_retention::encode_json_body(&body, retained)
                    },
                );
                drop(retained);
                database
                    .capture_deferred_request_archive(identity, &body, &pepper, compression_enabled)
                    .await
            };
            let result = tokio::time::timeout_at(
                deadline,
                ARCHIVE_DEADLINE.scope((database.gateway_persistence.clone(), deadline), capture),
            )
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
        let archive_admission = RequestArchiveAdmission::Queued;
        // Byte-exact archive tests must finish optional request capture before
        // dispatch can race it with settlement on SQLite's single writer.
        // This barrier is opt-in, task-local and tied to this database instance;
        // degradation tests and production keep the asynchronous path.
        #[cfg(test)]
        if TEST_CAPTURE_BEFORE_DISPATCH
            .try_with(|persistence| Arc::ptr_eq(persistence, &self.gateway_persistence))
            .unwrap_or(false)
        {
            self.drain_gateway_persistence_for_test().await;
        }
        Ok(StartedProxyRequest {
            reservation,
            archive_admission,
        })
    }

    #[cfg(test)]
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
            return Err(AppError::capacity(
                "request_archive_capture_failed",
                crate::error::ErrorFacts::new()
                    .phase("deferred_request_archive")
                    .stage("capacity"),
            ));
        }
        hold.commit(transaction).await?;
        capacity.release().await;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn with_request_archive_capture_for_test<F: std::future::Future>(
        &self,
        future: F,
    ) -> F::Output {
        TEST_CAPTURE_BEFORE_DISPATCH
            .scope(self.gateway_persistence.clone(), future)
            .await
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
