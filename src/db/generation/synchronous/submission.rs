use super::*;

impl Database {
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
        let changed = sqlx::query("UPDATE request_records SET submission_started_at = $1 WHERE id = $2 AND key_id = $3 AND reservation_id = $4 AND completed_at IS NULL AND submission_started_at IS NULL AND submission_uncertain_at IS NULL AND request_object LIKE 'objects/blake3/%' AND EXISTS (SELECT 1 FROM usage_reservations r WHERE r.id = $4 AND r.key_id = $3 AND r.status = 'reserved')")
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
