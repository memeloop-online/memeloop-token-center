use super::*;

impl Database {
    /// After cancelling/dropping the arm future, serialize behind its submitted
    /// SQL before deciding that no durable send authority exists. Missing owner
    /// evidence is unknown, never proof that cleanup/refund is safe.
    pub(crate) async fn confirm_synchronous_image_submission_started(
        &self,
        key_id: Uuid,
        request_id: Uuid,
        reservation_id: Uuid,
    ) -> Result<bool, AppError> {
        let mut tx = self.begin_write_transaction().await?;
        sqlx::query("UPDATE synchronous_image_idempotency SET lease_expires_at = lease_expires_at WHERE request_id = $1 AND key_id = $2")
            .bind(request_id.to_string()).bind(key_id.to_string()).execute(&mut *tx).await?;
        let locked = sqlx::query("UPDATE request_records SET completed_at = completed_at WHERE id = $1 AND key_id = $2 AND reservation_id = $3")
            .bind(request_id.to_string()).bind(key_id.to_string()).bind(reservation_id.to_string()).execute(&mut *tx).await?;
        if locked.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        let row = sqlx::query("SELECT q.submission_started_at, q.completed_at, r.status AS reservation_status FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id AND r.key_id = q.key_id WHERE q.id = $1 AND q.key_id = $2 AND q.reservation_id = $3")
            .bind(request_id.to_string()).bind(key_id.to_string()).bind(reservation_id.to_string()).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
        let marker: Option<i64> = row.try_get("submission_started_at")?;
        if marker.is_none()
            && (row.try_get::<Option<i64>, _>("completed_at")?.is_some()
                || row.try_get::<String, _>("reservation_status")? != "reserved")
        {
            return Err(AppError::Conflict(
                "synchronous image request is no longer an unsubmitted live owner".into(),
            ));
        }
        tx.commit().await?;
        Ok(marker.is_some())
    }

    pub(super) async fn synchronous_image_submission_started(
        &self,
        key_id: Uuid,
        request_id: Uuid,
    ) -> Result<bool, AppError> {
        Ok(sqlx::query("SELECT 1 FROM request_records WHERE id = $1 AND key_id = $2 AND submission_started_at IS NOT NULL AND completed_at IS NULL")
            .bind(request_id.to_string()).bind(key_id.to_string()).fetch_optional(&self.pool).await?.is_some())
    }

    /// Commit before the first network send. An ambiguous commit is never a
    /// reason to retry arming or sending: only an acknowledged CAS owns send.
    pub async fn arm_synchronous_image_submission(
        &self,
        key_id: Uuid,
        idempotency_key: Option<&str>,
        request_id: Uuid,
        reservation_id: Uuid,
    ) -> Result<(), AppError> {
        let now = unix_millis();
        let mut tx = self.begin_write_transaction().await?;
        fence_owner(
            &mut tx,
            key_id,
            idempotency_key,
            request_id,
            reservation_id,
            Some(now),
        )
        .await?;
        // Staged attachment publishes the request pointer and its bound receipt
        // atomically. A staging-looking path alone is not proof of persistence:
        // require the exact bound locator, request owner, and request purpose.
        // Metadata-only image requests are already durable in this row and do
        // not need an archive receipt. Retain the legacy content-addressed and
        // bound-staging attachment contracts for historical callers.
        let changed = sqlx::query("UPDATE request_records SET submission_started_at = $1 WHERE id = $2 AND key_id = $3 AND reservation_id = $4 AND completed_at IS NULL AND submission_started_at IS NULL AND submission_uncertain_at IS NULL AND (request_object LIKE 'metadata-only-json:%' OR request_object LIKE 'objects/blake3/%' OR EXISTS (SELECT 1 FROM archive_staging_attempts a WHERE a.owner_kind = 'synchronous_request' AND a.owner_id = request_records.id AND a.purpose = 'request' AND a.state = 'bound' AND a.bound_locator = request_records.request_object)) AND EXISTS (SELECT 1 FROM usage_reservations r WHERE r.id = $4 AND r.key_id = $3 AND r.status = 'reserved')")
            .bind(now).bind(request_id.to_string()).bind(key_id.to_string()).bind(reservation_id.to_string()).execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "synchronous image send owner was fenced".into(),
            ));
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn quarantine_synchronous_image_submission(
        &self,
        key_id: Uuid,
        idempotency_key: Option<&str>,
        request_id: Uuid,
        reservation_id: Uuid,
    ) -> Result<(), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        fence_owner(
            &mut tx,
            key_id,
            idempotency_key,
            request_id,
            reservation_id,
            None,
        )
        .await?;
        mark_uncertain(&mut tx, key_id, idempotency_key, request_id, unix_millis()).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn fence_owner(
    tx: &mut Transaction<'_, Any>,
    key_id: Uuid,
    idempotency_key: Option<&str>,
    request_id: Uuid,
    reservation_id: Uuid,
    live_at: Option<i64>,
) -> Result<(), AppError> {
    if let Some(idempotency_key) = idempotency_key {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let changed = sqlx::query("UPDATE synchronous_image_idempotency SET lease_expires_at = lease_expires_at WHERE key_id = $1 AND idempotency_key = $2 AND request_id = $3 AND reservation_id = $4 AND status = 'pending' AND lease_expires_at > $5")
            .bind(key_id.to_string()).bind(idempotency_key).bind(request_id.to_string()).bind(reservation_id.to_string()).bind(live_at.unwrap_or(i64::MIN))
            .execute(&mut **tx).await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "synchronous image send owner changed".into(),
            ));
        }
        if live_at.is_some() {
            // The row lock may have waited beyond the supplied observation.
            // Recheck time after owning it, before publishing send authority.
            let expires_at: i64 = sqlx::query_scalar("SELECT lease_expires_at FROM synchronous_image_idempotency WHERE key_id = $1 AND idempotency_key = $2 AND request_id = $3")
                .bind(key_id.to_string()).bind(idempotency_key).bind(request_id.to_string())
                .fetch_one(&mut **tx).await?;
            if expires_at <= unix_millis() {
                return Err(AppError::Conflict(
                    "synchronous image send lease expired".into(),
                ));
            }
        }
    } else if sqlx::query(
        "SELECT 1 FROM synchronous_image_idempotency WHERE key_id = $1 AND request_id = $2",
    )
    .bind(key_id.to_string())
    .bind(request_id.to_string())
    .fetch_optional(&mut **tx)
    .await?
    .is_some()
    {
        return Err(AppError::Conflict(
            "synchronous image idempotency owner is required".into(),
        ));
    }
    let changed = sqlx::query("UPDATE request_records SET completed_at = completed_at WHERE id = $1 AND key_id = $2 AND reservation_id = $3 AND completed_at IS NULL")
        .bind(request_id.to_string()).bind(key_id.to_string()).bind(reservation_id.to_string()).execute(&mut **tx).await?;
    if changed.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "synchronous image request owner changed".into(),
        ));
    }
    Ok(())
}

pub(super) async fn mark_uncertain(
    tx: &mut Transaction<'_, Any>,
    key_id: Uuid,
    idempotency_key: Option<&str>,
    request_id: Uuid,
    now: i64,
) -> Result<(), AppError> {
    let changed = sqlx::query("UPDATE request_records SET submission_uncertain_at = COALESCE(submission_uncertain_at, $1), error_code = 'image_submission_uncertain' WHERE id = $2 AND key_id = $3 AND submission_started_at IS NOT NULL AND completed_at IS NULL")
        .bind(now).bind(request_id.to_string()).bind(key_id.to_string()).execute(&mut **tx).await?;
    if changed.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "synchronous image submission is not pending".into(),
        ));
    }
    if let Some(idempotency_key) = idempotency_key {
        sqlx::query("UPDATE synchronous_image_idempotency SET error_code = 'image_submission_uncertain' WHERE key_id = $1 AND idempotency_key = $2 AND request_id = $3 AND status = 'pending'")
            .bind(key_id.to_string()).bind(idempotency_key).bind(request_id.to_string()).execute(&mut **tx).await?;
    }
    Ok(())
}
