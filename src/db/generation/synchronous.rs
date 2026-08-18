use super::super::*;
use crate::archive_staging::{
    ArchiveStagingOwner, ArchiveStagingPurpose, ArchiveStagingWriteLease,
};

#[derive(Clone, Debug)]
pub struct GenerationJobIdempotency {
    pub key: String,
    pub request_hash: String,
}

#[derive(Clone, Debug)]
pub enum SynchronousImageIdempotencyClaim {
    Claimed,
    Pending {
        request_id: Uuid,
    },
    Completed {
        request_id: Uuid,
        response_status: i64,
        response_object: String,
    },
    Failed {
        request_id: Uuid,
        error_code: String,
    },
}

pub struct StartSynchronousImageRequest<'a> {
    pub request_id: Uuid,
    pub key: &'a AuthenticatedKey,
    pub price: &'a ModelPrice,
    pub input_token_ceiling: i64,
    pub output_token_ceiling: i64,
    pub idempotency: Option<&'a GenerationJobIdempotency>,
    pub protocol: &'a str,
    pub model: &'a str,
    pub request_object: &'a str,
    pub upstream_account_id: Option<Uuid>,
    pub model_route_id: Option<Uuid>,
}

#[derive(Clone, Debug)]
pub enum StartSynchronousImageResult {
    Started(UsageReservation),
    Replay(SynchronousImageIdempotencyClaim),
}

pub struct AttachSynchronousImageRequestObject<'a> {
    pub key_id: Uuid,
    pub idempotency_key: Option<&'a str>,
    pub request_id: Uuid,
    pub reservation_id: Uuid,
    pub expected_staging_object: &'a str,
    pub request_object: &'a str,
}

pub struct FinishSynchronousImageRequest<'a> {
    pub key_id: Uuid,
    pub idempotency_key: Option<&'a str>,
    pub request_id: Uuid,
    pub reservation: &'a UsageReservation,
    pub status_code: i64,
    pub duration_ms: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub error_code: Option<&'a str>,
    pub response_object: &'a str,
    pub assets: &'a [ArchivedGenerationAsset],
}

#[derive(Clone, Debug)]
pub enum FinishSynchronousImageResult {
    Finished { cost_micros: i64 },
    Replay(SynchronousImageIdempotencyClaim),
}

impl Database {
    pub async fn claim_synchronous_image_idempotency(
        &self,
        key_id: Uuid,
        idempotency: &GenerationJobIdempotency,
        request_id: Uuid,
    ) -> Result<SynchronousImageIdempotencyClaim, AppError> {
        validate_generation_job_idempotency(idempotency)?;
        let now = unix_millis();
        let lease_expires_at = now.saturating_add(SYNCHRONOUS_IMAGE_IDEMPOTENCY_LEASE_MILLIS);
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO synchronous_image_idempotency (key_id, idempotency_key, request_hash, request_id, status, created_at, lease_expires_at) VALUES ($1, $2, $3, $4, 'pending', $5, $6) ON CONFLICT(key_id, idempotency_key) DO NOTHING",
        )
        .bind(key_id.to_string())
        .bind(&idempotency.key)
        .bind(&idempotency.request_hash)
        .bind(request_id.to_string())
        .bind(now)
        .bind(lease_expires_at)
        .execute(&mut *transaction)
        .await?;
        if inserted.rows_affected() == 1 {
            transaction.commit().await?;
            return Ok(SynchronousImageIdempotencyClaim::Claimed);
        }
        let row = sqlx::query(
            "SELECT request_hash, request_id, status, response_status, response_object, error_code, lease_expires_at FROM synchronous_image_idempotency WHERE key_id = $1 AND idempotency_key = $2",
        )
        .bind(key_id.to_string())
        .bind(&idempotency.key)
        .fetch_one(&mut *transaction)
        .await?;
        let existing_hash: String = row.try_get("request_hash")?;
        if existing_hash != idempotency.request_hash {
            transaction.commit().await?;
            return Err(AppError::BadRequest(
                "Idempotency-Key was already used for a different image request".into(),
            ));
        }
        if row.try_get::<String, _>("status")? == "pending"
            && row.try_get::<i64, _>("lease_expires_at")? <= now
        {
            // Early claim is intentionally read-only for an expired owner.
            // Route/archive preparation may now proceed, but only the atomic
            // start transaction may refund/recover the old request and CAS the
            // new owner into place.
            transaction.commit().await?;
            return Ok(SynchronousImageIdempotencyClaim::Claimed);
        }
        transaction.commit().await?;
        let request_id = parse_uuid(row.try_get("request_id")?)?;
        match row.try_get::<String, _>("status")?.as_str() {
            "pending" => Ok(SynchronousImageIdempotencyClaim::Pending { request_id }),
            "completed" => Ok(SynchronousImageIdempotencyClaim::Completed {
                request_id,
                response_status: row
                    .try_get::<Option<i64>, _>("response_status")?
                    .ok_or(AppError::Internal)?,
                response_object: row
                    .try_get::<Option<String>, _>("response_object")?
                    .ok_or(AppError::Internal)?,
            }),
            "failed" => Ok(SynchronousImageIdempotencyClaim::Failed {
                request_id,
                error_code: row
                    .try_get::<Option<String>, _>("error_code")?
                    .unwrap_or_else(|| "image_generation_failed".to_owned()),
            }),
            _ => Err(AppError::Internal),
        }
    }

    pub async fn start_synchronous_image_request(
        &self,
        input: StartSynchronousImageRequest<'_>,
    ) -> Result<StartSynchronousImageResult, AppError> {
        let now = unix_millis();
        let Some(idempotency) = input.idempotency else {
            let mut transaction = self.pool.begin().await?;
            let reservation = reserve_usage_in_transaction(
                &mut transaction,
                input.key,
                input.price,
                input.input_token_ceiling,
                input.output_token_ceiling,
                now,
            )
            .await?;
            record_request_started_in_transaction(
                &mut transaction,
                &NewRequest {
                    request_id: input.request_id,
                    key_id: input.key.key_id,
                    tenant_id: input.key.tenant_id,
                    protocol: input.protocol.to_owned(),
                    model: input.model.to_owned(),
                    request_object: input.request_object.to_owned(),
                    reservation_id: reservation.id,
                    upstream_account_id: input.upstream_account_id,
                    model_route_id: input.model_route_id,
                },
                now,
            )
            .await?;
            transaction.commit().await?;
            return Ok(StartSynchronousImageResult::Started(reservation));
        };
        validate_generation_job_idempotency(idempotency)?;
        let lease_expires_at = now.saturating_add(SYNCHRONOUS_IMAGE_IDEMPOTENCY_LEASE_MILLIS);
        let key_id = input.key.key_id.to_string();
        let request_id = input.request_id.to_string();
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO synchronous_image_idempotency (key_id, idempotency_key, request_hash, request_id, status, created_at, lease_expires_at) VALUES ($1, $2, $3, $4, 'pending', $5, $6) ON CONFLICT(key_id, idempotency_key) DO NOTHING",
        )
        .bind(&key_id)
        .bind(&idempotency.key)
        .bind(&idempotency.request_hash)
        .bind(&request_id)
        .bind(now)
        .bind(lease_expires_at)
        .execute(&mut *transaction)
        .await?;

        if inserted.rows_affected() == 0 {
            // A no-op write is portable across PostgreSQL and SQLite and keeps
            // the owner row locked until recovery/takeover commits.
            sqlx::query(
                "UPDATE synchronous_image_idempotency SET lease_expires_at = lease_expires_at WHERE key_id = $1 AND idempotency_key = $2",
            )
            .bind(&key_id)
            .bind(&idempotency.key)
            .execute(&mut *transaction)
            .await?;
            let row = sqlx::query(
                "SELECT request_hash, request_id, reservation_id, status, response_status, response_object, error_code, lease_expires_at FROM synchronous_image_idempotency WHERE key_id = $1 AND idempotency_key = $2",
            )
            .bind(&key_id)
            .bind(&idempotency.key)
            .fetch_one(&mut *transaction)
            .await?;
            let existing_hash: String = row.try_get("request_hash")?;
            if existing_hash != idempotency.request_hash {
                return Err(AppError::BadRequest(
                    "Idempotency-Key was already used for a different image request".into(),
                ));
            }
            let status: String = row.try_get("status")?;
            let existing_request_id = parse_uuid(row.try_get("request_id")?)?;
            if status != "pending" {
                let replay = synchronous_image_claim_from_row(row)?;
                transaction.commit().await?;
                return Ok(StartSynchronousImageResult::Replay(replay));
            }
            let existing_reservation_id: Option<String> = row.try_get("reservation_id")?;
            if existing_request_id == input.request_id && existing_reservation_id.is_some() {
                transaction.commit().await?;
                return Ok(StartSynchronousImageResult::Replay(
                    SynchronousImageIdempotencyClaim::Pending {
                        request_id: existing_request_id,
                    },
                ));
            }
            if existing_request_id == input.request_id {
                let renewed = sqlx::query(
                    "UPDATE synchronous_image_idempotency SET lease_expires_at = $1 WHERE key_id = $2 AND idempotency_key = $3 AND request_id = $4 AND reservation_id IS NULL AND status = 'pending'",
                )
                .bind(lease_expires_at)
                .bind(&key_id)
                .bind(&idempotency.key)
                .bind(&request_id)
                .execute(&mut *transaction)
                .await?;
                if renewed.rows_affected() != 1 {
                    return Err(AppError::Conflict(
                        "synchronous image idempotency owner changed before request start".into(),
                    ));
                }
            } else {
                if row.try_get::<i64, _>("lease_expires_at")? > now {
                    transaction.commit().await?;
                    return Ok(StartSynchronousImageResult::Replay(
                        SynchronousImageIdempotencyClaim::Pending {
                            request_id: existing_request_id,
                        },
                    ));
                }
                if let Some(replay) = recover_expired_synchronous_image_owner(
                    &mut transaction,
                    input.key.key_id,
                    &idempotency.key,
                    existing_request_id,
                    now,
                )
                .await?
                {
                    if matches!(replay, SynchronousImageIdempotencyClaim::Failed { .. }) {
                        super::cleanup_archive_staging_purpose_in_transaction(
                            &mut transaction,
                            ArchiveStagingOwner::SynchronousRequest(existing_request_id),
                            ArchiveStagingPurpose::Result,
                        )
                        .await?;
                        super::cleanup_writing_archive_staging_purpose_in_transaction(
                            &mut transaction,
                            ArchiveStagingOwner::SynchronousRequest(existing_request_id),
                            ArchiveStagingPurpose::Request,
                        )
                        .await?;
                    }
                    transaction.commit().await?;
                    return Ok(StartSynchronousImageResult::Replay(replay));
                }
                super::cleanup_archive_staging_purpose_in_transaction(
                    &mut transaction,
                    ArchiveStagingOwner::SynchronousRequest(existing_request_id),
                    ArchiveStagingPurpose::Result,
                )
                .await?;
                super::cleanup_writing_archive_staging_purpose_in_transaction(
                    &mut transaction,
                    ArchiveStagingOwner::SynchronousRequest(existing_request_id),
                    ArchiveStagingPurpose::Request,
                )
                .await?;
                let takeover = sqlx::query(
                    "UPDATE synchronous_image_idempotency SET request_id = $1, reservation_id = NULL, created_at = $2, lease_expires_at = $3, response_status = NULL, response_object = NULL, error_code = NULL, completed_at = NULL WHERE key_id = $4 AND idempotency_key = $5 AND request_hash = $6 AND request_id = $7 AND status = 'pending' AND lease_expires_at <= $2",
                )
                .bind(&request_id)
                .bind(now)
                .bind(lease_expires_at)
                .bind(&key_id)
                .bind(&idempotency.key)
                .bind(&idempotency.request_hash)
                .bind(existing_request_id.to_string())
                .execute(&mut *transaction)
                .await?;
                if takeover.rows_affected() != 1 {
                    return Err(AppError::Conflict(
                        "synchronous image idempotency owner changed during takeover".into(),
                    ));
                }
            }
        }

        let reservation = reserve_usage_in_transaction(
            &mut transaction,
            input.key,
            input.price,
            input.input_token_ceiling,
            input.output_token_ceiling,
            now,
        )
        .await?;
        record_request_started_in_transaction(
            &mut transaction,
            &NewRequest {
                request_id: input.request_id,
                key_id: input.key.key_id,
                tenant_id: input.key.tenant_id,
                protocol: input.protocol.to_owned(),
                model: input.model.to_owned(),
                request_object: input.request_object.to_owned(),
                reservation_id: reservation.id,
                upstream_account_id: input.upstream_account_id,
                model_route_id: input.model_route_id,
            },
            now,
        )
        .await?;
        let linked = sqlx::query(
            "UPDATE synchronous_image_idempotency SET reservation_id = $1 WHERE key_id = $2 AND idempotency_key = $3 AND request_hash = $4 AND request_id = $5 AND status = 'pending'",
        )
        .bind(reservation.id.to_string())
        .bind(&key_id)
        .bind(&idempotency.key)
        .bind(&idempotency.request_hash)
        .bind(&request_id)
        .execute(&mut *transaction)
        .await?;
        if linked.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "synchronous image idempotency owner changed before request start".into(),
            ));
        }
        transaction.commit().await?;
        Ok(StartSynchronousImageResult::Started(reservation))
    }

    /// Replaces the local staging digest with the durable CAS locator after
    /// admission succeeds. Idempotent requests additionally compare the live
    /// claim owner, so a worker that lost its lease can never attach an object
    /// to the replacement request.
    pub async fn attach_synchronous_image_request_object(
        &self,
        input: AttachSynchronousImageRequestObject<'_>,
    ) -> Result<(), AppError> {
        if !input
            .expected_staging_object
            .starts_with("staging://blake3/")
            || !input.request_object.starts_with("objects/blake3/")
        {
            return Err(AppError::BadRequest(
                "synchronous image archive locator is invalid".into(),
            ));
        }
        if let Some(idempotency_key) = input.idempotency_key {
            validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        }
        let key_id = input.key_id.to_string();
        let request_id = input.request_id.to_string();
        let reservation_id = input.reservation_id.to_string();
        let mut transaction = self.pool.begin().await?;
        if let Some(idempotency_key) = input.idempotency_key {
            let owner = sqlx::query(
                "UPDATE synchronous_image_idempotency SET lease_expires_at = lease_expires_at WHERE key_id = $1 AND idempotency_key = $2 AND request_id = $3 AND reservation_id = $4 AND status = 'pending'",
            )
            .bind(&key_id)
            .bind(idempotency_key)
            .bind(&request_id)
            .bind(&reservation_id)
            .execute(&mut *transaction)
            .await?;
            if owner.rows_affected() != 1 {
                return Err(AppError::NotFound);
            }
        }
        let attached = sqlx::query(
            "UPDATE request_records SET request_object = $1 WHERE id = $2 AND key_id = $3 AND reservation_id = $4 AND request_object = $5 AND completed_at IS NULL",
        )
        .bind(input.request_object)
        .bind(&request_id)
        .bind(&key_id)
        .bind(&reservation_id)
        .bind(input.expected_staging_object)
        .execute(&mut *transaction)
        .await?;
        if attached.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Atomically replaces the admission placeholder and binds the unique
    /// request object. A fenced staging writer cannot publish its locator.
    pub async fn attach_synchronous_image_request_object_staged(
        &self,
        input: AttachSynchronousImageRequestObject<'_>,
        staging_lease: &ArchiveStagingWriteLease,
    ) -> Result<(), AppError> {
        if staging_lease.key.owner != ArchiveStagingOwner::SynchronousRequest(input.request_id)
            || staging_lease.key.purpose != ArchiveStagingPurpose::Request
        {
            return Err(AppError::BadRequest(
                "synchronous image request staging owner is invalid".into(),
            ));
        }
        if let Some(idempotency_key) = input.idempotency_key {
            validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        }
        let key_id = input.key_id.to_string();
        let request_id = input.request_id.to_string();
        let reservation_id = input.reservation_id.to_string();
        let mut transaction = self.pool.begin().await?;
        if let Some(idempotency_key) = input.idempotency_key {
            let owner = sqlx::query(
                "UPDATE synchronous_image_idempotency SET lease_expires_at = lease_expires_at WHERE key_id = $1 AND idempotency_key = $2 AND request_id = $3 AND reservation_id = $4 AND status = 'pending'",
            )
            .bind(&key_id)
            .bind(idempotency_key)
            .bind(&request_id)
            .bind(&reservation_id)
            .execute(&mut *transaction)
            .await?;
            if owner.rows_affected() != 1 {
                super::cleanup_archive_staging_attempt_in_transaction(
                    &mut transaction,
                    staging_lease.key,
                )
                .await?;
                transaction.commit().await?;
                return Err(AppError::NotFound);
            }
        }
        let attached = sqlx::query(
            "UPDATE request_records SET request_object = $1 WHERE id = $2 AND key_id = $3 AND reservation_id = $4 AND request_object = $5 AND completed_at IS NULL",
        )
        .bind(input.request_object)
        .bind(&request_id)
        .bind(&key_id)
        .bind(&reservation_id)
        .bind(input.expected_staging_object)
        .execute(&mut *transaction)
        .await?;
        if attached.rows_affected() != 1 {
            let existing: Option<String> = sqlx::query_scalar(
                "SELECT request_object FROM request_records WHERE id = $1 AND key_id = $2 AND reservation_id = $3",
            )
            .bind(&request_id)
            .bind(&key_id)
            .bind(&reservation_id)
            .fetch_optional(&mut *transaction)
            .await?;
            if existing.as_deref() != Some(input.request_object) {
                super::cleanup_archive_staging_attempt_in_transaction(
                    &mut transaction,
                    staging_lease.key,
                )
                .await?;
                transaction.commit().await?;
                return Err(AppError::NotFound);
            }
        }
        let bound = super::super::archive_staging::bind_archive_staging_attempt_in_transaction(
            &mut transaction,
            self.backend,
            staging_lease,
            input.request_object,
        )
        .await?;
        if !bound {
            transaction.rollback().await?;
            return Err(AppError::Conflict(
                "synchronous image request staging writer was fenced".into(),
            ));
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn finish_synchronous_image_request(
        &self,
        input: FinishSynchronousImageRequest<'_>,
    ) -> Result<FinishSynchronousImageResult, AppError> {
        self.finish_synchronous_image_request_inner(input, None)
            .await
    }

    pub async fn finish_synchronous_image_request_staged(
        &self,
        input: FinishSynchronousImageRequest<'_>,
        result_lease: Option<&ArchiveStagingWriteLease>,
    ) -> Result<FinishSynchronousImageResult, AppError> {
        if let Some(lease) = result_lease
            && (lease.key.owner != ArchiveStagingOwner::SynchronousRequest(input.request_id)
                || lease.key.purpose != ArchiveStagingPurpose::Result)
        {
            return Err(AppError::BadRequest(
                "synchronous image result staging owner is invalid".into(),
            ));
        }
        self.finish_synchronous_image_request_inner(input, result_lease)
            .await
    }

    async fn finish_synchronous_image_request_inner(
        &self,
        input: FinishSynchronousImageRequest<'_>,
        result_lease: Option<&ArchiveStagingWriteLease>,
    ) -> Result<FinishSynchronousImageResult, AppError> {
        if let Some(idempotency_key) = input.idempotency_key {
            validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        }
        let now = unix_millis();
        let key_id = input.key_id.to_string();
        let request_id = input.request_id.to_string();
        let reservation_id = input.reservation.id.to_string();
        let mut transaction = self.pool.begin().await?;
        if let Some(idempotency_key) = input.idempotency_key {
            let owner = sqlx::query(
                "UPDATE synchronous_image_idempotency SET lease_expires_at = lease_expires_at WHERE key_id = $1 AND idempotency_key = $2 AND request_id = $3 AND reservation_id = $4 AND status = 'pending'",
            )
            .bind(&key_id)
            .bind(idempotency_key)
            .bind(&request_id)
            .bind(&reservation_id)
            .execute(&mut *transaction)
            .await?;
            if owner.rows_affected() != 1 {
                let row = sqlx::query(
                    "SELECT request_hash, request_id, status, response_status, response_object, error_code, lease_expires_at FROM synchronous_image_idempotency WHERE key_id = $1 AND idempotency_key = $2",
                )
                .bind(&key_id)
                .bind(idempotency_key)
                .fetch_optional(&mut *transaction)
                .await?
                .ok_or(AppError::NotFound)?;
                let replay = synchronous_image_claim_from_row(row)?;
                settle_synchronous_image_result_attempt(
                    &mut transaction,
                    self.backend,
                    &input,
                    result_lease,
                    &replay,
                )
                .await?;
                transaction.commit().await?;
                return Ok(FinishSynchronousImageResult::Replay(replay));
            }
        } else {
            let owner = sqlx::query(
                "UPDATE request_records SET completed_at = completed_at WHERE id = $1 AND key_id = $2 AND reservation_id = $3 AND completed_at IS NULL",
            )
            .bind(&request_id)
            .bind(&key_id)
            .bind(&reservation_id)
            .execute(&mut *transaction)
            .await?;
            if owner.rows_affected() != 1 {
                let replay =
                    recover_non_idempotent_synchronous_image_terminal(&mut transaction, &input)
                        .await?;
                settle_synchronous_image_result_attempt(
                    &mut transaction,
                    self.backend,
                    &input,
                    result_lease,
                    &replay,
                )
                .await?;
                transaction.commit().await?;
                return Ok(FinishSynchronousImageResult::Replay(replay));
            }
        }
        let request_owner =
            sqlx::query("SELECT reservation_id FROM request_records WHERE id = $1 AND key_id = $2")
                .bind(&request_id)
                .bind(&key_id)
                .fetch_optional(&mut *transaction)
                .await?
                .ok_or(AppError::NotFound)?;
        if request_owner.try_get::<String, _>("reservation_id")? != reservation_id {
            return Err(AppError::Conflict(
                "synchronous image request reservation does not match its owner".into(),
            ));
        }

        insert_synchronous_generation_assets_in_transaction(
            &mut transaction,
            input.request_id,
            input.assets,
            now,
        )
        .await?;
        let usage = TokenUsage {
            input_tokens: input.input_tokens,
            output_tokens: input.output_tokens,
            ..TokenUsage::default()
        };
        let cost_micros =
            settle_token_usage_in_transaction(&mut transaction, input.reservation, &usage, now)
                .await?;
        let finished = record_request_finished_in_transaction(
            &mut transaction,
            &FinishRequest {
                request_id: input.request_id,
                status_code: input.status_code,
                duration_ms: input.duration_ms,
                input_tokens: input.input_tokens,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: input.output_tokens,
                service_tier: None,
                cost_micros,
                error_code: input.error_code.map(str::to_owned),
                response_object: input.response_object.to_owned(),
            },
            now,
        )
        .await?;
        if !finished {
            let existing = sqlx::query(
                "SELECT status_code, cost_micros, error_code, response_object FROM request_records WHERE id = $1 AND reservation_id = $2 AND completed_at IS NOT NULL",
            )
            .bind(&request_id)
            .bind(&reservation_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::Conflict(
                "synchronous image terminal request is missing".into(),
            ))?;
            if existing.try_get::<i64, _>("status_code")? != input.status_code
                || existing.try_get::<i64, _>("cost_micros")? != cost_micros
                || existing.try_get::<Option<String>, _>("error_code")?
                    != input.error_code.map(str::to_owned)
                || existing
                    .try_get::<Option<String>, _>("response_object")?
                    .as_deref()
                    != Some(input.response_object)
            {
                return Err(AppError::Conflict(
                    "synchronous image terminal replay does not match the request".into(),
                ));
            }
        }
        if let Some(idempotency_key) = input.idempotency_key {
            let terminal = if let Some(error_code) = input.error_code {
                sqlx::query(
                    "UPDATE synchronous_image_idempotency SET status = 'failed', response_status = $1, response_object = NULL, error_code = $2, completed_at = $3 WHERE key_id = $4 AND idempotency_key = $5 AND request_id = $6 AND reservation_id = $7 AND status = 'pending'",
                )
                .bind(input.status_code)
                .bind(error_code)
                .bind(now)
                .bind(&key_id)
                .bind(idempotency_key)
                .bind(&request_id)
                .bind(&reservation_id)
                .execute(&mut *transaction)
                .await?
            } else {
                sqlx::query(
                    "UPDATE synchronous_image_idempotency SET status = 'completed', response_status = $1, response_object = $2, error_code = NULL, completed_at = $3 WHERE key_id = $4 AND idempotency_key = $5 AND request_id = $6 AND reservation_id = $7 AND status = 'pending'",
                )
                .bind(input.status_code)
                .bind(input.response_object)
                .bind(now)
                .bind(&key_id)
                .bind(idempotency_key)
                .bind(&request_id)
                .bind(&reservation_id)
                .execute(&mut *transaction)
                .await?
            };
            if terminal.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "synchronous image idempotency owner changed before terminal commit".into(),
                ));
            }
        }
        if input.error_code.is_some() {
            super::cleanup_archive_staging_purpose_in_transaction(
                &mut transaction,
                ArchiveStagingOwner::SynchronousRequest(input.request_id),
                ArchiveStagingPurpose::Result,
            )
            .await?;
        } else if let Some(lease) = result_lease {
            let prefix = lease.key.canonical_prefix();
            let bound = super::super::archive_staging::bind_archive_staging_attempt_in_transaction(
                &mut transaction,
                self.backend,
                lease,
                &prefix,
            )
            .await?;
            if !bound {
                transaction.rollback().await?;
                return Err(AppError::Conflict(
                    "synchronous image result staging writer was fenced".into(),
                ));
            }
        }
        transaction.commit().await?;
        Ok(FinishSynchronousImageResult::Finished { cost_micros })
    }

    pub async fn release_synchronous_image_idempotency_claim(
        &self,
        key_id: Uuid,
        idempotency_key: &str,
        request_id: Uuid,
    ) -> Result<(), AppError> {
        sqlx::query(
            "DELETE FROM synchronous_image_idempotency WHERE key_id = $1 AND idempotency_key = $2 AND request_id = $3 AND status = 'pending'",
        )
        .bind(key_id.to_string())
        .bind(idempotency_key)
        .bind(request_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn renew_synchronous_image_idempotency_claim(
        &self,
        key_id: Uuid,
        idempotency_key: &str,
        request_id: Uuid,
    ) -> Result<(), AppError> {
        let updated = sqlx::query(
            "UPDATE synchronous_image_idempotency SET lease_expires_at = $1 WHERE key_id = $2 AND idempotency_key = $3 AND request_id = $4 AND status = 'pending'",
        )
        .bind(unix_millis().saturating_add(SYNCHRONOUS_IMAGE_IDEMPOTENCY_LEASE_MILLIS))
        .bind(key_id.to_string())
        .bind(idempotency_key)
        .bind(request_id.to_string())
        .execute(&self.pool)
        .await?;
        if updated.rows_affected() == 1 {
            Ok(())
        } else {
            Err(AppError::NotFound)
        }
    }

    pub async fn record_synchronous_generation_assets(
        &self,
        request_id: Uuid,
        assets: &[ArchivedGenerationAsset],
    ) -> Result<(), AppError> {
        if assets.is_empty() {
            return Ok(());
        }
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        let request_exists =
            sqlx::query("SELECT 1 FROM request_record_locators WHERE id = $1 LIMIT 1")
                .bind(request_id.to_string())
                .fetch_optional(&mut *transaction)
                .await?
                .is_some();
        if !request_exists {
            return Err(AppError::NotFound);
        }
        for asset in assets {
            sqlx::query(
                "INSERT INTO generation_assets (id, request_id, asset_index, object_locator, mime_type, size_bytes, filename, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(asset.asset_id.to_string())
            .bind(request_id.to_string())
            .bind(asset.index)
            .bind(&asset.object_locator)
            .bind(&asset.mime_type)
            .bind(asset.size_bytes)
            .bind(&asset.filename)
            .bind(now)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn synchronous_generation_assets(
        &self,
        request_id: Uuid,
    ) -> Result<Vec<GenerationAssetDownload>, AppError> {
        let rows = sqlx::query(
            "SELECT id, asset_index, object_locator, mime_type, size_bytes, filename FROM generation_assets WHERE request_id = $1 ORDER BY asset_index, id",
        )
        .bind(request_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(generation_asset_download).collect()
    }

    pub async fn synchronous_generation_asset_for_key(
        &self,
        key_id: Uuid,
        request_id: Uuid,
        asset_id: Uuid,
    ) -> Result<GenerationAssetDownload, AppError> {
        let row = sqlx::query(
            "SELECT a.id, a.asset_index, a.object_locator, a.mime_type, a.size_bytes, a.filename FROM generation_assets a JOIN request_record_locators r ON r.id = a.request_id WHERE a.id = $1 AND a.request_id = $2 AND r.key_id = $3",
        )
        .bind(asset_id.to_string())
        .bind(request_id.to_string())
        .bind(key_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        generation_asset_download(row)
    }

    pub async fn synchronous_generation_asset_for_tenant(
        &self,
        tenant_external_id: &str,
        request_id: Uuid,
        asset_id: Uuid,
    ) -> Result<GenerationAssetDownload, AppError> {
        let row = sqlx::query(
            "SELECT a.id, a.asset_index, a.object_locator, a.mime_type, a.size_bytes, a.filename FROM generation_assets a JOIN request_record_locators r ON r.id = a.request_id JOIN tenants t ON t.id = r.tenant_id WHERE a.id = $1 AND a.request_id = $2 AND t.external_id = $3",
        )
        .bind(asset_id.to_string())
        .bind(request_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        generation_asset_download(row)
    }

    pub async fn synchronous_generation_asset_global(
        &self,
        request_id: Uuid,
        asset_id: Uuid,
    ) -> Result<GenerationAssetDownload, AppError> {
        let row = sqlx::query(
            "SELECT a.id, a.asset_index, a.object_locator, a.mime_type, a.size_bytes, a.filename FROM generation_assets a JOIN request_record_locators r ON r.id = a.request_id WHERE a.id = $1 AND a.request_id = $2",
        )
        .bind(asset_id.to_string())
        .bind(request_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        generation_asset_download(row)
    }
}

fn synchronous_image_claim_from_row(
    row: AnyRow,
) -> Result<SynchronousImageIdempotencyClaim, AppError> {
    let request_id = parse_uuid(row.try_get("request_id")?)?;
    match row.try_get::<String, _>("status")?.as_str() {
        "pending" => Ok(SynchronousImageIdempotencyClaim::Pending { request_id }),
        "completed" => Ok(SynchronousImageIdempotencyClaim::Completed {
            request_id,
            response_status: row
                .try_get::<Option<i64>, _>("response_status")?
                .ok_or(AppError::Internal)?,
            response_object: row
                .try_get::<Option<String>, _>("response_object")?
                .ok_or(AppError::Internal)?,
        }),
        "failed" => Ok(SynchronousImageIdempotencyClaim::Failed {
            request_id,
            error_code: row
                .try_get::<Option<String>, _>("error_code")?
                .unwrap_or_else(|| "image_generation_failed".to_owned()),
        }),
        _ => Err(AppError::Internal),
    }
}

async fn settle_synchronous_image_result_attempt(
    transaction: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    input: &FinishSynchronousImageRequest<'_>,
    result_lease: Option<&ArchiveStagingWriteLease>,
    replay: &SynchronousImageIdempotencyClaim,
) -> Result<(), AppError> {
    let Some(lease) = result_lease else {
        return Ok(());
    };
    let exact_committed_result = matches!(
        replay,
        SynchronousImageIdempotencyClaim::Completed {
            request_id,
            response_object,
            ..
        } if *request_id == input.request_id && response_object == input.response_object
    );
    if exact_committed_result {
        let prefix = lease.key.canonical_prefix();
        if !super::super::archive_staging::bind_archive_staging_attempt_in_transaction(
            transaction,
            backend,
            lease,
            &prefix,
        )
        .await?
        {
            return Err(AppError::Conflict(
                "synchronous image result staging writer was fenced".into(),
            ));
        }
    } else {
        super::cleanup_archive_staging_attempt_in_transaction(transaction, lease.key).await?;
    }
    Ok(())
}

async fn recover_expired_synchronous_image_owner(
    tx: &mut Transaction<'_, Any>,
    key_id: Uuid,
    idempotency_key: &str,
    request_id: Uuid,
    now: i64,
) -> Result<Option<SynchronousImageIdempotencyClaim>, AppError> {
    let row = sqlx::query(
        "SELECT q.created_at, q.completed_at, q.status_code, q.cost_micros, q.error_code, q.response_object, q.reservation_id, r.account_id, r.key_id AS reservation_key_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start, r.status AS reservation_status, r.actual_micros FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.id = $1 AND q.key_id = $2",
    )
    .bind(request_id.to_string())
    .bind(key_id.to_string())
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let created_at: i64 = row.try_get("created_at")?;
    let completed_at: Option<i64> = row.try_get("completed_at")?;
    let status_code: Option<i64> = row.try_get("status_code")?;
    let error_code: Option<String> = row.try_get("error_code")?;
    let response_object: Option<String> = row.try_get("response_object")?;
    let reservation_status: String = row.try_get("reservation_status")?;
    let reservation = UsageReservation {
        id: parse_uuid(row.try_get("reservation_id")?)?,
        account_id: parse_uuid(row.try_get("account_id")?)?,
        key_id: parse_uuid(row.try_get("reservation_key_id")?)?,
        reserved_micros: row.try_get("reserved_micros")?,
        input_micros_per_million: 0,
        output_micros_per_million: 0,
        price_tiers: Vec::new(),
        rate_window_start: row.try_get("rate_window_start")?,
        reserved_tokens: row.try_get("reserved_tokens")?,
    };

    if completed_at.is_some() {
        let retryable_expiry = matches!(
            error_code.as_deref(),
            Some("request_expired" | "idempotency_claim_expired")
        );
        if reservation_status == "reserved" {
            settle_token_usage_in_transaction(tx, &reservation, &TokenUsage::default(), now)
                .await?;
        }
        if retryable_expiry {
            // The generic orphan reaper and the owner-specific takeover path
            // may race. Both terminal records mean that no upstream result was
            // committed, so preserve the old failure audit row but allow the
            // same request hash to take ownership and execute again.
            return Ok(None);
        }
        let is_success = status_code.is_some_and(|status| (200..400).contains(&status))
            && error_code.is_none()
            && response_object.is_some();
        if is_success {
            let response_status = status_code.ok_or(AppError::Internal)?;
            let response_object = response_object.ok_or(AppError::Internal)?;
            let updated = sqlx::query(
                "UPDATE synchronous_image_idempotency SET status = 'completed', response_status = $1, response_object = $2, error_code = NULL, completed_at = $3 WHERE key_id = $4 AND idempotency_key = $5 AND request_id = $6 AND status = 'pending'",
            )
            .bind(response_status)
            .bind(&response_object)
            .bind(now)
            .bind(key_id.to_string())
            .bind(idempotency_key)
            .bind(request_id.to_string())
            .execute(&mut **tx)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "expired synchronous image owner changed during recovery".into(),
                ));
            }
            return Ok(Some(SynchronousImageIdempotencyClaim::Completed {
                request_id,
                response_status,
                response_object,
            }));
        }
        let error_code = error_code.unwrap_or_else(|| "idempotency_claim_expired".to_owned());
        let updated = sqlx::query(
            "UPDATE synchronous_image_idempotency SET status = 'failed', response_status = $1, response_object = NULL, error_code = $2, completed_at = $3 WHERE key_id = $4 AND idempotency_key = $5 AND request_id = $6 AND status = 'pending'",
        )
        .bind(status_code.unwrap_or(502))
        .bind(&error_code)
        .bind(now)
        .bind(key_id.to_string())
        .bind(idempotency_key)
        .bind(request_id.to_string())
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "expired synchronous image owner changed during recovery".into(),
            ));
        }
        return Ok(Some(SynchronousImageIdempotencyClaim::Failed {
            request_id,
            error_code,
        }));
    }

    if reservation_status == "settled" {
        let error_code = "idempotency_claim_expired";
        let cost_micros = row.try_get::<Option<i64>, _>("actual_micros")?.unwrap_or(0);
        record_request_finished_in_transaction(
            tx,
            &FinishRequest {
                request_id,
                status_code: 502,
                duration_ms: now.saturating_sub(created_at),
                input_tokens: 0,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                service_tier: None,
                cost_micros,
                error_code: Some(error_code.to_owned()),
                response_object: format!("gap://{request_id}/response"),
            },
            now,
        )
        .await?;
        let updated = sqlx::query(
            "UPDATE synchronous_image_idempotency SET status = 'failed', response_status = 502, response_object = NULL, error_code = $1, completed_at = $2 WHERE key_id = $3 AND idempotency_key = $4 AND request_id = $5 AND status = 'pending'",
        )
        .bind(error_code)
        .bind(now)
        .bind(key_id.to_string())
        .bind(idempotency_key)
        .bind(request_id.to_string())
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "expired synchronous image owner changed during recovery".into(),
            ));
        }
        return Ok(Some(SynchronousImageIdempotencyClaim::Failed {
            request_id,
            error_code: error_code.to_owned(),
        }));
    }
    if reservation_status != "reserved" {
        return Err(AppError::Internal);
    }
    settle_token_usage_in_transaction(tx, &reservation, &TokenUsage::default(), now).await?;
    record_request_finished_in_transaction(
        tx,
        &FinishRequest {
            request_id,
            status_code: 502,
            duration_ms: now.saturating_sub(created_at),
            input_tokens: 0,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            service_tier: None,
            cost_micros: 0,
            error_code: Some("idempotency_claim_expired".to_owned()),
            response_object: format!("gap://{request_id}/response"),
        },
        now,
    )
    .await?;
    Ok(None)
}

async fn insert_synchronous_generation_assets_in_transaction(
    tx: &mut Transaction<'_, Any>,
    request_id: Uuid,
    assets: &[ArchivedGenerationAsset],
    now: i64,
) -> Result<(), AppError> {
    for asset in assets {
        let inserted = sqlx::query(
            "INSERT INTO generation_assets (id, request_id, asset_index, object_locator, mime_type, size_bytes, filename, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT(request_id, asset_index) DO NOTHING",
        )
        .bind(asset.asset_id.to_string())
        .bind(request_id.to_string())
        .bind(asset.index)
        .bind(&asset.object_locator)
        .bind(&asset.mime_type)
        .bind(asset.size_bytes)
        .bind(&asset.filename)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let existing = sqlx::query(
                "SELECT id, object_locator, mime_type, size_bytes, filename FROM generation_assets WHERE request_id = $1 AND asset_index = $2",
            )
            .bind(request_id.to_string())
            .bind(asset.index)
            .fetch_one(&mut **tx)
            .await?;
            if existing.try_get::<String, _>("id")? != asset.asset_id.to_string()
                || existing.try_get::<String, _>("object_locator")? != asset.object_locator
                || existing.try_get::<String, _>("mime_type")? != asset.mime_type
                || existing.try_get::<i64, _>("size_bytes")? != asset.size_bytes
                || existing.try_get::<String, _>("filename")? != asset.filename
            {
                return Err(AppError::Conflict(
                    "synchronous image asset replay does not match archived metadata".into(),
                ));
            }
        }
    }
    Ok(())
}

async fn recover_non_idempotent_synchronous_image_terminal(
    tx: &mut Transaction<'_, Any>,
    input: &FinishSynchronousImageRequest<'_>,
) -> Result<SynchronousImageIdempotencyClaim, AppError> {
    let row = sqlx::query(
        "SELECT q.status_code, q.input_tokens, q.output_tokens, q.cost_micros, q.error_code, q.response_object, r.status AS reservation_status, r.actual_micros FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.id = $1 AND q.key_id = $2 AND q.reservation_id = $3 AND q.completed_at IS NOT NULL",
    )
    .bind(input.request_id.to_string())
    .bind(input.key_id.to_string())
    .bind(input.reservation.id.to_string())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let stored_error: Option<String> = row.try_get("error_code")?;
    let stored_response: Option<String> = row.try_get("response_object")?;
    let exact = row.try_get::<i64, _>("status_code")? == input.status_code
        && row.try_get::<i64, _>("input_tokens")? == input.input_tokens
        && row.try_get::<i64, _>("output_tokens")? == input.output_tokens
        && stored_error.as_deref() == input.error_code
        && stored_response.as_deref() == Some(input.response_object)
        && row.try_get::<String, _>("reservation_status")? == "settled"
        && row.try_get::<i64, _>("cost_micros")?
            == row
                .try_get::<Option<i64>, _>("actual_micros")?
                .ok_or(AppError::Internal)?
        && synchronous_image_assets_match(tx, input.request_id, input.assets).await?;
    if !exact {
        return Err(AppError::Conflict(
            "non-idempotent synchronous image terminal replay does not match".into(),
        ));
    }
    if let Some(error_code) = stored_error {
        Ok(SynchronousImageIdempotencyClaim::Failed {
            request_id: input.request_id,
            error_code,
        })
    } else {
        Ok(SynchronousImageIdempotencyClaim::Completed {
            request_id: input.request_id,
            response_status: input.status_code,
            response_object: stored_response.ok_or(AppError::Internal)?,
        })
    }
}

async fn synchronous_image_assets_match(
    tx: &mut Transaction<'_, Any>,
    request_id: Uuid,
    expected: &[ArchivedGenerationAsset],
) -> Result<bool, AppError> {
    let rows = sqlx::query(
        "SELECT id, asset_index, object_locator, mime_type, size_bytes, filename FROM generation_assets WHERE request_id = $1 ORDER BY asset_index, id",
    )
    .bind(request_id.to_string())
    .fetch_all(&mut **tx)
    .await?;
    if rows.len() != expected.len() {
        return Ok(false);
    }
    let mut expected = expected.iter().collect::<Vec<_>>();
    expected.sort_by_key(|asset| (asset.index, asset.asset_id));
    for (row, expected) in rows.iter().zip(expected) {
        if row.try_get::<String, _>("id")? != expected.asset_id.to_string()
            || row.try_get::<i64, _>("asset_index")? != expected.index
            || row.try_get::<String, _>("object_locator")? != expected.object_locator
            || row.try_get::<String, _>("mime_type")? != expected.mime_type
            || row.try_get::<i64, _>("size_bytes")? != expected.size_bytes
            || row.try_get::<String, _>("filename")? != expected.filename
        {
            return Ok(false);
        }
    }
    Ok(true)
}
