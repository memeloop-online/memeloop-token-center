//! Durable, bounded capacity reservations. Only acquire/refund transactions
//! touch the global counter; the long admission/settlement transaction owns its
//! private reservation row. Stored bytes + unused reservations never exceed the
//! counter. Cancellation/unknown commit is recovered by idempotent refund/GC.
use super::*;

const RESERVATION_TTL: i64 = 10 * 60 * 1000;

pub(crate) struct ArchiveBudgetReservation {
    db: Database,
    id: Uuid,
    identity: ArchiveSpoolIdentity,
    purpose: BufferedArchivePurpose,
    released: std::sync::atomic::AtomicBool,
}

impl ArchiveBudgetReservation {
    pub(super) async fn consume(
        &self,
        tx: &mut Transaction<'_, Any>,
        identity: ArchiveSpoolIdentity,
        purpose: BufferedArchivePurpose,
        amount: i64,
    ) -> Result<bool, AppError> {
        if self.identity != identity || self.purpose != purpose || amount < 0 {
            return Err(AppError::Internal);
        }
        let updated = sqlx::query("UPDATE archive_budget_reservations SET cipher_bytes = cipher_bytes - $1 WHERE id = $2 AND cipher_bytes >= $1")
            .bind(amount).bind(self.id.to_string()).execute(&mut **tx).await?;
        Ok(updated.rows_affected() == 1)
    }

    pub(super) async fn refund_in_transaction(
        &self,
        tx: &mut Transaction<'_, Any>,
        amount: i64,
    ) -> Result<(), AppError> {
        let updated = sqlx::query(
            "UPDATE archive_budget_reservations SET cipher_bytes = cipher_bytes + $1 WHERE id = $2",
        )
        .bind(amount)
        .bind(self.id.to_string())
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::Internal);
        }
        Ok(())
    }

    /// Refund after the lifecycle transaction has committed/rolled back. A
    /// failed refund cannot invalidate committed admission/settlement; Drop and
    /// expiry collection independently retry the idempotent operation.
    pub(crate) async fn release(&self) {
        match self.db.release_archive_budget_reservation(self.id).await {
            Ok(()) => self
                .released
                .store(true, std::sync::atomic::Ordering::Relaxed),
            Err(error) => {
                tracing::warn!(phase = "archive_capacity_refund", request_id = %self.identity.request_id,
                error_code = error.diagnostic_category(), "archive capacity refund deferred to recovery")
            }
        }
    }
}

impl Drop for ArchiveBudgetReservation {
    fn drop(&mut self) {
        if self.released.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        // Cancellation must not leak capacity until the full TTL. The durable
        // row remains the recovery authority if this task/process also exits.
        let db = self.db.clone();
        let id = self.id;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = db.release_archive_budget_reservation(id).await {
                    tracing::warn!(
                        phase = "archive_capacity_refund",
                        error_code = error.diagnostic_category(),
                        "archive capacity cancellation refund deferred to recovery"
                    );
                }
            });
        }
    }
}

impl Database {
    pub(crate) async fn reserve_buffered_archive_capacity(
        &self,
        archive: &crate::response_archive_spool::BufferedArchive<'_>,
    ) -> Result<Option<ArchiveBudgetReservation>, AppError> {
        if archive.body().len() > PLAIN_LIMIT as usize {
            return Ok(None);
        }
        let amount = archive
            .body()
            .chunks(crate::response_archive_spool::CHUNK_BYTES)
            .try_fold(SPOOL_OVERHEAD, |total, bytes| {
                total.checked_add(archive.sealed_len(bytes.len())? as i64 + CHUNK_OVERHEAD)
            })
            .ok_or(AppError::Internal)?;
        if amount > CIPHER_LIMIT {
            return Ok(None);
        }
        // Also recovers reservations during gateway-only rolling upgrades,
        // when the currently running worker predates this table.
        self.cleanup_expired_archive_budget_reservation().await?;
        let id = Uuid::now_v7();
        let started = Instant::now();
        let mut tx = self.begin_write_transaction().await?;
        let now = archive_clock(&mut tx, self.backend).await?;
        // The conditional budget update is the established short global
        // serialization point. Once it succeeds, the slot inventory is stable
        // until this transaction commits or rolls back the provisional charge.
        let (statement, purpose_limit) = match archive.purpose() {
            BufferedArchivePurpose::Request => (
                "UPDATE response_archive_spool_budget
                 SET cipher_bytes = cipher_bytes + $1,
                     request_cipher_bytes = request_cipher_bytes + $1
                 WHERE singleton = 1
                   AND cipher_bytes <= $2
                   AND request_cipher_bytes <= $3",
                REQUEST_CIPHER_LIMIT,
            ),
            BufferedArchivePurpose::Response => (
                "UPDATE response_archive_spool_budget
                 SET cipher_bytes = cipher_bytes + $1
                 WHERE singleton = 1
                   AND cipher_bytes <= $2
                   AND cipher_bytes - request_cipher_bytes <= $3",
                RESPONSE_CIPHER_LIMIT,
            ),
        };
        let updated = sqlx::query(statement)
            .bind(amount)
            .bind(CIPHER_LIMIT - amount)
            .bind(purpose_limit - amount)
            .execute(&mut *tx)
            .await?;
        if updated.rows_affected() != 1 {
            tx.rollback().await?;
            tracing::warn!(phase = "archive_capacity_reserve", requested_bytes = amount,
                request_id = %archive.identity().request_id, "archive capacity reservation unavailable");
            return Ok(None);
        }
        let active_slots: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(spool_sql(
            archive.purpose(),
            "SELECT
                (SELECT COUNT(*) FROM response_archive_spools
                 WHERE cleaned_at IS NULL AND state IN ('capturing', 'pending', 'uploading'))
              + (SELECT COUNT(*) FROM archive_budget_reservations
                 WHERE purpose = $1)",
        )))
        .bind(archive.purpose().as_str())
        .fetch_one(&mut *tx)
        .await?;
        if active_slots >= ARCHIVE_SLOT_LIMIT {
            tx.rollback().await?;
            tracing::warn!(phase = "archive_capacity_reserve", requested_bytes = amount,
                request_id = %archive.identity().request_id, purpose = archive.purpose().as_str(),
                reason = "slot_capacity", "archive capacity reservation unavailable");
            return Ok(None);
        }
        sqlx::query("INSERT INTO archive_budget_reservations (id, request_id, purpose, cipher_bytes, expires_at) VALUES ($1, $2, $3, $4, $5)")
            .bind(id.to_string()).bind(archive.identity().request_id.to_string())
            .bind(archive.purpose().as_str())
            .bind(amount).bind(now + RESERVATION_TTL).execute(&mut *tx).await?;
        // Arm cancellation recovery only once a reservation can commit. A
        // capacity rejection must not launch extra refund work into a busy pool.
        let reservation = ArchiveBudgetReservation {
            db: self.clone(),
            id,
            identity: archive.identity(),
            purpose: archive.purpose(),
            released: std::sync::atomic::AtomicBool::new(false),
        };
        tx.commit().await?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if elapsed_ms >= 250 {
            tracing::warn!(phase = "archive_capacity_reserve", requested_bytes = amount, elapsed_ms,
                request_id = %archive.identity().request_id, "slow archive capacity reservation");
        }
        Ok(Some(reservation))
    }

    pub(crate) async fn reserved_spool_transaction(
        &self,
        reservation: &ArchiveBudgetReservation,
        operation: &'static str,
    ) -> Result<(Transaction<'_, Any>, i64, BudgetHold), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let statement = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT expires_at FROM archive_budget_reservations WHERE id = $1 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT expires_at FROM archive_budget_reservations WHERE id = $1"
            }
        };
        let row = sqlx::query(statement)
            .bind(reservation.id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        let now = archive_clock(&mut tx, self.backend).await?;
        let expires_at = row
            .map(|row| row.try_get::<i64, _>("expires_at"))
            .transpose()?;
        if expires_at.is_none_or(|expires_at| expires_at <= now) {
            tx.rollback().await?;
            return Err(AppError::Overloaded);
        }
        let mut hold = BudgetHold::late(operation, Some(reservation.identity.request_id));
        hold.phase("reserved_capacity_work");
        Ok((tx, now, hold))
    }

    async fn release_archive_budget_reservation(&self, id: Uuid) -> Result<(), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let statement = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT cipher_bytes, purpose FROM archive_budget_reservations WHERE id = $1 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT cipher_bytes, purpose FROM archive_budget_reservations WHERE id = $1"
            }
        };
        let row = sqlx::query(statement)
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(row) = row {
            refund_reserved_capacity(&mut tx, id, row).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(super) async fn cleanup_expired_archive_budget_reservation(
        &self,
    ) -> Result<bool, AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let now = archive_clock(&mut tx, self.backend).await?;
        let statement = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT id, cipher_bytes, purpose FROM archive_budget_reservations WHERE expires_at <= $1 ORDER BY expires_at, id LIMIT 1 FOR UPDATE SKIP LOCKED"
            }
            DatabaseBackend::Sqlite => {
                "SELECT id, cipher_bytes, purpose FROM archive_budget_reservations WHERE expires_at <= $1 ORDER BY expires_at, id LIMIT 1"
            }
        };
        let row = sqlx::query(statement)
            .bind(now)
            .fetch_optional(&mut *tx)
            .await?;
        let found = row.is_some();
        if let Some(row) = row {
            let id = Uuid::parse_str(&row.try_get::<String, _>("id")?)
                .map_err(|_| AppError::Internal)?;
            let reclaimed_bytes: i64 = row.try_get("cipher_bytes")?;
            refund_reserved_capacity(&mut tx, id, row).await?;
            tracing::info!(
                phase = "archive_capacity_recovery",
                reclaimed_bytes,
                "expired archive capacity reservation reclaimed"
            );
        }
        tx.commit().await?;
        Ok(found)
    }
}

async fn refund_reserved_capacity(
    tx: &mut Transaction<'_, Any>,
    id: Uuid,
    row: AnyRow,
) -> Result<(), AppError> {
    let amount: i64 = row.try_get("cipher_bytes")?;
    let purpose = match row.try_get::<String, _>("purpose")?.as_str() {
        "request" => BufferedArchivePurpose::Request,
        "response" => BufferedArchivePurpose::Response,
        _ => return Err(AppError::Internal),
    };
    if amount > 0 {
        let updated = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose,
            "UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes - $1 WHERE singleton = 1 AND cipher_bytes >= $1")))
            .bind(amount).execute(&mut **tx).await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::Internal);
        }
    }
    sqlx::query("DELETE FROM archive_budget_reservations WHERE id = $1")
        .bind(id.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}
