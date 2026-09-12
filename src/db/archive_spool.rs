//! Bounded encrypted spool. Normal mutations serialize against the budget row.
//! GC uses bounded spool-first transactions with a NOWAIT budget lock; uploads
//! read bounded snapshot batches. No transaction encompasses object-storage I/O.
use std::time::{Duration, Instant};

use sqlx::{Any, Row, Transaction, any::AnyRow};
use uuid::Uuid;

use super::{AppError, Database, DatabaseBackend, allocate_request_event_cursor};
use crate::archive_staging::{
    ArchiveStagingOwner, ArchiveStagingPurpose, ArchiveStagingWriteLease,
};

const PLAIN_LIMIT: i64 = 64 * 1024 * 1024;
const CIPHER_CHUNK_LIMIT: usize = 512 * 1024;
const CIPHER_LIMIT: i64 = 256 * 1024 * 1024;
const CHUNK_LIMIT: i64 = 65536;
const CHUNK_OVERHEAD: i64 = 512;
const SPOOL_OVERHEAD: i64 = 1024;
const CAPTURE_TTL: i64 = 30 * 60 * 1000;
const RETENTION: i64 = 7 * 24 * 60 * 60 * 1000;
const LEASE_TTL: i64 = 60 * 1000;
const GC_CHUNK_LIMIT: i64 = 64;
const GC_BYTE_LIMIT: i64 = 1024 * 1024;
const LOAD_CHUNK_LIMIT: i64 = 256;
pub(crate) const LOAD_BYTE_LIMIT: i64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArchiveSpoolIdentity {
    pub request_id: Uuid,
    pub tenant_id: Uuid,
    pub reservation_id: Uuid,
}

#[derive(Clone, Debug)]
pub(crate) struct ArchiveSpoolTask {
    pub identity: ArchiveSpoolIdentity,
    pub lease_owner: Uuid,
    pub lease_token: Uuid,
    pub chunk_count: i64,
    pub byte_count: i64,
}

// Deliberately no Debug: ciphertext must not enter logs.
pub(crate) struct ArchiveSpoolChunk {
    pub seq: i64,
    pub ciphertext: String,
    pub byte_count: i64,
}

impl Database {
    pub(crate) async fn begin_response_archive_spool(
        &self,
        identity: ArchiveSpoolIdentity,
    ) -> Result<bool, AppError> {
        let (mut tx, now) = self.spool_transaction().await?;
        let valid: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_records WHERE id = $1 AND tenant_id = $2 AND reservation_id = $3 AND completed_at IS NULL")
            .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string())
            .bind(identity.reservation_id.to_string()).fetch_one(&mut *tx).await?;
        if valid != 1 {
            return Ok(false);
        }
        // Existing audit rows cannot be reopened, and exact retries must not
        // charge the fixed admission overhead a second time.
        if let Some(row) = sqlx::query("SELECT tenant_id, reservation_id, state, expires_at FROM response_archive_spools WHERE request_id = $1")
            .bind(identity.request_id.to_string()).fetch_optional(&mut *tx).await?
        {
            return Ok(row.try_get::<String, _>("tenant_id")? == identity.tenant_id.to_string()
                && row.try_get::<String, _>("reservation_id")? == identity.reservation_id.to_string()
                && row.try_get::<String, _>("state")? == "capturing"
                && row.try_get::<i64, _>("expires_at")? > now);
        }
        let budget = sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes + $1 WHERE singleton = 1 AND cipher_bytes <= $2")
            .bind(SPOOL_OVERHEAD).bind(CIPHER_LIMIT - SPOOL_OVERHEAD).execute(&mut *tx).await?;
        if budget.rows_affected() != 1 {
            return Ok(false);
        }
        sqlx::query("INSERT INTO response_archive_spools (request_id, tenant_id, reservation_id, state, next_attempt_at, created_at, updated_at, expires_at, cipher_bytes) VALUES ($1, $2, $3, 'capturing', $4, $4, $4, $5, $6)")
            .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string())
            .bind(identity.reservation_id.to_string()).bind(now).bind(now + CAPTURE_TTL)
            .bind(SPOOL_OVERHEAD)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub(crate) async fn append_response_archive_spool(
        &self,
        identity: ArchiveSpoolIdentity,
        seq: i64,
        byte_count: i64,
        ciphertext: &str,
    ) -> Result<bool, AppError> {
        if seq < 0
            || byte_count <= 0
            || byte_count > PLAIN_LIMIT
            || ciphertext.is_empty()
            || ciphertext.len() > CIPHER_CHUNK_LIMIT
        {
            return Ok(false);
        }
        let (mut tx, now) = self.spool_transaction().await?;
        let Some(row) = spool_row(&mut tx, identity).await? else {
            return Ok(false);
        };
        if row.try_get::<String, _>("state")? != "capturing"
            || row.try_get::<i64, _>("expires_at")? <= now
        {
            return Ok(false);
        }
        let count: i64 = row.try_get("chunk_count")?;
        if seq < count {
            let replay = sqlx::query("SELECT ciphertext, byte_count FROM response_archive_spool_chunks WHERE request_id = $1 AND seq = $2")
                .bind(identity.request_id.to_string()).bind(seq).fetch_optional(&mut *tx).await?;
            return Ok(replay.is_some_and(|r| {
                r.get::<String, _>("ciphertext") == ciphertext
                    && r.get::<i64, _>("byte_count") == byte_count
            }));
        }
        if seq != count
            || count >= CHUNK_LIMIT
            || row.try_get::<i64, _>("byte_count")? > PLAIN_LIMIT - byte_count
        {
            return Ok(false);
        }
        // Accounting includes a fixed row/index overhead, not just ciphertext.
        let cipher_bytes = ciphertext.len() as i64 + CHUNK_OVERHEAD;
        let budget = sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes + $1 WHERE singleton = 1 AND cipher_bytes <= $2")
            .bind(cipher_bytes).bind(CIPHER_LIMIT - cipher_bytes).execute(&mut *tx).await?;
        if budget.rows_affected() != 1 {
            return Ok(false);
        }
        sqlx::query("INSERT INTO response_archive_spool_chunks (request_id, seq, ciphertext, byte_count) VALUES ($1, $2, $3, $4)")
            .bind(identity.request_id.to_string()).bind(seq).bind(ciphertext).bind(byte_count).execute(&mut *tx).await?;
        sqlx::query("UPDATE response_archive_spools SET chunk_count = chunk_count + 1, byte_count = byte_count + $1, cipher_bytes = cipher_bytes + $2, updated_at = $3, expires_at = $4 WHERE request_id = $5")
            .bind(byte_count).bind(cipher_bytes).bind(now).bind(now + CAPTURE_TTL).bind(identity.request_id.to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub(crate) async fn seal_response_archive_spool(
        &self,
        identity: ArchiveSpoolIdentity,
        chunk_count: i64,
        byte_count: i64,
    ) -> Result<bool, AppError> {
        if chunk_count < 0 || byte_count < 0 {
            return Ok(false);
        }
        let (mut tx, now) = self.spool_transaction().await?;
        let Some(row) = spool_row(&mut tx, identity).await? else {
            return Ok(false);
        };
        if row.try_get::<i64, _>("chunk_count")? != chunk_count
            || row.try_get::<i64, _>("byte_count")? != byte_count
            || row.try_get::<i64, _>("expires_at")? <= now
        {
            return Ok(false);
        }
        let state: String = row.try_get("state")?;
        if state == "pending" {
            return Ok(true);
        }
        if state != "capturing" {
            return Ok(false);
        }
        sqlx::query("UPDATE response_archive_spools SET state = 'pending', updated_at = $1, next_attempt_at = $1, expires_at = $2 WHERE request_id = $3")
            .bind(now).bind(now + RETENTION).bind(identity.request_id.to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub(crate) async fn fail_response_archive_spool(
        &self,
        identity: ArchiveSpoolIdentity,
        reason: &str,
    ) -> Result<(), AppError> {
        let (mut tx, now) = self.spool_transaction().await?;
        // A lost seal ACK must not destroy a complete, recoverable pending spool.
        let changed = sqlx::query("UPDATE response_archive_spools SET state = 'gap', last_error_code = $1, updated_at = $2, expires_at = $3, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL WHERE request_id = $4 AND tenant_id = $5 AND reservation_id = $6 AND state = 'capturing'")
            .bind(reason_code(reason)).bind(now).bind(now + CAPTURE_TTL).bind(identity.request_id.to_string())
            .bind(identity.tenant_id.to_string()).bind(identity.reservation_id.to_string()).execute(&mut *tx).await?;
        if changed.rows_affected() == 1 {
            emit_response_archive_transition_event_in_transaction(
                &mut tx,
                identity.request_id,
                now,
                "archive_gap",
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn claim_response_archive_spool(
        &self,
        lease_owner: Uuid,
    ) -> Result<Option<ArchiveSpoolTask>, AppError> {
        self.claim_response_archive_spool_if(lease_owner, || true)
            .await
    }

    pub(crate) async fn claim_response_archive_spool_if(
        &self,
        lease_owner: Uuid,
        admit: impl FnOnce() -> bool,
    ) -> Result<Option<ArchiveSpoolTask>, AppError> {
        let (mut tx, now) = self.spool_transaction().await?;
        let row = sqlx::query("SELECT s.* FROM response_archive_spools s WHERE s.expires_at > $1 AND s.attempts < 10 AND ((s.state = 'pending' AND s.next_attempt_at <= $1) OR (s.state = 'uploading' AND s.lease_expires_at <= $1)) AND EXISTS (SELECT 1 FROM request_records r WHERE r.id = s.request_id AND r.tenant_id = s.tenant_id AND r.reservation_id = s.reservation_id AND r.completed_at IS NOT NULL AND r.response_object = 'gap://' || s.request_id || '/response') ORDER BY s.next_attempt_at, s.request_id LIMIT 1")
            .bind(now).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        // Decide shutdown admission while owning the serialized transaction,
        // before spending an attempt. Once admitted, the worker owns one
        // bounded upload even if shutdown arrives during COMMIT. Do not offer
        // a post-commit refund that could race object I/O or another owner.
        if !admit() {
            tx.commit().await?;
            return Ok(None);
        }
        let identity = identity_from_row(&row)?;
        let lease_token = Uuid::new_v4();
        sqlx::query("UPDATE response_archive_spools SET state = 'uploading', lease_owner = $1, lease_token = $2, lease_expires_at = $3, attempts = attempts + 1, updated_at = $4 WHERE request_id = $5")
            .bind(lease_owner.to_string()).bind(lease_token.to_string()).bind(now + LEASE_TTL).bind(now).bind(identity.request_id.to_string()).execute(&mut *tx).await?;
        let task = ArchiveSpoolTask {
            identity,
            lease_owner,
            lease_token,
            chunk_count: row.try_get("chunk_count")?,
            byte_count: row.try_get("byte_count")?,
        };
        tx.commit().await?;
        Ok(Some(task))
    }

    pub(crate) async fn load_response_archive_spool_batch(
        &self,
        task: &ArchiveSpoolTask,
        seq: i64,
    ) -> Result<Vec<ArchiveSpoolChunk>, AppError> {
        if seq < 0 || seq >= task.chunk_count {
            return Ok(Vec::new());
        }
        // A single statement snapshot checks ownership and selects a bounded
        // prefix. No producer budget lock or per-chunk transaction is needed:
        // a concurrent expiry/claim is fenced again at heartbeat and final bind.
        // Calculate sizes before fetching payload; never fetch unrestricted
        // ciphertext values and only then truncate in application memory.
        let (length, clock) = match self.backend {
            DatabaseBackend::PostgreSql => (
                "OCTET_LENGTH(ciphertext)",
                "CAST(FLOOR(EXTRACT(EPOCH FROM statement_timestamp()) * 1000) AS BIGINT)",
            ),
            DatabaseBackend::Sqlite => (
                "LENGTH(CAST(ciphertext AS BLOB))",
                "CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)",
            ),
        };
        let sql = format!(
            "WITH chunk_sizes AS (SELECT seq, {length} AS cipher_len FROM response_archive_spool_chunks WHERE request_id = $1 AND seq >= $2 ORDER BY seq LIMIT $3), bounded AS (SELECT seq, SUM(cipher_len) OVER (ORDER BY seq ROWS UNBOUNDED PRECEDING) AS running_bytes FROM chunk_sizes) SELECT c.seq, c.ciphertext, c.byte_count FROM bounded b JOIN response_archive_spool_chunks c ON c.request_id = $1 AND c.seq = b.seq JOIN response_archive_spools s ON s.request_id = c.request_id WHERE b.running_bytes <= $4 AND s.tenant_id = $5 AND s.reservation_id = $6 AND s.state = 'uploading' AND s.lease_owner = $7 AND s.lease_token = $8 AND s.lease_expires_at > {clock} AND s.expires_at > {clock} AND s.chunk_count = $9 AND s.byte_count = $10 ORDER BY c.seq"
        );
        // Only the backend-selected static length/clock expressions are
        // interpolated; every identity, lease and limit remains a bind.
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(task.identity.request_id.to_string())
            .bind(seq)
            .bind(LOAD_CHUNK_LIMIT)
            .bind(LOAD_BYTE_LIMIT)
            .bind(task.identity.tenant_id.to_string())
            .bind(task.identity.reservation_id.to_string())
            .bind(task.lease_owner.to_string())
            .bind(task.lease_token.to_string())
            .bind(task.chunk_count)
            .bind(task.byte_count)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| -> Result<_, AppError> {
                Ok(ArchiveSpoolChunk {
                    seq: r.try_get("seq")?,
                    ciphertext: r.try_get("ciphertext")?,
                    byte_count: r.try_get("byte_count")?,
                })
            })
            .collect()
    }

    #[cfg(test)]
    async fn load_response_archive_spool_chunk(
        &self,
        task: &ArchiveSpoolTask,
        seq: i64,
    ) -> Result<Option<ArchiveSpoolChunk>, AppError> {
        Ok(self
            .load_response_archive_spool_batch(task, seq)
            .await?
            .into_iter()
            .next()
            .filter(|chunk| chunk.seq == seq))
    }

    pub(crate) async fn heartbeat_response_archive_spool(
        &self,
        task: &ArchiveSpoolTask,
    ) -> Result<bool, AppError> {
        let (mut tx, now) = self.spool_transaction().await?;
        if !live_task(&mut tx, task, now).await? {
            return Ok(false);
        }
        sqlx::query("UPDATE response_archive_spools SET lease_expires_at = $1, updated_at = $2 WHERE request_id = $3")
            .bind(now + LEASE_TTL).bind(now).bind(task.identity.request_id.to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub(crate) async fn complete_response_archive_spool(
        &self,
        task: &ArchiveSpoolTask,
        staging: &ArchiveStagingWriteLease,
        locator: &str,
    ) -> Result<bool, AppError> {
        if staging.key.owner != ArchiveStagingOwner::ProxyRequest(task.identity.request_id)
            || staging.key.purpose != ArchiveStagingPurpose::Response
        {
            return Ok(false);
        }
        let (mut tx, now) = self.spool_transaction().await?;
        if !live_task(&mut tx, task, now).await? {
            return Ok(false);
        }
        let changed = sqlx::query("UPDATE request_records SET response_object = $1 WHERE id = $2 AND tenant_id = $3 AND reservation_id = $4 AND completed_at IS NOT NULL AND response_object = $5")
            .bind(locator).bind(task.identity.request_id.to_string()).bind(task.identity.tenant_id.to_string())
            .bind(task.identity.reservation_id.to_string()).bind(format!("gap://{}/response", task.identity.request_id)).execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Ok(false);
        }
        if !super::archive_staging::bind_archive_staging_attempt_in_transaction(
            &mut tx,
            self.backend,
            staging,
            locator,
        )
        .await?
        {
            return Ok(false);
        }
        let bound = sqlx::query("UPDATE response_archive_spools SET state = 'bound', bound_locator = $1, updated_at = $2, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL WHERE request_id = $3 AND tenant_id = $4 AND reservation_id = $5 AND state = 'uploading'")
            .bind(locator).bind(now).bind(task.identity.request_id.to_string())
            .bind(task.identity.tenant_id.to_string()).bind(task.identity.reservation_id.to_string())
            .execute(&mut *tx).await?;
        if bound.rows_affected() != 1 {
            return Ok(false);
        }
        emit_response_archive_transition_event_in_transaction(
            &mut tx,
            task.identity.request_id,
            now,
            "archive_bound",
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub(crate) async fn retry_response_archive_spool(
        &self,
        task: &ArchiveSpoolTask,
        reason: &str,
    ) -> Result<(), AppError> {
        let (mut tx, now) = self.spool_transaction().await?;
        if !live_task(&mut tx, task, now).await? {
            return Ok(());
        }
        let row = spool_row(&mut tx, task.identity)
            .await?
            .ok_or(AppError::Internal)?;
        let attempts: i64 = row.try_get("attempts")?;
        let backoff = 5_000_i64 * (1_i64 << attempts.clamp(0, 10) as u32);
        let terminal = attempts >= 10;
        sqlx::query("UPDATE response_archive_spools SET state = $1, next_attempt_at = $2, updated_at = $3, last_error_code = $4, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL WHERE request_id = $5")
            .bind(if attempts >= 10 { "gap" } else { "pending" }).bind(now + backoff).bind(now).bind(reason_code(reason)).bind(task.identity.request_id.to_string()).execute(&mut *tx).await?;
        if terminal {
            emit_response_archive_transition_event_in_transaction(
                &mut tx,
                task.identity.request_id,
                now,
                "archive_gap",
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn cleanup_response_archive_spools(
        &self,
        limit: i64,
    ) -> Result<u64, AppError> {
        self.cleanup_response_archive_spools_with_budget(limit, None)
            .await
    }

    pub(crate) async fn cleanup_response_archive_spools_for(
        &self,
        limit: i64,
        time_budget: Duration,
    ) -> Result<u64, AppError> {
        self.cleanup_response_archive_spools_with_budget(limit, Some(time_budget))
            .await
    }

    async fn cleanup_response_archive_spools_with_budget(
        &self,
        limit: i64,
        time_budget: Option<Duration>,
    ) -> Result<u64, AppError> {
        let started = Instant::now();
        let mut completed = 0;
        for _ in 0..limit.clamp(0, 32) {
            // Keep the transaction owned by an independent task. If the
            // worker is cancelled for shutdown while this await is pending,
            // dropping the JoinHandle detaches the batch and lets SQLx finish
            // its COMMIT/rollback protocol before that connection is reused.
            let db = self.clone();
            let batch =
                tokio::spawn(async move { db.cleanup_response_archive_spool_batch().await })
                    .await
                    .map_err(|_| AppError::Internal)??;
            match batch {
                Some(cleaned) => completed += u64::from(cleaned),
                None => break,
            }
            // Check only between committed batches. Cancelling a SQL future
            // at a wall-clock deadline can leave the pooled connection waiting
            // for its asynchronous rollback and race the next BEGIN. Each GC
            // transaction is already bounded by row/chunk/byte limits.
            if time_budget.is_some_and(|budget| started.elapsed() >= budget) {
                break;
            }
        }
        Ok(completed)
    }

    async fn cleanup_response_archive_spool_batch(&self) -> Result<Option<bool>, AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let row = match self.backend {
            DatabaseBackend::PostgreSql => {
                // Each indexed class contributes its oldest unlocked row, and
                // the globally oldest candidate wins. Locking happens during
                // selection so a busy oldest row cannot hide later work and
                // concurrent workers naturally fan out across the queue.
                sqlx::query(
                    "WITH db_clock AS MATERIALIZED (SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT) AS db_now),
                     bound_candidate AS MATERIALIZED (
                       SELECT s.request_id, s.updated_at AS eligible_at
                       FROM response_archive_spools s
                       WHERE s.cleaned_at IS NULL AND s.state = 'bound'
                       ORDER BY s.updated_at, s.request_id LIMIT 1 FOR UPDATE OF s SKIP LOCKED
                     ),
                     expired_candidate AS MATERIALIZED (
                       SELECT s.request_id, s.expires_at AS eligible_at
                       FROM response_archive_spools s CROSS JOIN db_clock c
                       WHERE s.cleaned_at IS NULL AND s.expires_at <= c.db_now
                       ORDER BY s.expires_at, s.request_id LIMIT 1 FOR UPDATE OF s SKIP LOCKED
                     ),
                     exhausted_candidate AS MATERIALIZED (
                       SELECT s.request_id, s.lease_expires_at AS eligible_at
                       FROM response_archive_spools s CROSS JOIN db_clock c
                       WHERE s.cleaned_at IS NULL AND s.state = 'uploading' AND s.attempts >= 10 AND s.lease_expires_at <= c.db_now
                       ORDER BY s.lease_expires_at, s.request_id LIMIT 1 FOR UPDATE OF s SKIP LOCKED
                     ),
                     chosen AS (
                       SELECT request_id, eligible_at FROM bound_candidate
                       UNION ALL SELECT request_id, eligible_at FROM expired_candidate
                       UNION ALL SELECT request_id, eligible_at FROM exhausted_candidate
                       ORDER BY eligible_at, request_id LIMIT 1
                     )
                     SELECT s.request_id, s.state, s.cipher_bytes, c.db_now
                     FROM chosen JOIN response_archive_spools s USING (request_id) CROSS JOIN db_clock c",
                )
                .fetch_optional(&mut *tx)
                .await?
            }
            DatabaseBackend::Sqlite => {
                // BEGIN IMMEDIATE serializes SQLite writers, so row-level skip
                // locking is neither available nor necessary. Preserve the
                // three partial-index-friendly probes and recheck eligibility
                // using database time inside the write transaction.
                let hint_now = super::unix_millis();
                let bound = sqlx::query("SELECT request_id, updated_at AS eligible_at FROM response_archive_spools WHERE cleaned_at IS NULL AND state = 'bound' ORDER BY updated_at, request_id LIMIT 1")
                    .fetch_optional(&mut *tx).await?;
                let expired = sqlx::query("SELECT request_id, expires_at AS eligible_at FROM response_archive_spools WHERE cleaned_at IS NULL AND expires_at <= $1 ORDER BY expires_at, request_id LIMIT 1")
                    .bind(hint_now).fetch_optional(&mut *tx).await?;
                let exhausted = sqlx::query("SELECT request_id, lease_expires_at AS eligible_at FROM response_archive_spools WHERE cleaned_at IS NULL AND state = 'uploading' AND attempts >= 10 AND lease_expires_at <= $1 ORDER BY lease_expires_at, request_id LIMIT 1")
                    .bind(hint_now).fetch_optional(&mut *tx).await?;
                let mut candidate: Option<(i64, String)> = None;
                for candidate_row in [bound, expired, exhausted].into_iter().flatten() {
                    let item = (
                        candidate_row.try_get::<i64, _>("eligible_at")?,
                        candidate_row.try_get::<String, _>("request_id")?,
                    );
                    if candidate.as_ref().is_none_or(|current| &item < current) {
                        candidate = Some(item);
                    }
                }
                let Some((_, id)) = candidate else {
                    return Ok(None);
                };
                sqlx::query(
                    "SELECT request_id, state, cipher_bytes, CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER) AS db_now FROM response_archive_spools WHERE request_id = $1 AND cleaned_at IS NULL AND (state = 'bound' OR expires_at <= CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER) OR (state = 'uploading' AND attempts >= 10 AND lease_expires_at <= CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)))",
                )
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
            }
        };
        let Some(row) = row else { return Ok(None) };
        let id: String = row.try_get("request_id")?;
        let now: i64 = row.try_get("db_now")?;
        let chunk_sql = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT seq, CAST(OCTET_LENGTH(ciphertext) AS BIGINT) AS cipher_len FROM response_archive_spool_chunks WHERE request_id = $1 ORDER BY seq LIMIT $2"
            }
            DatabaseBackend::Sqlite => {
                "SELECT seq, LENGTH(CAST(ciphertext AS BLOB)) AS cipher_len FROM response_archive_spool_chunks WHERE request_id = $1 ORDER BY seq LIMIT $2"
            }
        };
        let chunks = sqlx::query(chunk_sql)
            .bind(&id)
            .bind(GC_CHUNK_LIMIT)
            .fetch_all(&mut *tx)
            .await?;
        let mut released = 0_i64;
        let mut last_seq = None;
        for chunk in chunks {
            let bytes = chunk.try_get::<i64, _>("cipher_len")? + CHUNK_OVERHEAD;
            // Reserve final-spool overhead even when this batch may be last.
            if released + bytes + SPOOL_OVERHEAD > GC_BYTE_LIMIT {
                break;
            }
            released += bytes;
            last_seq = Some(chunk.try_get::<i64, _>("seq")?);
        }
        if let Some(seq) = last_seq {
            sqlx::query(
                "DELETE FROM response_archive_spool_chunks WHERE request_id = $1 AND seq <= $2",
            )
            .bind(&id)
            .bind(seq)
            .execute(&mut *tx)
            .await?;
        }
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM (SELECT seq FROM response_archive_spool_chunks WHERE request_id = $1 LIMIT 1) remaining_chunk")
            .bind(&id).fetch_one(&mut *tx).await?;
        let cleaned = remaining == 0;
        if cleaned {
            released += SPOOL_OVERHEAD;
        }
        let accounted: i64 = row.try_get("cipher_bytes")?;
        if released > accounted || (cleaned && released != accounted) {
            return Err(AppError::Internal);
        }
        // Partial cleanup fences expired uploaders immediately, but retains
        // the fixed overhead and cleaned_at=NULL until the final chunk is gone.
        // Every deletion and both accounting changes commit or roll back together.
        let previous_state: String = row.try_get("state")?;
        let bound = previous_state == "bound";
        sqlx::query("UPDATE response_archive_spools SET state = $1, cleaned_at = $2, cipher_bytes = cipher_bytes - $3, updated_at = $4, expires_at = CASE WHEN $5 = 1 THEN expires_at ELSE $4 END, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL WHERE request_id = $6")
            .bind(if bound { "bound" } else { "gap" }).bind(cleaned.then_some(now))
            .bind(released).bind(now).bind(i64::from(bound)).bind(&id).execute(&mut *tx).await?;
        // GC locks a bounded candidate set first, then tries the global lock
        // WITHOUT waiting. Producers use global -> spool order. NOWAIT is
        // essential: if one is waiting for a candidate, abort GC and let that
        // producer progress rather than creating a lock-order deadlock. The
        // entire bounded deletion rolls back, so a later pass can safely resume.
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            let _: i64 = sqlx::query_scalar("SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1 FOR UPDATE NOWAIT")
                .fetch_one(&mut *tx).await?;
        }
        // The shared budget is held only for this decrement and commit.
        sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes - $1 WHERE singleton = 1")
            .bind(released).execute(&mut *tx).await?;
        if !matches!(previous_state.as_str(), "bound" | "gap") {
            let request_id = Uuid::parse_str(&id).map_err(|_| AppError::Internal)?;
            emit_response_archive_transition_event_in_transaction(
                &mut tx,
                request_id,
                now,
                "archive_gap",
            )
            .await?;
        }
        tx.commit().await?;
        Ok(Some(cleaned))
    }

    async fn spool_transaction(&self) -> Result<(Transaction<'_, Any>, i64), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        // First for normal mutations. GC uses spool -> budget NOWAIT, never
        // waits on the reversed order, and rolls its bounded work back on busy.
        let lock = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1"
            }
        };
        let _: i64 = sqlx::query_scalar(lock).fetch_one(&mut *tx).await?;
        let clock = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT)"
            }
            DatabaseBackend::Sqlite => {
                "SELECT CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)"
            }
        };
        let now: i64 = sqlx::query_scalar(clock).fetch_one(&mut *tx).await?;
        Ok((tx, now))
    }
}

/// Emit a sparse, locator-free convergence signal only after a real terminal
/// spool transition. The same write transaction owns both the state change and
/// the globally monotonic request-event cursor, so rollback cannot expose one
/// without the other. Missing legacy request rows are tolerated for bounded GC
/// of pre-invariant audit data; live spool admission always has this owner row.
async fn emit_response_archive_transition_event_in_transaction(
    tx: &mut Transaction<'_, Any>,
    request_id: Uuid,
    now: i64,
    event_kind: &'static str,
) -> Result<(), AppError> {
    debug_assert!(matches!(event_kind, "archive_bound" | "archive_gap"));
    let request_id = request_id.to_string();
    let owner = sqlx::query(
        "SELECT r.tenant_id, locator.key_id FROM request_records r JOIN request_record_locators locator ON locator.id = r.id AND locator.tenant_id = r.tenant_id JOIN response_archive_spools s ON s.request_id = r.id AND s.tenant_id = r.tenant_id AND s.reservation_id = r.reservation_id WHERE r.id = $1",
    )
    .bind(&request_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(owner) = owner else {
        return Ok(());
    };
    let tenant_id: String = owner.try_get("tenant_id")?;
    let key_id: String = owner.try_get("key_id")?;
    let cursor = allocate_request_event_cursor(tx, now, &tenant_id, &key_id, &request_id).await?;
    let inserted = sqlx::query(
        "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) SELECT $1, tenant_id, key_id, id, $2, $3, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_records WHERE id = $4 AND tenant_id = $5 AND key_id = $6",
    )
    .bind(cursor.event_id)
    .bind(cursor.event_at)
    .bind(event_kind)
    .bind(request_id)
    .bind(tenant_id)
    .bind(key_id)
    .execute(&mut **tx)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(AppError::Internal);
    }
    Ok(())
}

async fn spool_row(
    tx: &mut Transaction<'_, Any>,
    id: ArchiveSpoolIdentity,
) -> Result<Option<AnyRow>, AppError> {
    Ok(sqlx::query("SELECT * FROM response_archive_spools WHERE request_id = $1 AND tenant_id = $2 AND reservation_id = $3")
        .bind(id.request_id.to_string()).bind(id.tenant_id.to_string()).bind(id.reservation_id.to_string()).fetch_optional(&mut **tx).await?)
}

async fn live_task(
    tx: &mut Transaction<'_, Any>,
    task: &ArchiveSpoolTask,
    now: i64,
) -> Result<bool, AppError> {
    let Some(row) = spool_row(tx, task.identity).await? else {
        return Ok(false);
    };
    Ok(row.try_get::<String, _>("state")? == "uploading"
        && row.try_get::<Option<String>, _>("lease_owner")?.as_deref()
            == Some(task.lease_owner.to_string().as_str())
        && row.try_get::<Option<String>, _>("lease_token")?.as_deref()
            == Some(task.lease_token.to_string().as_str())
        && row
            .try_get::<Option<i64>, _>("lease_expires_at")?
            .is_some_and(|expiry| expiry > now)
        && row.try_get::<i64, _>("expires_at")? > now
        && row.try_get::<i64, _>("chunk_count")? == task.chunk_count
        && row.try_get::<i64, _>("byte_count")? == task.byte_count)
}

fn identity_from_row(row: &AnyRow) -> Result<ArchiveSpoolIdentity, AppError> {
    let parse = |name| -> Result<Uuid, AppError> {
        Uuid::parse_str(&row.try_get::<String, _>(name)?).map_err(|_| AppError::Internal)
    };
    Ok(ArchiveSpoolIdentity {
        request_id: parse("request_id")?,
        tenant_id: parse("tenant_id")?,
        reservation_id: parse("reservation_id")?,
    })
}

fn reason_code(reason: &str) -> &'static str {
    match reason {
        "capacity" => "capacity",
        "capture_timeout" => "capture_timeout",
        "capture_failed" => "capture_failed",
        "upload_failed" => "upload_failed",
        "decrypt_failed" => "decrypt_failed",
        "lease_lost" => "lease_lost",
        "invalid_chunk" => "invalid_chunk",
        _ => "internal",
    }
}

#[cfg(test)]
mod tests;
