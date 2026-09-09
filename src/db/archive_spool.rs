//! Bounded encrypted spool. Every mutation/read of a lease is serialized against
//! the singleton budget row (SQLite's immediate transaction is the equivalent).
//! No transaction encompasses object-storage I/O.
use sqlx::{Any, Row, Transaction, any::AnyRow};
use uuid::Uuid;

use super::{AppError, Database, DatabaseBackend};
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
        sqlx::query("UPDATE response_archive_spools SET state = 'gap', last_error_code = $1, updated_at = $2, expires_at = $3, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL WHERE request_id = $4 AND tenant_id = $5 AND reservation_id = $6 AND state = 'capturing'")
            .bind(reason_code(reason)).bind(now).bind(now + CAPTURE_TTL).bind(identity.request_id.to_string())
            .bind(identity.tenant_id.to_string()).bind(identity.reservation_id.to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn claim_response_archive_spool(
        &self,
        lease_owner: Uuid,
    ) -> Result<Option<ArchiveSpoolTask>, AppError> {
        let (mut tx, now) = self.spool_transaction().await?;
        let row = sqlx::query("SELECT s.* FROM response_archive_spools s WHERE s.expires_at > $1 AND s.attempts < 10 AND ((s.state = 'pending' AND s.next_attempt_at <= $1) OR (s.state = 'uploading' AND s.lease_expires_at <= $1)) AND EXISTS (SELECT 1 FROM request_records r WHERE r.id = s.request_id AND r.tenant_id = s.tenant_id AND r.reservation_id = s.reservation_id AND r.completed_at IS NOT NULL AND r.response_object = 'gap://' || s.request_id || '/response') ORDER BY s.next_attempt_at, s.request_id LIMIT 1")
            .bind(now).fetch_optional(&mut *tx).await?;
        let Some(row) = row else { return Ok(None) };
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

    pub(crate) async fn load_response_archive_spool_chunk(
        &self,
        task: &ArchiveSpoolTask,
        seq: i64,
    ) -> Result<Option<ArchiveSpoolChunk>, AppError> {
        let (mut tx, now) = self.spool_transaction().await?;
        if !live_task(&mut tx, task, now).await? {
            return Ok(None);
        }
        let row = sqlx::query("SELECT seq, ciphertext, byte_count FROM response_archive_spool_chunks WHERE request_id = $1 AND seq = $2")
            .bind(task.identity.request_id.to_string()).bind(seq).fetch_optional(&mut *tx).await?;
        let chunk = row
            .map(|r| -> Result<_, AppError> {
                Ok(ArchiveSpoolChunk {
                    seq: r.try_get("seq")?,
                    ciphertext: r.try_get("ciphertext")?,
                    byte_count: r.try_get("byte_count")?,
                })
            })
            .transpose()?;
        tx.commit().await?;
        Ok(chunk)
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
        sqlx::query("UPDATE response_archive_spools SET state = 'bound', bound_locator = $1, updated_at = $2, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL WHERE request_id = $3")
            .bind(locator).bind(now).bind(task.identity.request_id.to_string()).execute(&mut *tx).await?;
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
        sqlx::query("UPDATE response_archive_spools SET state = $1, next_attempt_at = $2, updated_at = $3, last_error_code = $4, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL WHERE request_id = $5")
            .bind(if attempts >= 10 { "gap" } else { "pending" }).bind(now + backoff).bind(now).bind(reason_code(reason)).bind(task.identity.request_id.to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn cleanup_response_archive_spools(
        &self,
        limit: i64,
    ) -> Result<u64, AppError> {
        // Expiry/attempt exhaustion deliberately discards the encrypted source:
        // the retained gap audit is permanent, not a retryable historical job.
        // Bound rows keep their archive locator and their staging ownership.
        let (mut tx, now) = self.spool_transaction().await?;
        let rows = sqlx::query("SELECT request_id, cipher_bytes, state FROM response_archive_spools WHERE cleaned_at IS NULL AND (state = 'bound' OR expires_at <= $1 OR (state = 'uploading' AND attempts >= 10 AND lease_expires_at <= $1)) ORDER BY expires_at, request_id LIMIT $2")
            .bind(now).bind(limit.clamp(0, 32)).fetch_all(&mut *tx).await?;
        for row in &rows {
            let id: String = row.try_get("request_id")?;
            let bytes: i64 = row.try_get("cipher_bytes")?;
            sqlx::query("DELETE FROM response_archive_spool_chunks WHERE request_id = $1")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes - $1 WHERE singleton = 1")
                .bind(bytes).execute(&mut *tx).await?;
            sqlx::query("UPDATE response_archive_spools SET state = $1, cleaned_at = $2, cipher_bytes = 0, updated_at = $2, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL WHERE request_id = $3")
                .bind(if row.try_get::<String, _>("state")? == "bound" { "bound" } else { "gap" }).bind(now).bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(rows.len() as u64)
    }

    async fn spool_transaction(&self) -> Result<(Transaction<'_, Any>, i64), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        // Always first: one common lock order for quota, append, lease and GC.
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
