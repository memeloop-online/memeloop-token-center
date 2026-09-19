//! Bounded encrypted spool. Buffered lifecycle transactions use durable private
//! reservations; streaming append and GC update the global counter only at
//! their transaction tail. No global budget lock spans account/session work or
//! buffered compression. No transaction encompasses object I/O.
use crate::response_archive_spool::BufferedArchivePurpose;
mod hold_diagnostics;
mod reservations;
pub(crate) use hold_diagnostics::BudgetHold;
pub(crate) use reservations::ArchiveBudgetReservation;

use std::time::{Duration, Instant};

use sqlx::{Any, Row, Transaction, any::AnyRow};
use uuid::Uuid;

use super::{AppError, Database, DatabaseBackend, allocate_request_event_cursor};
#[cfg(test)]
use crate::archive_staging::ArchiveStagingPurpose;
use crate::archive_staging::{ArchiveStagingOwner, ArchiveStagingWriteLease};

const PLAIN_LIMIT: i64 = 64 * 1024 * 1024;
/// Bodies above the default Responses envelope remain valid inference input,
/// but retaining them during a configured larger-ingress deployment would let
/// a few conversations monopolize the durable spool. They therefore keep only
/// irreversible gap evidence.
pub(crate) const REQUEST_ARCHIVE_PLAIN_LIMIT: usize = 16 * 1024 * 1024;
const CIPHER_CHUNK_LIMIT: usize = 512 * 1024;
const CIPHER_LIMIT: i64 = 256 * 1024 * 1024;
pub(super) const REQUEST_CIPHER_LIMIT: i64 = CIPHER_LIMIT / 2;
pub(super) const RESPONSE_CIPHER_LIMIT: i64 = CIPHER_LIMIT - REQUEST_CIPHER_LIMIT;
pub(super) const ARCHIVE_SLOT_LIMIT: i64 = 4096;
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
    pub purpose: BufferedArchivePurpose,
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

pub(crate) async fn insert_request_archive_gap_in_transaction(
    tx: &mut Transaction<'_, Any>,
    now: i64,
    identity: ArchiveSpoolIdentity,
    body: &bytes::Bytes,
    reason: &'static str,
) -> Result<(), AppError> {
    if !matches!(reason, "capacity" | "retention_limit") {
        return Err(AppError::Internal);
    }
    let byte_count = i64::try_from(body.len()).map_err(|_| AppError::Internal)?;
    let digest = blake3::hash(body).to_hex().to_string();
    let inserted = sqlx::query(
        "INSERT INTO request_archive_spools (
            request_id, tenant_id, reservation_id, state, chunk_count,
            byte_count, cipher_bytes, attempts, next_attempt_at, created_at,
            updated_at, expires_at, cleaned_at, last_error_code, gap_reason,
            body_byte_count, body_blake3
         )
         SELECT $1, $2, $3, 'gap', 0, 0, 0, 0, $4, $4, $4, $4, $4,
                'capacity', $5, $6, $7
         WHERE EXISTS (
             SELECT 1 FROM request_records
             WHERE id = $1 AND tenant_id = $2 AND reservation_id = $3
               AND completed_at IS NULL
         )",
    )
    .bind(identity.request_id.to_string())
    .bind(identity.tenant_id.to_string())
    .bind(identity.reservation_id.to_string())
    .bind(now)
    .bind(reason)
    .bind(byte_count)
    .bind(digest)
    .execute(&mut **tx)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(AppError::Internal);
    }
    Ok(())
}

impl Database {
    /// Atomic buffered capture: no partially captured row is ever visible.
    /// The global budget serializes admission for both archive purposes.
    #[cfg(test)]
    pub(crate) async fn capture_buffered_archive_spool(
        &self,
        identity: ArchiveSpoolIdentity,
        purpose: BufferedArchivePurpose,
        chunks: &[ArchiveSpoolChunk],
    ) -> Result<bool, AppError> {
        let (mut tx, now, mut hold) = self
            .tracked_spool_transaction("buffered_capture", Some(identity.request_id))
            .await?;
        hold.phase("capture");
        let captured = self
            .capture_buffered_archive_spool_in_transaction(&mut tx, now, identity, purpose, chunks)
            .await?;
        hold.commit(tx).await?;
        Ok(captured)
    }

    #[cfg(test)]
    pub(super) async fn capture_buffered_archive_spool_in_transaction(
        &self,
        tx: &mut Transaction<'_, Any>,
        now: i64,
        identity: ArchiveSpoolIdentity,
        purpose: BufferedArchivePurpose,
        chunks: &[ArchiveSpoolChunk],
    ) -> Result<bool, AppError> {
        let mut bytes = 0_i64;
        let mut accounted = SPOOL_OVERHEAD;
        if chunks.len() > CHUNK_LIMIT as usize {
            return Ok(false);
        }
        for (seq, chunk) in chunks.iter().enumerate() {
            if chunk.seq != seq as i64
                || !(1..=65536).contains(&chunk.byte_count)
                || chunk.ciphertext.is_empty()
                || chunk.ciphertext.len() > CIPHER_CHUNK_LIMIT
            {
                return Err(AppError::Internal);
            }
            bytes = bytes
                .checked_add(chunk.byte_count)
                .ok_or(AppError::Internal)?;
            accounted = accounted
                .checked_add(chunk.ciphertext.len() as i64 + CHUNK_OVERHEAD)
                .ok_or(AppError::Internal)?;
        }
        if bytes > PLAIN_LIMIT || accounted > CIPHER_LIMIT {
            return Ok(false);
        }
        let valid: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_records WHERE id = $1 AND tenant_id = $2 AND reservation_id = $3 AND completed_at IS NULL")
            .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string())
            .bind(identity.reservation_id.to_string()).fetch_one(&mut **tx).await?;
        if valid != 1 {
            return Err(AppError::Internal);
        }
        if let Some(row) = spool_row(tx, identity, purpose).await? {
            // A repeated call can acknowledge only this exact sealed batch.
            if row.try_get::<String, _>("state")? != "pending"
                || row.try_get::<i64, _>("chunk_count")? != chunks.len() as i64
                || row.try_get::<i64, _>("byte_count")? != bytes
                || row.try_get::<i64, _>("expires_at")? <= now
            {
                return Err(AppError::Internal);
            }
            for chunk in chunks {
                let same: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(spool_sql(purpose, "SELECT COUNT(*) FROM response_archive_spool_chunks WHERE request_id = $1 AND seq = $2 AND ciphertext = $3 AND byte_count = $4")))
                    .bind(identity.request_id.to_string()).bind(chunk.seq).bind(&chunk.ciphertext).bind(chunk.byte_count).fetch_one(&mut **tx).await?;
                if same != 1 {
                    return Err(AppError::Internal);
                }
            }
            return Ok(true);
        }
        let budget = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes + $1 WHERE singleton = 1 AND cipher_bytes <= $2")))
            .bind(accounted).bind(CIPHER_LIMIT - accounted).execute(&mut **tx).await?;
        if budget.rows_affected() != 1 {
            return Ok(false);
        }
        sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "INSERT INTO response_archive_spools (request_id, tenant_id, reservation_id, state, chunk_count, byte_count, cipher_bytes, next_attempt_at, created_at, updated_at, expires_at) VALUES ($1, $2, $3, 'pending', $4, $5, $6, $7, $7, $7, $8)")))
            .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(identity.reservation_id.to_string())
            .bind(chunks.len() as i64).bind(bytes).bind(accounted).bind(now).bind(now + RETENTION).execute(&mut **tx).await?;
        // 512 binds per statement remains below SQLite's conservative 999
        // limit, while avoiding one database round trip per 64KiB chunk.
        for batch in chunks.chunks(128) {
            let values = (0..batch.len())
                .map(|index| {
                    let base = index * 4;
                    format!(
                        "(${}, ${}, ${}, ${})",
                        base + 1,
                        base + 2,
                        base + 3,
                        base + 4
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let statement = spool_sql(
                purpose,
                &format!(
                    "INSERT INTO response_archive_spool_chunks (request_id, seq, ciphertext, byte_count) VALUES {values}"
                ),
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(statement));
            for chunk in batch {
                query = query
                    .bind(identity.request_id.to_string())
                    .bind(chunk.seq)
                    .bind(&chunk.ciphertext)
                    .bind(chunk.byte_count);
            }
            query.execute(&mut **tx).await?;
        }
        Ok(true)
    }

    /// Captures a large body without retaining its amplified ciphertext in
    /// memory. Per-chunk nonces live in `archive`, so a transaction retry after
    /// an unknown COMMIT acknowledgement regenerates identical ciphertext.
    #[cfg(test)]
    pub(super) async fn capture_buffered_archive_body_in_transaction(
        &self,
        tx: &mut Transaction<'_, Any>,
        now: i64,
        archive: &crate::response_archive_spool::BufferedArchive<'_>,
        prepared_first_batch: Option<crate::response_archive_spool::PreparedArchiveBatch>,
    ) -> Result<bool, AppError> {
        self.capture_reserved_buffered_archive_body_in_transaction(
            tx,
            now,
            archive,
            prepared_first_batch,
            None,
        )
        .await
    }

    pub(super) async fn capture_reserved_buffered_archive_body_in_transaction(
        &self,
        tx: &mut Transaction<'_, Any>,
        now: i64,
        archive: &crate::response_archive_spool::BufferedArchive<'_>,
        prepared_first_batch: Option<crate::response_archive_spool::PreparedArchiveBatch>,
        reservation: Option<&ArchiveBudgetReservation>,
    ) -> Result<bool, AppError> {
        let identity = archive.identity();
        let purpose = archive.purpose();
        let body = archive.body();
        let chunk_count = body
            .len()
            .div_ceil(crate::response_archive_spool::CHUNK_BYTES);
        if chunk_count > CHUNK_LIMIT as usize || body.len() > PLAIN_LIMIT as usize {
            return Ok(false);
        }
        let mut accounted = SPOOL_OVERHEAD;
        for bytes in body.chunks(crate::response_archive_spool::CHUNK_BYTES) {
            let cipher_bytes = archive.sealed_len(bytes.len()).ok_or(AppError::Internal)?;
            accounted = accounted
                .checked_add(cipher_bytes as i64 + CHUNK_OVERHEAD)
                .ok_or(AppError::Internal)?;
        }
        if accounted > CIPHER_LIMIT {
            return Ok(false);
        }
        let prepared_first_batch = prepared_first_batch
            .map(|prepared| prepared.into_chunks_for(archive).ok_or(AppError::Internal))
            .transpose()?;
        let expected_prepared =
            chunk_count.min(crate::response_archive_spool::CAPTURE_INSERT_BATCH_CHUNKS);
        if let Some(chunks) = prepared_first_batch.as_deref() {
            if chunks.len() != expected_prepared {
                return Err(AppError::Internal);
            }
            for (seq, chunk) in chunks.iter().enumerate() {
                let start = seq
                    .checked_mul(crate::response_archive_spool::CHUNK_BYTES)
                    .ok_or(AppError::Internal)?;
                let expected_bytes = body
                    .len()
                    .saturating_sub(start)
                    .min(crate::response_archive_spool::CHUNK_BYTES);
                if chunk.seq != i64::try_from(seq).map_err(|_| AppError::Internal)?
                    || chunk.byte_count
                        != i64::try_from(expected_bytes).map_err(|_| AppError::Internal)?
                {
                    return Err(AppError::Internal);
                }
            }
        }
        let byte_count = i64::try_from(body.len()).map_err(|_| AppError::Internal)?;
        let valid: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_records WHERE id = $1 AND tenant_id = $2 AND reservation_id = $3 AND completed_at IS NULL")
            .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string())
            .bind(identity.reservation_id.to_string()).fetch_one(&mut **tx).await?;
        if valid != 1 {
            return Err(AppError::Internal);
        }
        if let Some(row) = spool_row(tx, identity, purpose).await? {
            if row.try_get::<String, _>("state")? != "pending"
                || row.try_get::<i64, _>("chunk_count")? != chunk_count as i64
                || row.try_get::<i64, _>("byte_count")? != byte_count
                || row.try_get::<i64, _>("expires_at")? <= now
            {
                return Err(AppError::Internal);
            }
            let mut actual_accounted = SPOOL_OVERHEAD;
            for (seq, bytes) in body
                .chunks(crate::response_archive_spool::CHUNK_BYTES)
                .enumerate()
            {
                let ciphertext = archive.seal(seq)?;
                add_chunk_accounting(&mut actual_accounted, &ciphertext)?;
                let same: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(spool_sql(purpose, "SELECT COUNT(*) FROM response_archive_spool_chunks WHERE request_id = $1 AND seq = $2 AND ciphertext = $3 AND byte_count = $4")))
                    .bind(identity.request_id.to_string()).bind(seq as i64).bind(&ciphertext)
                    .bind(bytes.len() as i64).fetch_one(&mut **tx).await?;
                if same != 1 {
                    return Err(AppError::Internal);
                }
            }
            if row.try_get::<i64, _>("cipher_bytes")? != actual_accounted {
                return Err(AppError::Internal);
            }
            return Ok(true);
        }
        let reserved = match reservation {
            Some(reservation) => reservation.consume(tx, identity, purpose, accounted).await?,
            None => sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes + $1 WHERE singleton = 1 AND cipher_bytes <= $2")))
                .bind(accounted).bind(CIPHER_LIMIT - accounted).execute(&mut **tx).await?.rows_affected() == 1,
        };
        if !reserved {
            return Ok(false);
        }
        sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "INSERT INTO response_archive_spools (request_id, tenant_id, reservation_id, state, chunk_count, byte_count, cipher_bytes, next_attempt_at, created_at, updated_at, expires_at) VALUES ($1, $2, $3, 'pending', $4, $5, $6, $7, $7, $7, $8)")))
            .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(identity.reservation_id.to_string())
            .bind(chunk_count as i64).bind(byte_count).bind(accounted).bind(now).bind(now + RETENTION).execute(&mut **tx).await?;
        let mut actual_accounted = SPOOL_OVERHEAD;
        let mut first_seq = 0;
        if let Some(chunks) = prepared_first_batch {
            add_chunks_accounting(&mut actual_accounted, &chunks)?;
            insert_spool_chunks(tx, purpose, identity, &chunks).await?;
            first_seq = chunks.len();
            drop(chunks);
        }
        while first_seq < chunk_count {
            let end_seq = (first_seq + crate::response_archive_spool::CAPTURE_INSERT_BATCH_CHUNKS)
                .min(chunk_count);
            let chunks = (first_seq..end_seq)
                .map(|seq| {
                    let start = seq * crate::response_archive_spool::CHUNK_BYTES;
                    let end = (start + crate::response_archive_spool::CHUNK_BYTES).min(body.len());
                    Ok(ArchiveSpoolChunk {
                        seq: seq as i64,
                        ciphertext: archive.seal(seq)?,
                        byte_count: (end - start) as i64,
                    })
                })
                .collect::<Result<Vec<_>, AppError>>()?;
            add_chunks_accounting(&mut actual_accounted, &chunks)?;
            insert_spool_chunks(tx, purpose, identity, &chunks).await?;
            first_seq = end_seq;
        }
        let refund = accounted
            .checked_sub(actual_accounted)
            .ok_or(AppError::Internal)?;
        if refund > 0 {
            let spool = sqlx::query(sqlx::AssertSqlSafe(spool_sql(
                purpose,
                "UPDATE response_archive_spools SET cipher_bytes = $1 WHERE request_id = $2 AND tenant_id = $3 AND reservation_id = $4 AND state = 'pending' AND cipher_bytes = $5",
            )))
            .bind(actual_accounted)
            .bind(identity.request_id.to_string())
            .bind(identity.tenant_id.to_string())
            .bind(identity.reservation_id.to_string())
            .bind(accounted)
            .execute(&mut **tx)
            .await?;
            if spool.rows_affected() != 1 {
                return Err(AppError::Internal);
            }
            if let Some(reservation) = reservation {
                reservation.refund_in_transaction(tx, refund).await?;
            } else {
                let budget = sqlx::query(sqlx::AssertSqlSafe(spool_sql(
                    purpose,
                    "UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes - $1 WHERE singleton = 1 AND cipher_bytes >= $1",
                )))
                .bind(refund)
                .execute(&mut **tx)
                .await?;
                if budget.rows_affected() != 1 {
                    return Err(AppError::Internal);
                }
            }
        }
        Ok(true)
    }

    pub(crate) async fn begin_response_archive_spool(
        &self,
        identity: ArchiveSpoolIdentity,
    ) -> Result<bool, AppError> {
        let purpose = BufferedArchivePurpose::Response;
        // Validate request ownership and exact replays before touching the
        // cross-request budget row. The provisional budget update then owns
        // the short byte-and-slot decision and rolls back with any rejected
        // spool insert.
        let mut tx = self.archive_state_transaction().await?;
        let mut hold = BudgetHold::late("response_begin", Some(identity.request_id));
        let now = archive_clock(&mut tx, self.backend).await?;
        hold.phase("request_and_spool_owner");
        // The response writer is owned independently from the proxy lifecycle.
        // A short stream may therefore finalize its request before the writer's
        // begin transaction acquires the global spool budget. The canonical gap
        // locator is the exact, fenced placeholder that finalization writes
        // until the worker binds a durable object; no other completed request
        // may be reopened for capture.
        let gap_locator = format!("gap://{}/response", identity.request_id);
        let valid: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(spool_sql(purpose, "SELECT COUNT(*) FROM request_records WHERE id = $1 AND tenant_id = $2 AND reservation_id = $3 AND (completed_at IS NULL OR response_object = $4)")))
            .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string())
            .bind(identity.reservation_id.to_string()).bind(gap_locator).fetch_one(&mut *tx).await?;
        if valid != 1 {
            return Ok(spool_write_rejected(
                identity,
                "begin",
                "request_owner_not_eligible",
            ));
        }
        // Existing audit rows cannot be reopened, and exact retries must not
        // charge the fixed admission overhead a second time.
        if let Some(row) = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "SELECT tenant_id, reservation_id, state, expires_at FROM response_archive_spools WHERE request_id = $1")))
            .bind(identity.request_id.to_string()).fetch_optional(&mut *tx).await?
        {
            let accepted = row.try_get::<String, _>("tenant_id")? == identity.tenant_id.to_string()
                && row.try_get::<String, _>("reservation_id")? == identity.reservation_id.to_string()
                && row.try_get::<String, _>("state")? == "capturing"
                && row.try_get::<i64, _>("expires_at")? > now;
            if !accepted { spool_write_rejected(identity, "begin", "existing_spool_not_eligible"); }
            return Ok(accepted);
        }
        hold.phase("budget_and_slot_admission");
        let budget = sqlx::query(
            "UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes + $1
             WHERE singleton = 1
               AND cipher_bytes <= $2
               AND cipher_bytes - request_cipher_bytes <= $3",
        )
        .bind(SPOOL_OVERHEAD)
        .bind(CIPHER_LIMIT - SPOOL_OVERHEAD)
        .bind(RESPONSE_CIPHER_LIMIT - SPOOL_OVERHEAD)
        .execute(&mut *tx)
        .await?;
        if budget.rows_affected() != 1 {
            BudgetHold::rollback_optional(tx, Some(hold)).await?;
            return Ok(spool_write_rejected(
                identity,
                "begin",
                "global_cipher_capacity",
            ));
        }
        let active_slots: i64 = sqlx::query_scalar(
            "SELECT
                (SELECT COUNT(*) FROM response_archive_spools
                 WHERE cleaned_at IS NULL AND state IN ('capturing', 'pending', 'uploading'))
              + (SELECT COUNT(*) FROM archive_budget_reservations
                 WHERE purpose = 'response')",
        )
        .fetch_one(&mut *tx)
        .await?;
        if active_slots >= ARCHIVE_SLOT_LIMIT {
            BudgetHold::rollback_optional(tx, Some(hold)).await?;
            return Ok(spool_write_rejected(
                identity,
                "begin",
                "response_slot_capacity",
            ));
        }
        hold.phase("spool_insert");
        let inserted = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "INSERT INTO response_archive_spools (request_id, tenant_id, reservation_id, state, next_attempt_at, created_at, updated_at, expires_at, cipher_bytes) VALUES ($1, $2, $3, 'capturing', $4, $4, $4, $5, $6) ON CONFLICT(request_id) DO NOTHING")))
            .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string())
            .bind(identity.reservation_id.to_string()).bind(now).bind(now + CAPTURE_TTL)
            .bind(SPOOL_OVERHEAD)
            .execute(&mut *tx).await?;
        if inserted.rows_affected() == 0 {
            // A same-identity begin may win after our optimistic read. Recheck
            // the committed owner instead of surfacing a unique-key error or
            // charging the fixed budget overhead twice.
            let row = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "SELECT tenant_id, reservation_id, state, expires_at FROM response_archive_spools WHERE request_id = $1")))
                .bind(identity.request_id.to_string()).fetch_one(&mut *tx).await?;
            let accepted = row.try_get::<String, _>("tenant_id")? == identity.tenant_id.to_string()
                && row.try_get::<String, _>("reservation_id")?
                    == identity.reservation_id.to_string()
                && row.try_get::<String, _>("state")? == "capturing"
                && row.try_get::<i64, _>("expires_at")? > now;
            BudgetHold::rollback_optional(tx, Some(hold)).await?;
            if !accepted {
                spool_write_rejected(identity, "begin", "existing_spool_not_eligible");
            }
            return Ok(accepted);
        }
        hold.commit(tx).await?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) async fn append_response_archive_spool(
        &self,
        identity: ArchiveSpoolIdentity,
        seq: i64,
        byte_count: i64,
        ciphertext: &str,
    ) -> Result<bool, AppError> {
        self.append_response_archive_spool_batch(
            identity,
            &[ArchiveSpoolChunk {
                seq,
                byte_count,
                ciphertext: ciphertext.to_owned(),
            }],
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn append_response_archive_spool_batch(
        &self,
        identity: ArchiveSpoolIdentity,
        chunks: &[ArchiveSpoolChunk],
    ) -> Result<bool, AppError> {
        let Some(first) = chunks.first() else {
            return Ok(spool_write_rejected(identity, "append", "invalid_chunk"));
        };
        self.append_response_archive_spool_batch_with(identity, first.seq, chunks.len(), |seq| {
            let offset = usize::try_from(seq - first.seq).map_err(|_| AppError::Internal)?;
            let chunk = chunks.get(offset).ok_or(AppError::Internal)?;
            Ok(ArchiveSpoolChunk {
                seq: chunk.seq,
                byte_count: chunk.byte_count,
                ciphertext: chunk.ciphertext.clone(),
            })
        })
        .await
    }

    pub(crate) async fn append_response_archive_spool_batch_with<F>(
        &self,
        identity: ArchiveSpoolIdentity,
        first_seq: i64,
        chunk_count: usize,
        mut next_chunk: F,
    ) -> Result<bool, AppError>
    where
        F: FnMut(i64) -> Result<ArchiveSpoolChunk, AppError> + Send,
    {
        let purpose = BufferedArchivePurpose::Response;
        if first_seq < 0
            || chunk_count == 0
            || chunk_count > crate::response_archive_spool::CAPTURE_DATABASE_BATCH_CHUNKS
        {
            return Ok(spool_write_rejected(identity, "append", "invalid_chunk"));
        }
        // Serialize chunks on their own spool first. The singleton budget is
        // updated last, so its row lock covers only the capacity check and
        // commit rather than every request-local validation and write.
        let mut tx = self.archive_state_transaction().await?;
        let mut hold = BudgetHold::late("response_append", Some(identity.request_id));
        let now = archive_clock(&mut tx, self.backend).await?;
        hold.phase("spool_owner_and_append");
        let Some(row) = locked_spool_row(&mut tx, self.backend, identity, purpose).await? else {
            return Ok(spool_write_rejected(
                identity,
                "append",
                "spool_owner_missing",
            ));
        };
        if row.try_get::<String, _>("state")? != "capturing"
            || row.try_get::<i64, _>("expires_at")? <= now
        {
            return Ok(spool_write_rejected(
                identity,
                "append",
                "spool_not_capturing_or_expired",
            ));
        }
        let count: i64 = row.try_get("chunk_count")?;
        let batch_count = i64::try_from(chunk_count).map_err(|_| AppError::Internal)?;
        let validate = |chunk: &ArchiveSpoolChunk, expected_seq: i64| {
            chunk.seq == expected_seq
                && chunk.byte_count > 0
                && chunk.byte_count <= PLAIN_LIMIT
                && !chunk.ciphertext.is_empty()
                && chunk.ciphertext.len() <= CIPHER_CHUNK_LIMIT
        };
        if first_seq < count {
            let end = first_seq
                .checked_add(batch_count)
                .ok_or(AppError::Internal)?;
            let mut accepted = end <= count;
            if accepted {
                for offset in 0..batch_count {
                    let expected_seq = first_seq.checked_add(offset).ok_or(AppError::Internal)?;
                    let chunk = next_chunk(expected_seq)?;
                    if !validate(&chunk, expected_seq) {
                        accepted = false;
                        break;
                    }
                    let replay = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "SELECT ciphertext, byte_count FROM response_archive_spool_chunks WHERE request_id = $1 AND seq = $2")))
                        .bind(identity.request_id.to_string()).bind(chunk.seq).fetch_optional(&mut *tx).await?;
                    accepted &= replay.is_some_and(|row| {
                        row.get::<String, _>("ciphertext") == chunk.ciphertext.as_str()
                            && row.get::<i64, _>("byte_count") == chunk.byte_count
                    });
                    if !accepted {
                        break;
                    }
                }
            }
            if !accepted {
                spool_write_rejected(identity, "append", "replay_mismatch");
            }
            return Ok(accepted);
        }
        if first_seq != count || count > CHUNK_LIMIT - batch_count {
            return Ok(spool_write_rejected(
                identity,
                "append",
                "sequence_or_plain_capacity",
            ));
        }
        let existing_bytes = row.try_get::<i64, _>("byte_count")?;
        let mut byte_count = 0_i64;
        let mut cipher_bytes = 0_i64;
        for offset in 0..batch_count {
            let expected_seq = first_seq.checked_add(offset).ok_or(AppError::Internal)?;
            let chunk = next_chunk(expected_seq)?;
            if !validate(&chunk, expected_seq) {
                return Ok(spool_write_rejected(identity, "append", "invalid_chunk"));
            }
            byte_count = byte_count
                .checked_add(chunk.byte_count)
                .ok_or(AppError::Internal)?;
            if existing_bytes > PLAIN_LIMIT - byte_count {
                return Ok(spool_write_rejected(
                    identity,
                    "append",
                    "sequence_or_plain_capacity",
                ));
            }
            cipher_bytes = cipher_bytes
                .checked_add(
                    i64::try_from(chunk.ciphertext.len()).map_err(|_| AppError::Internal)?
                        + CHUNK_OVERHEAD,
                )
                .ok_or(AppError::Internal)?;
            // Insert each prepared chunk immediately. The transaction remains
            // atomic, while the caller can release its plaintext before the
            // next ciphertext is prepared instead of retaining both batches.
            sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "INSERT INTO response_archive_spool_chunks (request_id, seq, ciphertext, byte_count) VALUES ($1, $2, $3, $4)")))
                .bind(identity.request_id.to_string())
                .bind(chunk.seq)
                .bind(chunk.ciphertext)
                .bind(chunk.byte_count)
                .execute(&mut *tx).await?;
        }
        sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "UPDATE response_archive_spools SET chunk_count = chunk_count + $1, byte_count = byte_count + $2, cipher_bytes = cipher_bytes + $3, updated_at = $4, expires_at = $5 WHERE request_id = $6")))
            .bind(batch_count).bind(byte_count).bind(cipher_bytes).bind(now).bind(now + CAPTURE_TTL).bind(identity.request_id.to_string()).execute(&mut *tx).await?;
        hold.phase("budget_update");
        let budget = sqlx::query(
            "UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes + $1
             WHERE singleton = 1
               AND cipher_bytes <= $2
               AND cipher_bytes - request_cipher_bytes <= $3",
        )
        .bind(cipher_bytes)
        .bind(CIPHER_LIMIT - cipher_bytes)
        .bind(RESPONSE_CIPHER_LIMIT - cipher_bytes)
        .execute(&mut *tx)
        .await?;
        if budget.rows_affected() != 1 {
            BudgetHold::rollback_optional(tx, Some(hold)).await?;
            return Ok(spool_write_rejected(
                identity,
                "append",
                "global_cipher_capacity",
            ));
        }
        hold.commit(tx).await?;
        Ok(true)
    }

    pub(crate) async fn seal_response_archive_spool(
        &self,
        identity: ArchiveSpoolIdentity,
        chunk_count: i64,
        byte_count: i64,
    ) -> Result<bool, AppError> {
        let purpose = BufferedArchivePurpose::Response;
        if chunk_count < 0 || byte_count < 0 {
            return Ok(spool_write_rejected(identity, "seal", "invalid_counts"));
        }
        let mut tx = self.archive_state_transaction().await?;
        let Some(row) = locked_spool_row(&mut tx, self.backend, identity, purpose).await? else {
            return Ok(spool_write_rejected(
                identity,
                "seal",
                "spool_owner_missing",
            ));
        };
        let now = archive_clock(&mut tx, self.backend).await?;
        if row.try_get::<i64, _>("chunk_count")? != chunk_count
            || row.try_get::<i64, _>("byte_count")? != byte_count
            || row.try_get::<i64, _>("expires_at")? <= now
        {
            return Ok(spool_write_rejected(
                identity,
                "seal",
                "counts_mismatch_or_expired",
            ));
        }
        let state: String = row.try_get("state")?;
        if state == "pending" {
            return Ok(true);
        }
        if state != "capturing" {
            return Ok(spool_write_rejected(
                identity,
                "seal",
                "spool_not_capturing",
            ));
        }
        sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "UPDATE response_archive_spools SET state = 'pending', updated_at = $1, next_attempt_at = $1, expires_at = $2 WHERE request_id = $3")))
            .bind(now).bind(now + RETENTION).bind(identity.request_id.to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub(crate) async fn fail_response_archive_spool(
        &self,
        identity: ArchiveSpoolIdentity,
        reason: &str,
    ) -> Result<(), AppError> {
        let purpose = BufferedArchivePurpose::Response;
        let mut tx = self.archive_state_transaction().await?;
        let now = archive_clock(&mut tx, self.backend).await?;
        // A lost seal ACK must not destroy a complete, recoverable pending spool.
        // The audit row and terminal reason remain retained, but incomplete
        // ciphertext can never be uploaded. Make it immediately eligible for
        // the existing expiry-indexed GC so it cannot pin global capacity.
        let changed = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "UPDATE response_archive_spools SET state = 'gap', last_error_code = $1, updated_at = $2, expires_at = $2, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL WHERE request_id = $3 AND tenant_id = $4 AND reservation_id = $5 AND state = 'capturing'")))
            .bind(reason_code(reason)).bind(now).bind(identity.request_id.to_string())
            .bind(identity.tenant_id.to_string()).bind(identity.reservation_id.to_string()).execute(&mut *tx).await?;
        if changed.rows_affected() == 1 {
            emit_response_archive_transition_event_in_transaction(
                &mut tx,
                purpose,
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

    #[cfg(test)]
    pub(crate) async fn claim_response_archive_spool_if(
        &self,
        lease_owner: Uuid,
        admit: impl FnOnce() -> bool,
    ) -> Result<Option<ArchiveSpoolTask>, AppError> {
        self.claim_archive_spool_if(lease_owner, BufferedArchivePurpose::Response, admit)
            .await
    }

    pub(crate) async fn claim_archive_spool_if(
        &self,
        lease_owner: Uuid,
        purpose: BufferedArchivePurpose,
        admit: impl FnOnce() -> bool,
    ) -> Result<Option<ArchiveSpoolTask>, AppError> {
        let mut tx = self.archive_state_transaction().await?;
        let hint_now = archive_clock(&mut tx, self.backend).await?;
        let claim = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT s.* FROM response_archive_spools s WHERE s.expires_at > $1 AND s.attempts < 10 AND ((s.state = 'pending' AND s.next_attempt_at <= $1) OR (s.state = 'uploading' AND s.lease_expires_at <= $1)) AND EXISTS (SELECT 1 FROM request_records r WHERE r.id = s.request_id AND r.tenant_id = s.tenant_id AND r.reservation_id = s.reservation_id AND r.completed_at IS NOT NULL AND r.response_object = 'gap://' || s.request_id || '/response') ORDER BY s.next_attempt_at, s.request_id LIMIT 1 FOR UPDATE OF s SKIP LOCKED"
            }
            DatabaseBackend::Sqlite => {
                "SELECT s.* FROM response_archive_spools s WHERE s.expires_at > $1 AND s.attempts < 10 AND ((s.state = 'pending' AND s.next_attempt_at <= $1) OR (s.state = 'uploading' AND s.lease_expires_at <= $1)) AND EXISTS (SELECT 1 FROM request_records r WHERE r.id = s.request_id AND r.tenant_id = s.tenant_id AND r.reservation_id = s.reservation_id AND r.completed_at IS NOT NULL AND r.response_object = 'gap://' || s.request_id || '/response') ORDER BY s.next_attempt_at, s.request_id LIMIT 1"
            }
        };
        let row = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, claim)))
            .bind(hint_now)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        // Candidate scans may wait on I/O even though locked rows are skipped.
        // Recheck every time-sensitive predicate against the database clock
        // after owning the candidate row.
        let now = archive_clock(&mut tx, self.backend).await?;
        let state: String = row.try_get("state")?;
        let claimable = row.try_get::<i64, _>("expires_at")? > now
            && row.try_get::<i64, _>("attempts")? < 10
            && match state.as_str() {
                "pending" => row.try_get::<i64, _>("next_attempt_at")? <= now,
                "uploading" => row
                    .try_get::<Option<i64>, _>("lease_expires_at")?
                    .is_some_and(|expiry| expiry <= now),
                _ => false,
            };
        if !claimable {
            tx.commit().await?;
            return Ok(None);
        }
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
        sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "UPDATE response_archive_spools SET state = 'uploading', lease_owner = $1, lease_token = $2, lease_expires_at = $3, attempts = attempts + 1, updated_at = $4 WHERE request_id = $5")))
            .bind(lease_owner.to_string()).bind(lease_token.to_string()).bind(now + LEASE_TTL).bind(now).bind(identity.request_id.to_string()).execute(&mut *tx).await?;
        let task = ArchiveSpoolTask {
            identity,
            purpose,
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
        let purpose = task.purpose;
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
        sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, &sql)))
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
        let purpose = task.purpose;
        let mut tx = self.archive_state_transaction().await?;
        let Some((_, now)) = locked_live_task(&mut tx, self.backend, task).await? else {
            return Ok(false);
        };
        sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "UPDATE response_archive_spools SET lease_expires_at = $1, updated_at = $2 WHERE request_id = $3")))
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
        let purpose = task.purpose;
        if staging.key.owner != ArchiveStagingOwner::ProxyRequest(task.identity.request_id)
            || staging.key.purpose != purpose.staging()
        {
            return Ok(false);
        }
        let mut tx = self.archive_state_transaction().await?;
        let Some((_, now)) = locked_live_task(&mut tx, self.backend, task).await? else {
            return Ok(false);
        };
        let changed = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "UPDATE request_records SET response_object = $1 WHERE id = $2 AND tenant_id = $3 AND reservation_id = $4 AND completed_at IS NOT NULL AND response_object = $5")))
            .bind(locator).bind(task.identity.request_id.to_string()).bind(task.identity.tenant_id.to_string())
            .bind(task.identity.reservation_id.to_string()).bind(format!("gap://{}/{}", task.identity.request_id, purpose.as_str())).execute(&mut *tx).await?;
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
        let bound = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "UPDATE response_archive_spools SET state = 'bound', bound_locator = $1, updated_at = $2, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL WHERE request_id = $3 AND tenant_id = $4 AND reservation_id = $5 AND state = 'uploading'")))
            .bind(locator).bind(now).bind(task.identity.request_id.to_string())
            .bind(task.identity.tenant_id.to_string()).bind(task.identity.reservation_id.to_string())
            .execute(&mut *tx).await?;
        if bound.rows_affected() != 1 {
            return Ok(false);
        }
        emit_response_archive_transition_event_in_transaction(
            &mut tx,
            purpose,
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
        let purpose = task.purpose;
        let mut tx = self.archive_state_transaction().await?;
        let Some((row, now)) = locked_live_task(&mut tx, self.backend, task).await? else {
            return Ok(());
        };
        let attempts: i64 = row.try_get("attempts")?;
        let backoff = 5_000_i64 * (1_i64 << attempts.clamp(0, 10) as u32);
        let terminal = attempts >= 10;
        // Terminal retry exhaustion has the same irreversible payload state
        // as producer capture failure: retain audit facts, but let GC reclaim
        // ciphertext through the existing expiry index immediately.
        sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "UPDATE response_archive_spools SET state = $1, next_attempt_at = $2, updated_at = $3, expires_at = CASE WHEN $1 = 'gap' THEN $3 ELSE expires_at END, last_error_code = $4, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL WHERE request_id = $5")))
            .bind(if attempts >= 10 { "gap" } else { "pending" }).bind(now + backoff).bind(now).bind(reason_code(reason)).bind(task.identity.request_id.to_string()).execute(&mut *tx).await?;
        if terminal {
            emit_response_archive_transition_event_in_transaction(
                &mut tx,
                purpose,
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
        for index in 0..limit.clamp(0, 32) {
            // Keep the transaction owned by an independent task. If the
            // worker is cancelled for shutdown while this await is pending,
            // dropping the JoinHandle detaches the batch and lets SQLx finish
            // its COMMIT/rollback protocol before that connection is reused.
            let db = self.clone();
            let batch = tokio::spawn(async move {
                let recovered = db.cleanup_expired_archive_budget_reservation().await?;
                let first = if index % 2 == 0 {
                    BufferedArchivePurpose::Response
                } else {
                    BufferedArchivePurpose::Request
                };
                let second = if first == BufferedArchivePurpose::Response {
                    BufferedArchivePurpose::Request
                } else {
                    BufferedArchivePurpose::Response
                };
                match db.cleanup_archive_spool_batch(first).await? {
                    Some(result) => Ok::<_, AppError>(Some(result)),
                    None => Ok(db
                        .cleanup_archive_spool_batch(second)
                        .await?
                        .or(recovered.then_some(false))),
                }
            })
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

    async fn cleanup_archive_spool_batch(
        &self,
        purpose: BufferedArchivePurpose,
    ) -> Result<Option<bool>, AppError> {
        // Select and mutate one request-owned spool before touching the global
        // counter. Empty GC polls therefore never queue behind active streams,
        // and a real cleanup holds the budget row only for its final decrement
        // and commit. Each transaction remains bounded to 64 chunks/1 MiB.
        let mut tx = self.archive_state_transaction().await?;
        let mut hold = BudgetHold::late("cleanup", None);
        hold.phase("gc_select_and_delete");
        let row = match self.backend {
            DatabaseBackend::PostgreSql => {
                // Each indexed class contributes its oldest unlocked row, and
                // the globally oldest candidate wins. Locking happens during
                // selection so a busy oldest row cannot hide later work and
                // concurrent workers naturally fan out across the queue.
                sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose,
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
                     FROM chosen JOIN response_archive_spools s USING (request_id) CROSS JOIN db_clock c")))
                .fetch_optional(&mut *tx)
                .await?
            }
            DatabaseBackend::Sqlite => {
                // BEGIN IMMEDIATE serializes SQLite writers, so row-level skip
                // locking is neither available nor necessary. Preserve the
                // three partial-index-friendly probes and recheck eligibility
                // using database time inside the write transaction.
                let hint_now = super::unix_millis();
                let bound = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "SELECT request_id, updated_at AS eligible_at FROM response_archive_spools WHERE cleaned_at IS NULL AND state = 'bound' ORDER BY updated_at, request_id LIMIT 1")))
                    .fetch_optional(&mut *tx).await?;
                let expired = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "SELECT request_id, expires_at AS eligible_at FROM response_archive_spools WHERE cleaned_at IS NULL AND expires_at <= $1 ORDER BY expires_at, request_id LIMIT 1")))
                    .bind(hint_now).fetch_optional(&mut *tx).await?;
                let exhausted = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "SELECT request_id, lease_expires_at AS eligible_at FROM response_archive_spools WHERE cleaned_at IS NULL AND state = 'uploading' AND attempts >= 10 AND lease_expires_at <= $1 ORDER BY lease_expires_at, request_id LIMIT 1")))
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
                sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose,
                    "SELECT request_id, state, cipher_bytes, CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER) AS db_now FROM response_archive_spools WHERE request_id = $1 AND cleaned_at IS NULL AND (state = 'bound' OR expires_at <= CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER) OR (state = 'uploading' AND attempts >= 10 AND lease_expires_at <= CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)))")))
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
        let chunks = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, chunk_sql)))
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
            sqlx::query(sqlx::AssertSqlSafe(spool_sql(
                purpose,
                "DELETE FROM response_archive_spool_chunks WHERE request_id = $1 AND seq <= $2",
            )))
            .bind(&id)
            .bind(seq)
            .execute(&mut *tx)
            .await?;
        }
        let remaining: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(spool_sql(purpose, "SELECT COUNT(*) FROM (SELECT seq FROM response_archive_spool_chunks WHERE request_id = $1 LIMIT 1) remaining_chunk")))
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
        sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "UPDATE response_archive_spools SET state = $1, cleaned_at = $2, cipher_bytes = cipher_bytes - $3, updated_at = $4, expires_at = CASE WHEN $5 = 1 THEN expires_at ELSE $4 END, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL WHERE request_id = $6")))
            .bind(if bound { "bound" } else { "gap" }).bind(cleaned.then_some(now))
            .bind(released).bind(now).bind(i64::from(bound)).bind(&id).execute(&mut *tx).await?;
        // The shared budget and this bounded spool mutation still commit or
        // roll back together; acquire it before the global request-event
        // cursor to preserve the budget -> cursor order used by admission and
        // buffered terminal transactions. Empty polls still touch neither.
        hold.phase("budget_update");
        let budget = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes - $1 WHERE singleton = 1 AND cipher_bytes >= $1")))
            .bind(released).execute(&mut *tx).await?;
        if budget.rows_affected() != 1 {
            return Err(AppError::Internal);
        }
        if !matches!(previous_state.as_str(), "bound" | "gap") {
            hold.phase("event_cursor");
            let request_id = Uuid::parse_str(&id).map_err(|_| AppError::Internal)?;
            emit_response_archive_transition_event_in_transaction(
                &mut tx,
                purpose,
                request_id,
                now,
                "archive_gap",
            )
            .await?;
        }
        hold.commit(tx).await?;
        Ok(Some(cleaned))
    }

    #[cfg(test)]
    pub(super) async fn spool_transaction(&self) -> Result<(Transaction<'_, Any>, i64), AppError> {
        let (tx, now, _hold) = self.tracked_spool_transaction("test_capture", None).await?;
        Ok((tx, now))
    }

    #[cfg(test)]
    pub(super) async fn tracked_spool_transaction(
        &self,
        operation: &'static str,
        request_id: Option<Uuid>,
    ) -> Result<(Transaction<'_, Any>, i64, BudgetHold), AppError> {
        let pool_started = Instant::now();
        let mut tx = self.begin_write_transaction().await.inspect_err(|_| {
            tracing::warn!(phase = "archive_budget_acquire", operation,
                request_id = ?request_id, outcome = "transaction_acquire_failed",
                pool_wait_ms = pool_started.elapsed().as_millis() as u64,
                "archive transaction acquisition failed before budget ownership");
        })?;
        let pool_wait_ms = pool_started.elapsed().as_millis() as u64;
        // Buffered admission and terminal capture reserve the budget before
        // acquiring their lifecycle rows. Streaming append and GC deliberately
        // use archive_state_transaction instead and update the budget last;
        // they never precede that update with the global request-event cursor.
        let lock = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT cipher_bytes, CAST(pg_backend_pid() AS BIGINT) AS backend_pid FROM response_archive_spool_budget WHERE singleton = 1 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT cipher_bytes, CAST(NULL AS BIGINT) AS backend_pid FROM response_archive_spool_budget WHERE singleton = 1"
            }
        };
        let lock_started = Instant::now();
        let row = sqlx::query(lock)
            .fetch_one(&mut *tx)
            .await
            .inspect_err(|_| {
                tracing::warn!(phase = "archive_budget_acquire", operation,
                request_id = ?request_id, outcome = "budget_lock_failed", pool_wait_ms,
                budget_wait_ms = lock_started.elapsed().as_millis() as u64,
                "archive budget acquisition failed without confirmed ownership");
            })?;
        let backend_pid: Option<i64> = row.try_get("backend_pid")?;
        let mut hold = BudgetHold::new(operation, request_id, backend_pid);
        let budget_wait_ms = lock_started.elapsed().as_millis() as u64;
        // Per-append successes stay DEBUG. Slow acquisitions are independently
        // visible without logging query values or adding per-chunk INFO I/O.
        if pool_wait_ms >= 250 || budget_wait_ms >= 250 {
            tracing::warn!(
                phase = "archive_budget_acquire",
                operation, request_id = ?request_id, backend_pid = ?backend_pid,
                pool_wait_ms,
                budget_wait_ms,
                "slow archive budget acquisition"
            );
        } else {
            tracing::debug!(
                phase = "archive_budget_acquire",
                pool_wait_ms,
                budget_wait_ms,
                "archive budget acquired"
            );
        }
        let now = archive_clock(&mut tx, self.backend).await?;
        hold.phase("owned_work");
        Ok((tx, now, hold))
    }

    /// State and lease transitions do not change global archive accounting.
    /// Lock only their spool row so slow claims, events, or WAL flushes cannot
    /// block unrelated producer admissions behind the singleton budget row.
    async fn archive_state_transaction(&self) -> Result<Transaction<'_, Any>, AppError> {
        Ok(self.begin_write_transaction().await?)
    }
}

async fn archive_clock(
    tx: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
) -> Result<i64, AppError> {
    let clock = match backend {
        DatabaseBackend::PostgreSql => {
            "SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT)"
        }
        DatabaseBackend::Sqlite => {
            "SELECT CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)"
        }
    };
    Ok(sqlx::query_scalar(clock).fetch_one(&mut **tx).await?)
}

/// Emit a sparse, locator-free convergence signal only after a real terminal
/// spool transition. The same write transaction owns both the state change and
/// the globally monotonic request-event cursor, so rollback cannot expose one
/// without the other. Missing legacy request rows are tolerated for bounded GC
/// of pre-invariant audit data; live spool admission always has this owner row.
async fn emit_response_archive_transition_event_in_transaction(
    tx: &mut Transaction<'_, Any>,
    purpose: BufferedArchivePurpose,
    request_id: Uuid,
    now: i64,
    event_kind: &'static str,
) -> Result<(), AppError> {
    debug_assert!(matches!(event_kind, "archive_bound" | "archive_gap"));
    let request_id = request_id.to_string();
    let owner = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose,
        "SELECT r.tenant_id, locator.key_id FROM request_records r JOIN request_record_locators locator ON locator.id = r.id AND locator.tenant_id = r.tenant_id JOIN response_archive_spools s ON s.request_id = r.id AND s.tenant_id = r.tenant_id AND s.reservation_id = r.reservation_id WHERE r.id = $1")))
    .bind(&request_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(owner) = owner else {
        return Ok(());
    };
    let tenant_id: String = owner.try_get("tenant_id")?;
    let key_id: String = owner.try_get("key_id")?;
    let cursor = allocate_request_event_cursor(tx, now, &tenant_id, &key_id, &request_id).await?;
    let inserted = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose,
        "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) SELECT $1, tenant_id, key_id, id, $2, $3, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_records WHERE id = $4 AND tenant_id = $5 AND key_id = $6")))
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

async fn insert_spool_chunks(
    tx: &mut Transaction<'_, Any>,
    purpose: BufferedArchivePurpose,
    identity: ArchiveSpoolIdentity,
    chunks: &[ArchiveSpoolChunk],
) -> Result<(), AppError> {
    let values = (0..chunks.len())
        .map(|index| {
            let base = index * 4;
            format!(
                "(${}, ${}, ${}, ${})",
                base + 1,
                base + 2,
                base + 3,
                base + 4
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let statement = spool_sql(
        purpose,
        &format!(
            "INSERT INTO response_archive_spool_chunks (request_id, seq, ciphertext, byte_count) VALUES {values}"
        ),
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(statement));
    for chunk in chunks {
        query = query
            .bind(identity.request_id.to_string())
            .bind(chunk.seq)
            .bind(&chunk.ciphertext)
            .bind(chunk.byte_count);
    }
    query.execute(&mut **tx).await?;
    Ok(())
}

fn add_chunks_accounting(
    accounted: &mut i64,
    chunks: &[ArchiveSpoolChunk],
) -> Result<(), AppError> {
    for chunk in chunks {
        add_chunk_accounting(accounted, &chunk.ciphertext)?;
    }
    Ok(())
}

fn add_chunk_accounting(accounted: &mut i64, ciphertext: &str) -> Result<(), AppError> {
    if ciphertext.len() > CIPHER_CHUNK_LIMIT {
        return Err(AppError::Internal);
    }
    let cipher_bytes = i64::try_from(ciphertext.len()).map_err(|_| AppError::Internal)?;
    *accounted = accounted
        .checked_add(cipher_bytes + CHUNK_OVERHEAD)
        .ok_or(AppError::Internal)?;
    Ok(())
}

async fn spool_row(
    tx: &mut Transaction<'_, Any>,
    id: ArchiveSpoolIdentity,
    purpose: BufferedArchivePurpose,
) -> Result<Option<AnyRow>, AppError> {
    Ok(sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, "SELECT * FROM response_archive_spools WHERE request_id = $1 AND tenant_id = $2 AND reservation_id = $3")))
        .bind(id.request_id.to_string()).bind(id.tenant_id.to_string()).bind(id.reservation_id.to_string()).fetch_optional(&mut **tx).await?)
}

async fn locked_spool_row(
    tx: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    id: ArchiveSpoolIdentity,
    purpose: BufferedArchivePurpose,
) -> Result<Option<AnyRow>, AppError> {
    let select = match backend {
        DatabaseBackend::PostgreSql => {
            "SELECT * FROM response_archive_spools WHERE request_id = $1 AND tenant_id = $2 AND reservation_id = $3 FOR UPDATE"
        }
        DatabaseBackend::Sqlite => {
            "SELECT * FROM response_archive_spools WHERE request_id = $1 AND tenant_id = $2 AND reservation_id = $3"
        }
    };
    Ok(sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, select)))
        .bind(id.request_id.to_string())
        .bind(id.tenant_id.to_string())
        .bind(id.reservation_id.to_string())
        .fetch_optional(&mut **tx)
        .await?)
}

async fn locked_live_task(
    tx: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    task: &ArchiveSpoolTask,
) -> Result<Option<(AnyRow, i64)>, AppError> {
    let Some(row) = locked_spool_row(tx, backend, task.identity, task.purpose).await? else {
        return Ok(None);
    };
    let now = archive_clock(tx, backend).await?;
    let live = row.try_get::<String, _>("state")? == "uploading"
        && row.try_get::<Option<String>, _>("lease_owner")?.as_deref()
            == Some(task.lease_owner.to_string().as_str())
        && row.try_get::<Option<String>, _>("lease_token")?.as_deref()
            == Some(task.lease_token.to_string().as_str())
        && row
            .try_get::<Option<i64>, _>("lease_expires_at")?
            .is_some_and(|expiry| expiry > now)
        && row.try_get::<i64, _>("expires_at")? > now
        && row.try_get::<i64, _>("chunk_count")? == task.chunk_count
        && row.try_get::<i64, _>("byte_count")? == task.byte_count;
    Ok(live.then_some((row, now)))
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

fn spool_sql(purpose: BufferedArchivePurpose, sql: &str) -> String {
    match purpose {
        BufferedArchivePurpose::Response => sql.to_owned(),
        BufferedArchivePurpose::Request => sql
            .replace("UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes + $1", "UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes + $1, request_cipher_bytes = request_cipher_bytes + $1")
            .replace("UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes - $1", "UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes - $1, request_cipher_bytes = request_cipher_bytes - $1")
            .replace("response_archive_spools", "request_archive_spools")
            .replace(
                "response_archive_spool_chunks",
                "request_archive_spool_chunks",
            )
            .replace("response_object", "request_object")
            .replace("/response'", "/request'"),
    }
}

fn spool_write_rejected(
    identity: ArchiveSpoolIdentity,
    operation: &'static str,
    reason: &'static str,
) -> bool {
    tracing::warn!(request_id = %identity.request_id, phase = "response_spool_write_rejected", operation, outcome = reason, "response archive write rejected without a database error");
    false
}

fn reason_code(reason: &str) -> &'static str {
    match reason {
        "capacity" => "capacity",
        "capture_timeout" => "capture_timeout",
        "capture_failed" => "capture_failed",
        "upload_timeout" => "upload_timeout",
        "upload_failed" => "upload_failed",
        "decrypt_failed" => "decrypt_failed",
        "lease_lost" => "lease_lost",
        "invalid_chunk" => "invalid_chunk",
        _ => "internal",
    }
}

#[cfg(test)]
mod tests;
