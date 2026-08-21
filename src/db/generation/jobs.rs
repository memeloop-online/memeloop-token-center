use super::super::*;
use super::aggregate_terminal_generation_job;
use crate::archive_staging::{
    ArchiveStagingOwner, ArchiveStagingPurpose, ArchiveStagingWriteLease, locator_matches_prefix,
};

pub struct CreateGenerationJobInput {
    pub job_id: Uuid,
    pub key: AuthenticatedKey,
    pub upstream_account_id: Uuid,
    pub reservation: UsageReservation,
    pub public_model: String,
    pub upstream_model: String,
    pub driver: String,
    pub request_object: String,
    pub estimated_units: i64,
    pub billing_unit: String,
    pub micros_per_unit: i64,
}

pub struct StartGenerationJobInput {
    pub job_id: Uuid,
    pub key: AuthenticatedKey,
    pub model_route_id: Uuid,
    pub upstream_account_id: Uuid,
    pub reservation_price: ModelPrice,
    pub public_model: String,
    pub upstream_model: String,
    pub driver: String,
    pub request_hash: String,
    pub estimated_units: i64,
    pub billing_unit: String,
    pub micros_per_unit: i64,
}

#[derive(Clone, Debug)]
pub enum CreateGenerationJobResult {
    Created(GenerationJobView),
    Replayed(GenerationJobView),
}

#[derive(Clone, Debug)]
pub enum AttachGenerationJobResult {
    Attached(Box<GenerationJobView>),
    /// The archive is durable and admission succeeded, but the database could
    /// not prove whether its preparing->queued commit was acknowledged. The
    /// API must return the known job identity as accepted and must not refund;
    /// exact recovery or the preparation reaper resolves the durable state.
    Indeterminate,
}

pub struct FinishGenerationJobInput<'a> {
    pub job_id: Uuid,
    pub worker_id: &'a str,
    pub status: &'a str,
    pub billed_units: i64,
    pub error_code: Option<&'a str>,
    pub assets: &'a [ArchivedGenerationAsset],
    pub staged_assets: Option<&'a GenerationStagedAssets>,
}

impl Database {
    pub async fn create_generation_job(
        &self,
        input: CreateGenerationJobInput,
    ) -> Result<GenerationJobView, AppError> {
        match self.create_generation_job_idempotent(input, None).await? {
            CreateGenerationJobResult::Created(job) => Ok(job),
            CreateGenerationJobResult::Replayed(_) => Err(AppError::Internal),
        }
    }

    pub async fn generation_job_by_idempotency(
        &self,
        key_id: Uuid,
        idempotency: &GenerationJobIdempotency,
    ) -> Result<Option<GenerationJobView>, AppError> {
        validate_generation_job_idempotency(idempotency)?;
        let row = sqlx::query(
            "SELECT id, created_at, updated_at, completed_at, public_model, driver, billing_unit_snapshot, status, upstream_job_id, estimated_units, billed_units, cost_micros, error_code, result_json, request_hash, lease_expires_at FROM generation_jobs WHERE key_id = $1 AND client_idempotency_key = $2",
        )
        .bind(key_id.to_string())
        .bind(&idempotency.key)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let existing_hash: Option<String> = row.try_get("request_hash")?;
        if existing_hash.as_deref() != Some(idempotency.request_hash.as_str()) {
            return Err(AppError::BadRequest(
                "Idempotency-Key was already used for a different generation request".into(),
            ));
        }
        let status: String = row.try_get("status")?;
        let lease_expires_at: Option<i64> = row.try_get("lease_expires_at")?;
        if status == "preparing"
            && lease_expires_at.is_some_and(|expires_at| expires_at <= unix_millis())
        {
            // The request owner died after admission but before CAS attach.
            // Let start_generation_job acquire the idempotency lock and take
            // over the expired preparation without reserving again.
            return Ok(None);
        }
        Ok(Some(generation_job_view(row)?))
    }

    /// Atomically admits an asynchronous generation request.
    ///
    /// The idempotency namespace is locked before inspecting an existing job,
    /// so a replay or payload mismatch returns before touching quota, rate, or
    /// concurrency state. The reservation, preparing job, and started event
    /// then commit together; a late insert/trigger failure rolls all of them
    /// back. Only the returned owner may upload and attach the request CAS.
    pub async fn start_generation_job(
        &self,
        input: StartGenerationJobInput,
        idempotency: Option<&GenerationJobIdempotency>,
    ) -> Result<CreateGenerationJobResult, AppError> {
        if input.model_route_id.is_nil() {
            return Err(AppError::BadRequest(
                "generation model route snapshot is required".into(),
            ));
        }
        if input.estimated_units <= 0 {
            return Err(AppError::BadRequest(
                "generation estimated units must be positive".into(),
            ));
        }
        if !is_sha256_hex(&input.request_hash) {
            return Err(AppError::BadRequest(
                "generation request hash must be a SHA-256 digest".into(),
            ));
        }
        if let Some(idempotency) = idempotency {
            validate_generation_job_idempotency(idempotency)?;
            if idempotency.request_hash != input.request_hash {
                return Err(AppError::BadRequest(
                    "generation request hash does not match its Idempotency-Key claim".into(),
                ));
            }
        }
        let now = unix_millis();
        let preparation_expires_at = now.saturating_add(GENERATION_PREPARATION_LEASE_MILLIS);
        let mut transaction = self.pool.begin().await?;

        if let Some(idempotency) = idempotency {
            match self.backend {
                DatabaseBackend::PostgreSql => {
                    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, $2))")
                        .bind(format!("{}:{}", input.key.key_id, idempotency.key))
                        .bind(GENERATION_JOB_IDEMPOTENCY_LOCK_SEED)
                        .execute(&mut *transaction)
                        .await?;
                }
                DatabaseBackend::Sqlite => {
                    // SQLite has no row-level advisory locks. A no-op write on
                    // the stable key row obtains its database write lock before
                    // the replay check, matching the entitlement/rotation CAS.
                    let locked =
                        sqlx::query("UPDATE key_records SET updated_at = updated_at WHERE id = $1")
                            .bind(input.key.key_id.to_string())
                            .execute(&mut *transaction)
                            .await?;
                    if locked.rows_affected() != 1 {
                        transaction.rollback().await?;
                        return Err(AppError::NotFound);
                    }
                }
            }

            if let Some(row) = sqlx::query(
                "SELECT id, created_at, updated_at, completed_at, public_model, driver, billing_unit_snapshot, status, upstream_job_id, estimated_units, billed_units, cost_micros, error_code, result_json, request_hash, lease_expires_at FROM generation_jobs WHERE key_id = $1 AND client_idempotency_key = $2",
            )
            .bind(input.key.key_id.to_string())
            .bind(&idempotency.key)
            .fetch_optional(&mut *transaction)
            .await?
            {
                let existing_hash: Option<String> = row.try_get("request_hash")?;
                if existing_hash.as_deref() != Some(idempotency.request_hash.as_str()) {
                    transaction.rollback().await?;
                    return Err(AppError::BadRequest(
                        "Idempotency-Key was already used for a different generation request"
                            .into(),
                    ));
                }
                let status: String = row.try_get("status")?;
                let lease_expires_at: Option<i64> = row.try_get("lease_expires_at")?;
                let replayed = generation_job_view(row)?;
                if status == "preparing"
                    && lease_expires_at.is_some_and(|expires_at| expires_at <= now)
                {
                    let claimed = sqlx::query(
                        "UPDATE generation_jobs SET lease_expires_at = $1, updated_at = $2 WHERE id = $3 AND key_id = $4 AND status = 'preparing' AND lease_expires_at <= $5",
                    )
                    .bind(preparation_expires_at)
                    .bind(now)
                    .bind(replayed.job_id.to_string())
                    .bind(input.key.key_id.to_string())
                    .bind(now)
                    .execute(&mut *transaction)
                    .await?;
                    if claimed.rows_affected() == 1 {
                        super::cleanup_archive_staging_purpose_in_transaction(
                            &mut transaction,
                            ArchiveStagingOwner::GenerationJob(replayed.job_id),
                            ArchiveStagingPurpose::Request,
                        )
                        .await?;
                        transaction.commit().await?;
                        return Ok(CreateGenerationJobResult::Created(replayed));
                    }
                }
                transaction.commit().await?;
                return Ok(CreateGenerationJobResult::Replayed(replayed));
            }
        }

        let reservation = reserve_usage_in_transaction(
            &mut transaction,
            &input.key,
            &input.reservation_price,
            0,
            input.estimated_units,
            now,
        )
        .await?;
        let inserted = sqlx::query(
            "INSERT INTO generation_jobs (id, tenant_id, key_id, model_route_id, upstream_account_id, reservation_id, public_model, upstream_model, driver, status, request_object, estimated_units, billing_unit_snapshot, micros_per_unit_snapshot, client_idempotency_key, request_hash, next_attempt_at, lease_expires_at, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'preparing', '', $10, $11, $12, $13, $14, $15, $16, $17, $18) ON CONFLICT(key_id, client_idempotency_key) DO NOTHING",
        )
        .bind(input.job_id.to_string())
        .bind(input.key.tenant_id.to_string())
        .bind(input.key.key_id.to_string())
        .bind(input.model_route_id.to_string())
        .bind(input.upstream_account_id.to_string())
        .bind(reservation.id.to_string())
        .bind(&input.public_model)
        .bind(&input.upstream_model)
        .bind(&input.driver)
        .bind(input.estimated_units)
        .bind(&input.billing_unit)
        .bind(input.micros_per_unit)
        .bind(idempotency.map(|value| value.key.as_str()))
        .bind(&input.request_hash)
        .bind(now)
        .bind(preparation_expires_at)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if inserted.rows_affected() == 0 {
            // A writer not using the advisory CAS may still win the unique
            // index race. Roll back the reservation before reading its result.
            transaction.rollback().await?;
            return self
                .generation_job_by_idempotency(
                    input.key.key_id,
                    idempotency.ok_or(AppError::Internal)?,
                )
                .await?
                .map(CreateGenerationJobResult::Replayed)
                .ok_or(AppError::Internal);
        }

        let event_id = Uuid::now_v7().to_string();
        let tenant_id = input.key.tenant_id.to_string();
        let key_id = input.key.key_id.to_string();
        let request_id = input.job_id.to_string();
        if claim_request_event_locator(
            &mut transaction,
            &event_id,
            now,
            &tenant_id,
            &key_id,
            &request_id,
        )
        .await?
        {
            sqlx::query(
                "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, 'started', 'generation', $6, 0, 0, 0)",
            )
            .bind(&event_id)
            .bind(&tenant_id)
            .bind(&key_id)
            .bind(&request_id)
            .bind(now)
            .bind(&input.public_model)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(CreateGenerationJobResult::Created(GenerationJobView {
            job_id: input.job_id,
            created_at: now,
            updated_at: now,
            completed_at: None,
            model: input.public_model,
            driver: input.driver,
            billing_unit: input.billing_unit,
            status: "preparing".to_owned(),
            upstream_job_id: None,
            estimated_units: input.estimated_units,
            billed_units: None,
            cost: "0".to_owned(),
            error_code: None,
            result: None,
            assets: Vec::new(),
        }))
    }

    /// Promotes the preparation owner to the worker-visible queue after its
    /// content-addressed request object is durable.
    pub async fn attach_generation_job_request(
        &self,
        key_id: Uuid,
        job_id: Uuid,
        request_hash: &str,
        request_object: &str,
    ) -> Result<AttachGenerationJobResult, AppError> {
        if !is_sha256_hex(request_hash) || !is_content_object_location(request_object) {
            return Err(AppError::BadRequest(
                "generation request archive metadata is invalid".into(),
            ));
        }
        let mut indeterminate = false;
        for retry_delay in [0_u64, 25, 100, 250] {
            if retry_delay > 0 {
                tokio::time::sleep(Duration::from_millis(retry_delay)).await;
            }
            let now = unix_millis();
            let promoted = sqlx::query(
                "UPDATE generation_jobs SET request_object = $1, status = 'queued', next_attempt_at = $2, updated_at = $3, lease_expires_at = NULL WHERE id = $4 AND key_id = $5 AND status = 'preparing' AND request_object = '' AND request_hash = $6 AND lease_expires_at > $7",
            )
            .bind(request_object)
            .bind(now)
            .bind(now)
            .bind(job_id.to_string())
            .bind(key_id.to_string())
            .bind(request_hash)
            .bind(now)
            .execute(&self.pool)
            .await;
            match promoted {
                Ok(promoted) if promoted.rows_affected() == 1 => {
                    // The autocommit is acknowledged and the job is definitely
                    // queued. A transient follow-up view read must never turn
                    // that accepted job into an HTTP 500 that invites a
                    // no-idempotency duplicate.
                    for read_delay in [0_u64, 25, 100, 250] {
                        if read_delay > 0 {
                            tokio::time::sleep(Duration::from_millis(read_delay)).await;
                        }
                        if let Ok(job) = self.generation_job(key_id, job_id).await {
                            return Ok(AttachGenerationJobResult::Attached(Box::new(job)));
                        }
                    }
                    return Ok(AttachGenerationJobResult::Indeterminate);
                }
                Ok(_) => {
                    match self
                        .generation_job_after_request_attach(
                            key_id,
                            job_id,
                            request_hash,
                            request_object,
                        )
                        .await
                    {
                        Ok(Some(job)) => {
                            return Ok(AttachGenerationJobResult::Attached(Box::new(job)));
                        }
                        Ok(None) if !indeterminate => {
                            return Err(AppError::Conflict(
                                "generation request archive owner changed before queueing".into(),
                            ));
                        }
                        Ok(None) | Err(_) => {
                            indeterminate = true;
                            continue;
                        }
                    }
                }
                Err(error) if is_indeterminate_database_error(&error) => {
                    indeterminate = true;
                    match self
                        .generation_job_after_request_attach(
                            key_id,
                            job_id,
                            request_hash,
                            request_object,
                        )
                        .await
                    {
                        Ok(Some(job)) => {
                            return Ok(AttachGenerationJobResult::Attached(Box::new(job)));
                        }
                        Ok(None) => continue,
                        Err(_) => continue,
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        if indeterminate {
            // Returning the already admitted job identity is safer than a 500:
            // a caller without Idempotency-Key must not create a second job
            // after an attach commit whose acknowledgement was lost.
            Ok(AttachGenerationJobResult::Indeterminate)
        } else {
            Err(AppError::Internal)
        }
    }

    /// Queues a prepared job and binds its unique request object in one
    /// transaction. Exact retries recover a commit whose acknowledgement was
    /// lost; a stale writer token can never publish a locator.
    pub async fn attach_generation_job_request_staged(
        &self,
        key_id: Uuid,
        job_id: Uuid,
        request_hash: &str,
        request_object: &str,
        staging_lease: &ArchiveStagingWriteLease,
    ) -> Result<AttachGenerationJobResult, AppError> {
        if !is_sha256_hex(request_hash)
            || staging_lease.key.owner != ArchiveStagingOwner::GenerationJob(job_id)
            || staging_lease.key.purpose != ArchiveStagingPurpose::Request
            || !locator_matches_prefix(request_object, &staging_lease.key.canonical_prefix())
        {
            return Err(AppError::BadRequest(
                "generation request staging metadata is invalid".into(),
            ));
        }
        let mut indeterminate = false;
        for retry_delay in [0_u64, 25, 100, 250] {
            if retry_delay > 0 {
                tokio::time::sleep(Duration::from_millis(retry_delay)).await;
            }
            let now = unix_millis();
            let mut transaction = match self.pool.begin().await {
                Ok(transaction) => transaction,
                Err(error) if is_indeterminate_database_error(&error) => {
                    indeterminate = true;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let promoted = sqlx::query(
                "UPDATE generation_jobs SET request_object = $1, status = 'queued', next_attempt_at = $2, updated_at = $3, lease_expires_at = NULL WHERE id = $4 AND key_id = $5 AND status = 'preparing' AND request_object = '' AND request_hash = $6 AND lease_expires_at > $7",
            )
            .bind(request_object)
            .bind(now)
            .bind(now)
            .bind(job_id.to_string())
            .bind(key_id.to_string())
            .bind(request_hash)
            .bind(now)
            .execute(&mut *transaction)
            .await;
            let promoted = match promoted {
                Ok(promoted) => promoted,
                Err(error) if is_indeterminate_database_error(&error) => {
                    indeterminate = true;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if promoted.rows_affected() == 0 {
                let exact: Option<i64> = sqlx::query_scalar(
                    "SELECT 1 FROM generation_jobs WHERE id = $1 AND key_id = $2 AND request_hash = $3 AND request_object = $4 AND status <> 'preparing'",
                )
                .bind(job_id.to_string())
                .bind(key_id.to_string())
                .bind(request_hash)
                .bind(request_object)
                .fetch_optional(&mut *transaction)
                .await?;
                if exact.is_none() {
                    transaction.rollback().await?;
                    if indeterminate {
                        continue;
                    }
                    return Err(AppError::Conflict(
                        "generation request archive owner changed before queueing".into(),
                    ));
                }
            }
            let bound = super::super::archive_staging::bind_archive_staging_attempt_in_transaction(
                &mut transaction,
                self.backend,
                staging_lease,
                request_object,
            )
            .await?;
            if !bound {
                transaction.rollback().await?;
                return Err(AppError::Conflict(
                    "generation request staging writer was fenced".into(),
                ));
            }
            match transaction.commit().await {
                Ok(()) => {
                    for read_delay in [0_u64, 25, 100, 250] {
                        if read_delay > 0 {
                            tokio::time::sleep(Duration::from_millis(read_delay)).await;
                        }
                        if let Ok(job) = self.generation_job(key_id, job_id).await {
                            return Ok(AttachGenerationJobResult::Attached(Box::new(job)));
                        }
                    }
                    return Ok(AttachGenerationJobResult::Indeterminate);
                }
                Err(error) if is_indeterminate_database_error(&error) => {
                    indeterminate = true;
                }
                Err(error) => return Err(error.into()),
            }
        }
        if indeterminate {
            Ok(AttachGenerationJobResult::Indeterminate)
        } else {
            Err(AppError::Internal)
        }
    }

    async fn generation_job_after_request_attach(
        &self,
        key_id: Uuid,
        job_id: Uuid,
        request_hash: &str,
        request_object: &str,
    ) -> Result<Option<GenerationJobView>, AppError> {
        let row = sqlx::query(
            "SELECT status, request_hash, request_object FROM generation_jobs WHERE id = $1 AND key_id = $2",
        )
        .bind(job_id.to_string())
        .bind(key_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let status: String = row.try_get("status")?;
        let stored_hash: Option<String> = row.try_get("request_hash")?;
        let stored_object: String = row.try_get("request_object")?;
        if status != "preparing"
            && stored_hash.as_deref() == Some(request_hash)
            && stored_object == request_object
        {
            return self.generation_job(key_id, job_id).await.map(Some);
        }
        Ok(None)
    }

    pub async fn fail_generation_job_preparation(
        &self,
        key_id: Uuid,
        job_id: Uuid,
        error_code: &str,
    ) -> Result<GenerationJobView, AppError> {
        validate_generation_preparation_error(error_code)?;
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE generation_jobs SET updated_at = updated_at WHERE id = $1 AND key_id = $2 AND status = 'preparing'",
        )
        .bind(job_id.to_string())
        .bind(key_id.to_string())
        .execute(&mut *transaction)
        .await?;
        if locked.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(AppError::Conflict(
                "generation request archive owner changed before failure settlement".into(),
            ));
        }
        fail_preparing_generation_in_transaction(&mut transaction, job_id, error_code, now).await?;
        super::cleanup_archive_staging_purpose_in_transaction(
            &mut transaction,
            ArchiveStagingOwner::GenerationJob(job_id),
            ArchiveStagingPurpose::Request,
        )
        .await?;
        transaction.commit().await?;
        self.generation_job(key_id, job_id).await
    }

    /// Refunds preparation owners that died after admission but before their
    /// request CAS was attached. RPM remains charged for the admitted attempt.
    pub async fn expire_preparing_generation_jobs(&self, limit: i64) -> Result<u64, AppError> {
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT id FROM generation_jobs WHERE status = 'preparing' AND lease_expires_at <= $1 ORDER BY lease_expires_at, id FOR UPDATE SKIP LOCKED LIMIT $2"
            }
            DatabaseBackend::Sqlite => {
                "SELECT id FROM generation_jobs WHERE status = 'preparing' AND lease_expires_at <= $1 ORDER BY lease_expires_at, id LIMIT $2"
            }
        };
        let rows = sqlx::query(select)
            .bind(now)
            .bind(limit.clamp(1, 1_000))
            .fetch_all(&mut *transaction)
            .await?;
        let mut expired = 0_u64;
        for row in rows {
            let job_id = parse_uuid(row.try_get("id")?)?;
            if fail_preparing_generation_in_transaction(
                &mut transaction,
                job_id,
                "generation_archive_expired",
                now,
            )
            .await?
            {
                super::cleanup_archive_staging_purpose_in_transaction(
                    &mut transaction,
                    ArchiveStagingOwner::GenerationJob(job_id),
                    ArchiveStagingPurpose::Request,
                )
                .await?;
                expired += 1;
            }
        }
        transaction.commit().await?;
        Ok(expired)
    }

    pub async fn create_generation_job_idempotent(
        &self,
        input: CreateGenerationJobInput,
        idempotency: Option<&GenerationJobIdempotency>,
    ) -> Result<CreateGenerationJobResult, AppError> {
        if let Some(idempotency) = idempotency {
            validate_generation_job_idempotency(idempotency)?;
        }
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO generation_jobs (id, tenant_id, key_id, upstream_account_id, reservation_id, public_model, upstream_model, driver, status, request_object, estimated_units, billing_unit_snapshot, micros_per_unit_snapshot, client_idempotency_key, request_hash, next_attempt_at, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'queued', $9, $10, $11, $12, $13, $14, $15, $16, $17) ON CONFLICT(key_id, client_idempotency_key) DO NOTHING",
        )
        .bind(input.job_id.to_string())
        .bind(input.key.tenant_id.to_string())
        .bind(input.key.key_id.to_string())
        .bind(input.upstream_account_id.to_string())
        .bind(input.reservation.id.to_string())
        .bind(&input.public_model)
        .bind(&input.upstream_model)
        .bind(&input.driver)
        .bind(input.request_object)
        .bind(input.estimated_units)
        .bind(&input.billing_unit)
        .bind(input.micros_per_unit)
        .bind(idempotency.map(|value| value.key.as_str()))
        .bind(idempotency.map(|value| value.request_hash.as_str()))
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if inserted.rows_affected() == 0 {
            let row = sqlx::query(
                "SELECT id, created_at, updated_at, completed_at, public_model, driver, billing_unit_snapshot, status, upstream_job_id, estimated_units, billed_units, cost_micros, error_code, result_json, request_hash, reservation_id FROM generation_jobs WHERE key_id = $1 AND client_idempotency_key = $2",
            )
            .bind(input.key.key_id.to_string())
            .bind(idempotency.map(|value| value.key.as_str()))
            .fetch_one(&mut *transaction)
            .await?;
            let existing_hash: Option<String> = row.try_get("request_hash")?;
            let existing_reservation_id = parse_uuid(row.try_get("reservation_id")?)?;
            let replayed = generation_job_view(row)?;
            transaction.commit().await?;

            if existing_reservation_id != input.reservation.id {
                self.settle_usage(&input.reservation, 0, 0).await?;
            }
            if existing_hash.as_deref() != idempotency.map(|value| value.request_hash.as_str()) {
                return Err(AppError::BadRequest(
                    "Idempotency-Key was already used for a different generation request".into(),
                ));
            }
            return Ok(CreateGenerationJobResult::Replayed(replayed));
        }
        let event_id = Uuid::now_v7().to_string();
        let tenant_id = input.key.tenant_id.to_string();
        let key_id = input.key.key_id.to_string();
        let request_id = input.job_id.to_string();
        if claim_request_event_locator(
            &mut transaction,
            &event_id,
            now,
            &tenant_id,
            &key_id,
            &request_id,
        )
        .await?
        {
            sqlx::query(
                "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, 'started', 'generation', $6, 0, 0, 0)",
            )
            .bind(&event_id)
            .bind(&tenant_id)
            .bind(&key_id)
            .bind(&request_id)
            .bind(now)
            .bind(&input.public_model)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(CreateGenerationJobResult::Created(GenerationJobView {
            job_id: input.job_id,
            created_at: now,
            updated_at: now,
            completed_at: None,
            model: input.public_model,
            driver: input.driver,
            billing_unit: input.billing_unit,
            status: "queued".to_owned(),
            upstream_job_id: None,
            estimated_units: input.estimated_units,
            billed_units: None,
            cost: "0".to_owned(),
            error_code: None,
            result: None,
            assets: Vec::new(),
        }))
    }

    pub async fn list_generation_jobs(
        &self,
        key_id: Uuid,
        limit: i64,
    ) -> Result<Vec<GenerationJobView>, AppError> {
        let rows = sqlx::query(
            "SELECT id, created_at, updated_at, completed_at, public_model, driver, billing_unit_snapshot, status, upstream_job_id, estimated_units, billed_units, cost_micros, error_code, result_json FROM generation_jobs WHERE key_id = $1 ORDER BY created_at DESC, id DESC LIMIT $2",
        )
        .bind(key_id.to_string())
        .bind(limit.clamp(1, 200))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(generation_job_view).collect()
    }

    pub async fn generation_job(
        &self,
        key_id: Uuid,
        job_id: Uuid,
    ) -> Result<GenerationJobView, AppError> {
        let row = sqlx::query(
            "SELECT id, created_at, updated_at, completed_at, public_model, driver, billing_unit_snapshot, status, upstream_job_id, estimated_units, billed_units, cost_micros, error_code, result_json FROM generation_jobs WHERE id = $1 AND key_id = $2",
        )
        .bind(job_id.to_string())
        .bind(key_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        generation_job_view(row)
    }

    pub async fn generation_asset_for_key(
        &self,
        key_id: Uuid,
        job_id: Uuid,
        asset_id: Uuid,
    ) -> Result<GenerationAssetDownload, AppError> {
        let row = sqlx::query(
            "SELECT a.id, a.asset_index, a.object_locator, a.mime_type, a.size_bytes, a.filename FROM generation_assets a JOIN generation_jobs j ON j.id = a.job_id WHERE a.id = $1 AND a.job_id = $2 AND j.key_id = $3",
        )
        .bind(asset_id.to_string())
        .bind(job_id.to_string())
        .bind(key_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        generation_asset_download(row)
    }

    pub async fn generation_asset_for_tenant(
        &self,
        tenant_external_id: &str,
        job_id: Uuid,
        asset_id: Uuid,
    ) -> Result<GenerationAssetDownload, AppError> {
        let row = sqlx::query(
            "SELECT a.id, a.asset_index, a.object_locator, a.mime_type, a.size_bytes, a.filename FROM generation_assets a JOIN generation_jobs j ON j.id = a.job_id JOIN tenants t ON t.id = j.tenant_id WHERE a.id = $1 AND a.job_id = $2 AND t.external_id = $3",
        )
        .bind(asset_id.to_string())
        .bind(job_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        generation_asset_download(row)
    }

    pub async fn generation_asset_global(
        &self,
        job_id: Uuid,
        asset_id: Uuid,
    ) -> Result<GenerationAssetDownload, AppError> {
        let row = sqlx::query(
            "SELECT a.id, a.asset_index, a.object_locator, a.mime_type, a.size_bytes, a.filename FROM generation_assets a JOIN generation_jobs j ON j.id = a.job_id WHERE a.id = $1 AND a.job_id = $2",
        )
        .bind(asset_id.to_string())
        .bind(job_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        generation_asset_download(row)
    }

    /// Atomically cancels a queued job, or fences a running job for the
    /// driver-specific cancellation worker. A running reservation is not
    /// refunded until that worker proves the upstream cancellation succeeded.
    pub async fn cancel_generation_job(
        &self,
        key_id: Uuid,
        job_id: Uuid,
    ) -> Result<GenerationJobView, AppError> {
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT j.status, j.lease_owner, j.lease_expires_at, j.staged_assets_json, j.created_at, j.tenant_id, j.public_model, r.id AS reservation_id, r.account_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start, r.status AS reservation_status, r.actual_micros FROM generation_jobs j JOIN usage_reservations r ON r.id = j.reservation_id WHERE j.id = $1 AND j.key_id = $2 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT j.status, j.lease_owner, j.lease_expires_at, j.staged_assets_json, j.created_at, j.tenant_id, j.public_model, r.id AS reservation_id, r.account_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start, r.status AS reservation_status, r.actual_micros FROM generation_jobs j JOIN usage_reservations r ON r.id = j.reservation_id WHERE j.id = $1 AND j.key_id = $2"
            }
        };
        let row = sqlx::query(select)
            .bind(job_id.to_string())
            .bind(key_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
        let status: String = row.try_get("status")?;
        if status == "cancelled" {
            aggregate_terminal_generation_job(&mut transaction, &job_id.to_string(), now).await?;
            transaction.commit().await?;
            return self.generation_job(key_id, job_id).await;
        }
        if status == "cancelling" {
            transaction.commit().await?;
            return self.generation_job(key_id, job_id).await;
        }
        if status == "running" {
            let staged_assets: Option<String> = row.try_get("staged_assets_json")?;
            if staged_assets.is_some() {
                return Err(AppError::BadRequest(
                    "generation result is already being finalized".into(),
                ));
            }
            let requested = sqlx::query(
                "UPDATE generation_jobs SET status = 'cancelling', next_attempt_at = $1, lease_owner = NULL, lease_expires_at = NULL, error_code = NULL, updated_at = $2 WHERE id = $3 AND key_id = $4 AND status = 'running' AND staged_assets_json IS NULL",
            )
            .bind(now)
            .bind(now)
            .bind(job_id.to_string())
            .bind(key_id.to_string())
            .execute(&mut *transaction)
            .await?;
            if requested.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "generation state changed while cancellation was requested".into(),
                ));
            }
            transaction.commit().await?;
            return self.generation_job(key_id, job_id).await;
        }
        if status != "queued" {
            return Err(AppError::BadRequest(
                "generation job cannot be cancelled in its current state".into(),
            ));
        }
        let lease_owner: Option<String> = row.try_get("lease_owner")?;
        let lease_expires_at: Option<i64> = row.try_get("lease_expires_at")?;
        if lease_owner.is_some() && lease_expires_at.is_some_and(|expires_at| expires_at >= now) {
            return Err(AppError::BadRequest(
                "generation job is currently being submitted upstream".into(),
            ));
        }

        let reservation_id: String = row.try_get("reservation_id")?;
        let account_id: String = row.try_get("account_id")?;
        let reserved_micros: i64 = row.try_get("reserved_micros")?;
        let reserved_tokens: i64 = row.try_get("reserved_tokens")?;
        let rate_window_start: i64 = row.try_get("rate_window_start")?;
        let reservation_status: String = row.try_get("reservation_status")?;
        let actual_micros: Option<i64> = row.try_get("actual_micros")?;
        let created_at: i64 = row.try_get("created_at")?;
        let tenant_id: String = row.try_get("tenant_id")?;
        let public_model: String = row.try_get("public_model")?;

        if reservation_status != "reserved" && actual_micros != Some(0) {
            return Err(AppError::BadRequest(
                "generation job usage has already been settled".into(),
            ));
        }
        if reservation_status == "reserved" {
            lock_key_budget_state(&mut transaction, key_id, now).await?;
            sqlx::query("UPDATE credit_accounts SET updated_at = updated_at WHERE id = $1")
                .bind(&account_id)
                .execute(&mut *transaction)
                .await?;
            let settled = sqlx::query(
                "UPDATE usage_reservations SET actual_micros = 0, status = 'settled', settled_at = $1 WHERE id = $2 AND status = 'reserved'",
            )
            .bind(now)
            .bind(&reservation_id)
            .execute(&mut *transaction)
            .await?;
            if settled.rows_affected() != 1 {
                return Err(AppError::Internal);
            }
            let budget_state = sqlx::query(
                "UPDATE key_budget_state SET reserved_micros = reserved_micros - $1, updated_at = $2 WHERE key_id = $3 AND reserved_micros >= $4",
            )
            .bind(reserved_micros)
            .bind(now)
            .bind(key_id.to_string())
            .bind(reserved_micros)
            .execute(&mut *transaction)
            .await?;
            if budget_state.rows_affected() != 1 {
                return Err(AppError::Internal);
            }
            sqlx::query(
                "UPDATE credit_accounts SET available_micros = available_micros + $1, reserved_micros = reserved_micros - $2, updated_at = $3 WHERE id = $4",
            )
            .bind(reserved_micros)
            .bind(reserved_micros)
            .bind(now)
            .bind(&account_id)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "UPDATE rate_limit_windows SET tokens = CASE WHEN tokens > $1 THEN tokens - $2 ELSE 0 END WHERE key_id = $3 AND window_start = $4",
            )
            .bind(reserved_tokens)
            .bind(reserved_tokens)
            .bind(key_id.to_string())
            .bind(rate_window_start)
            .execute(&mut *transaction)
            .await?;
            let usage_ledger_entry_id = Uuid::now_v7();
            let usage_ledger = sqlx::query(
                "INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) SELECT $1, $2, $3, 'usage', 0, currency, $4, $5 FROM credit_accounts WHERE id = $6",
            )
            .bind(usage_ledger_entry_id.to_string())
            .bind(&account_id)
            .bind(key_id.to_string())
            .bind(&reservation_id)
            .bind(now)
            .bind(&account_id)
            .execute(&mut *transaction)
            .await?;
            if usage_ledger.rows_affected() != 1 {
                return Err(AppError::Internal);
            }
            sqlx::query(
                "INSERT INTO key_budget_usage_events (usage_entry_id, reservation_id, key_id, account_id, amount_micros, settled_at) VALUES ($1, $2, $3, $4, 0, $5)",
            )
            .bind(usage_ledger_entry_id.to_string())
            .bind(&reservation_id)
            .bind(key_id.to_string())
            .bind(&account_id)
            .bind(now)
            .execute(&mut *transaction)
            .await?;
        }

        let cancelled = sqlx::query(
            "UPDATE generation_jobs SET status = 'cancelled', billed_units = 0, cost_micros = 0, error_code = 'cancelled_by_user', completed_at = $1, updated_at = $2, lease_owner = NULL, lease_expires_at = NULL WHERE id = $3 AND key_id = $4 AND status = 'queued' AND (lease_expires_at IS NULL OR lease_expires_at < $5)",
        )
        .bind(now)
        .bind(now)
        .bind(job_id.to_string())
        .bind(key_id.to_string())
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if cancelled.rows_affected() != 1 {
            return Err(AppError::BadRequest(
                "generation job is currently being submitted upstream".into(),
            ));
        }
        aggregate_terminal_generation_job(&mut transaction, &job_id.to_string(), now).await?;
        let event_id = Uuid::now_v7().to_string();
        let key_id_string = key_id.to_string();
        let request_id = job_id.to_string();
        if claim_request_event_locator(
            &mut transaction,
            &event_id,
            now,
            &tenant_id,
            &key_id_string,
            &request_id,
        )
        .await?
        {
            sqlx::query(
                "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) VALUES ($1, $2, $3, $4, $5, 'finished', 'generation', $6, 499, $7, 0, 0, 0, 'cancelled_by_user')",
            )
            .bind(&event_id)
            .bind(&tenant_id)
            .bind(&key_id_string)
            .bind(&request_id)
            .bind(now)
            .bind(public_model)
            .bind(now.saturating_sub(created_at))
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        self.generation_job(key_id, job_id).await
    }

    pub async fn claim_generation_job(
        &self,
        worker_id: &str,
    ) -> Result<Option<GenerationJobWork>, AppError> {
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT id FROM generation_jobs WHERE status IN ('queued', 'running', 'submitting', 'cancelling') AND next_attempt_at <= $1 AND (lease_expires_at IS NULL OR lease_expires_at < $2) ORDER BY next_attempt_at, created_at, id FOR UPDATE SKIP LOCKED LIMIT 1"
            }
            DatabaseBackend::Sqlite => {
                "SELECT id FROM generation_jobs WHERE status IN ('queued', 'running', 'submitting', 'cancelling') AND next_attempt_at <= $1 AND (lease_expires_at IS NULL OR lease_expires_at < $2) ORDER BY next_attempt_at, created_at, id LIMIT 1"
            }
        };
        let candidate = sqlx::query(select)
            .bind(now)
            .bind(now)
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(candidate) = candidate else {
            transaction.commit().await?;
            return Ok(None);
        };
        let job_id: String = candidate.try_get("id")?;
        let claimed = sqlx::query(
            "UPDATE generation_jobs SET lease_owner = $1, lease_expires_at = $2, attempt_count = attempt_count + 1, updated_at = $3 WHERE id = $4 AND status IN ('queued', 'running', 'submitting', 'cancelling') AND (lease_expires_at IS NULL OR lease_expires_at < $5)",
        )
        .bind(worker_id)
        .bind(now.saturating_add(60_000))
        .bind(now)
        .bind(&job_id)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if claimed.rows_affected() == 0 {
            transaction.commit().await?;
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT j.id, j.created_at, j.tenant_id, j.key_id, j.model_route_id, j.upstream_account_id, j.public_model, j.upstream_model, j.driver, j.status, j.request_object, j.upstream_job_id, j.submission_nonce, j.staged_assets_json, j.billing_unit_snapshot, j.estimated_units, j.attempt_count, j.failure_count, r.id AS reservation_id, r.account_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start, j.micros_per_unit_snapshot FROM generation_jobs j JOIN usage_reservations r ON r.id = j.reservation_id WHERE j.id = $1",
        )
        .bind(&job_id)
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;
        let micros_per_unit: i64 = row.try_get("micros_per_unit_snapshot")?;
        let billing_unit: String = row.try_get("billing_unit_snapshot")?;
        let key_id = parse_uuid(row.try_get("key_id")?)?;
        let submission_nonce = row
            .try_get::<Option<String>, _>("submission_nonce")?
            .map(parse_uuid)
            .transpose()?;
        let staged_assets = row
            .try_get::<Option<String>, _>("staged_assets_json")?
            .map(|value| serde_json::from_str(&value).map_err(|_| AppError::Internal))
            .transpose()?;
        Ok(Some(GenerationJobWork {
            job_id: parse_uuid(row.try_get("id")?)?,
            created_at: row.try_get("created_at")?,
            tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
            key_id,
            model_route_id: row
                .try_get::<Option<String>, _>("model_route_id")?
                .map(parse_uuid)
                .transpose()?,
            upstream_account_id: parse_uuid(row.try_get("upstream_account_id")?)?,
            reservation: UsageReservation {
                id: parse_uuid(row.try_get("reservation_id")?)?,
                account_id: parse_uuid(row.try_get("account_id")?)?,
                key_id,
                reserved_micros: row.try_get("reserved_micros")?,
                input_micros_per_million: 0,
                output_micros_per_million: if billing_unit == "megapixel" {
                    micros_per_unit
                } else {
                    micros_per_unit
                        .checked_mul(1_000_000)
                        .ok_or(AppError::Internal)?
                },
                price_tiers: Vec::new(),
                rate_window_start: row.try_get("rate_window_start")?,
                reserved_tokens: row.try_get("reserved_tokens")?,
            },
            public_model: row.try_get("public_model")?,
            upstream_model: row.try_get("upstream_model")?,
            driver: row.try_get("driver")?,
            status: row.try_get("status")?,
            request_object: row.try_get("request_object")?,
            upstream_job_id: row.try_get("upstream_job_id")?,
            submission_nonce,
            staged_assets,
            billing_unit,
            estimated_units: row.try_get("estimated_units")?,
            attempt_count: row.try_get("attempt_count")?,
            failure_count: row.try_get("failure_count")?,
        }))
    }

    /// Loads the exact upstream candidate frozen at generation admission.
    ///
    /// Route membership, priority, enablement, and model mappings are
    /// deliberately not consulted here: changing them must affect only new
    /// admissions. The account itself must still be active and have a current,
    /// non-revoked credential, which makes disabled/deleted accounts and broken
    /// rotations fail closed without silently selecting another candidate.
    pub async fn load_generation_upstream_snapshot(
        &self,
        job: &GenerationJobWork,
        key_material: &[u8],
    ) -> Result<Option<ResolvedUpstream>, AppError> {
        let row = sqlx::query(
            "SELECT a.id AS account_id, a.driver, a.config_json, c.credential_ciphertext
             FROM upstream_accounts a
             JOIN upstream_credentials c
               ON c.upstream_account_id = a.id
              AND c.generation = a.credential_generation
              AND c.revoked_at IS NULL
             WHERE a.id = $1 AND a.tenant_id = $2 AND a.status = 'active'",
        )
        .bind(job.upstream_account_id.to_string())
        .bind(job.tenant_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let current_driver: String = row.try_get("driver")?;
        if current_driver != job.driver {
            return Ok(None);
        }
        let config_json: String = row.try_get("config_json")?;
        let config: serde_json::Value =
            serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?;
        let base_url = validate_config(&config)?;
        let ciphertext: String = row.try_get("credential_ciphertext")?;
        Ok(Some(ResolvedUpstream {
            route_id: job.model_route_id.unwrap_or_else(Uuid::nil),
            account_id: parse_uuid(row.try_get("account_id")?)?,
            driver: job.driver.clone(),
            base_url,
            config,
            upstream_model: job.upstream_model.clone(),
            credential: open_credential(&ciphertext, key_material)?,
        }))
    }

    pub async fn mark_generation_submitting(
        &self,
        job_id: Uuid,
        worker_id: &str,
        submission_nonce: Uuid,
    ) -> Result<(), AppError> {
        generation_update_claimed(
            sqlx::query(
                "UPDATE generation_jobs SET status = 'submitting', submission_nonce = $1, updated_at = $2 WHERE id = $3 AND lease_owner = $4 AND status = 'queued' AND upstream_job_id IS NULL",
            )
            .bind(submission_nonce.to_string())
            .bind(unix_millis())
            .bind(job_id.to_string())
            .bind(worker_id)
            .execute(&self.pool)
            .await?,
        )
    }

    pub async fn mark_generation_submitted(
        &self,
        job_id: Uuid,
        worker_id: &str,
        submission_nonce: Uuid,
        upstream_job_id: &str,
    ) -> Result<(), AppError> {
        generation_update_claimed(
            sqlx::query("UPDATE generation_jobs SET status = 'running', upstream_job_id = $1, submission_nonce = NULL, failure_count = 0, error_code = NULL, next_attempt_at = $2, lease_owner = NULL, lease_expires_at = NULL, updated_at = $3 WHERE id = $4 AND lease_owner = $5 AND status = 'submitting' AND submission_nonce = $6")
                .bind(upstream_job_id)
                .bind(unix_millis().saturating_add(2_000))
                .bind(unix_millis())
                .bind(job_id.to_string())
                .bind(worker_id)
                .bind(submission_nonce.to_string())
                .execute(&self.pool)
                .await?,
        )
    }

    pub async fn save_generation_staged_assets(
        &self,
        job_id: Uuid,
        worker_id: &str,
        staged: &GenerationStagedAssets,
    ) -> Result<(), AppError> {
        let staged_json = serde_json::to_string(staged).map_err(|_| AppError::Internal)?;
        generation_update_claimed(
            sqlx::query(
                "UPDATE generation_jobs SET staged_assets_json = $1, updated_at = $2 WHERE id = $3 AND lease_owner = $4 AND status = 'running' AND upstream_job_id IS NOT NULL AND staged_assets_json IS NULL",
            )
            .bind(staged_json)
            .bind(unix_millis())
            .bind(job_id.to_string())
            .bind(worker_id)
            .execute(&self.pool)
            .await?,
        )
    }

    /// Publishes the recovery manifest, its normalized asset rows, and the
    /// staging binding atomically. `Ok(false)` is a durable terminal-loser
    /// outcome: the unused attempt was moved to cleanup-pending in this same
    /// transaction.
    pub async fn save_generation_staged_assets_staged(
        &self,
        job_id: Uuid,
        worker_id: &str,
        staged: &GenerationStagedAssets,
        staging_lease: &ArchiveStagingWriteLease,
    ) -> Result<bool, AppError> {
        let prefix = staging_lease.key.canonical_prefix();
        if staging_lease.key.owner != ArchiveStagingOwner::GenerationJob(job_id)
            || staging_lease.key.purpose != ArchiveStagingPurpose::Assets
            || staged.attempt_nonce != staging_lease.key.attempt_id
            || staged.assets.is_empty()
            || staged
                .assets
                .iter()
                .any(|asset| !locator_matches_prefix(&asset.object_locator, &prefix))
        {
            return Err(AppError::BadRequest(
                "generation staged asset binding is invalid".into(),
            ));
        }
        let now = unix_millis();
        let staged_json = serde_json::to_string(staged).map_err(|_| AppError::Internal)?;
        let mut transaction = self.pool.begin().await?;
        let saved = sqlx::query(
            "UPDATE generation_jobs SET staged_assets_json = $1, updated_at = $2 WHERE id = $3 AND lease_owner = $4 AND status = 'running' AND upstream_job_id IS NOT NULL AND staged_assets_json IS NULL",
        )
        .bind(&staged_json)
        .bind(now)
        .bind(job_id.to_string())
        .bind(worker_id)
        .execute(&mut *transaction)
        .await?;
        if saved.rows_affected() == 0 {
            let exact_manifest: Option<String> = sqlx::query_scalar(
                "SELECT staged_assets_json FROM generation_jobs WHERE id = $1 AND staged_assets_json IS NOT NULL",
            )
            .bind(job_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?;
            let exact = exact_manifest.as_deref() == Some(staged_json.as_str())
                && generation_assets_match(&mut transaction, job_id, &staged.assets).await?;
            if !exact {
                super::cleanup_archive_staging_attempt_in_transaction(
                    &mut transaction,
                    staging_lease.key,
                )
                .await?;
                transaction.commit().await?;
                return Ok(false);
            }
        } else {
            insert_generation_assets_in_transaction(&mut transaction, job_id, &staged.assets, now)
                .await?;
        }
        let bound = super::super::archive_staging::bind_archive_staging_attempt_in_transaction(
            &mut transaction,
            self.backend,
            staging_lease,
            &prefix,
        )
        .await?;
        if !bound {
            transaction.rollback().await?;
            return Err(AppError::Conflict(
                "generation asset staging writer was fenced".into(),
            ));
        }
        transaction.commit().await?;
        Ok(true)
    }

    pub async fn reschedule_generation_job(
        &self,
        job_id: Uuid,
        worker_id: &str,
        delay_ms: i64,
        error_code: Option<&str>,
    ) -> Result<(), AppError> {
        let now = unix_millis();
        generation_update_claimed(
            sqlx::query("UPDATE generation_jobs SET next_attempt_at = $1, error_code = $2, failure_count = CASE WHEN $3 IS NULL THEN 0 ELSE failure_count + 1 END, lease_owner = NULL, lease_expires_at = NULL, updated_at = $4 WHERE id = $5 AND lease_owner = $6")
                .bind(now.saturating_add(delay_ms.max(500)))
                .bind(error_code)
                .bind(error_code)
                .bind(now)
                .bind(job_id.to_string())
                .bind(worker_id)
                .execute(&self.pool)
                .await?,
        )
    }

    pub async fn renew_generation_lease(
        &self,
        job_id: Uuid,
        worker_id: &str,
    ) -> Result<(), AppError> {
        let now = unix_millis();
        generation_update_claimed(
            sqlx::query(
                "UPDATE generation_jobs SET lease_expires_at = $1, updated_at = $2 WHERE id = $3 AND lease_owner = $4 AND status IN ('queued', 'running', 'submitting', 'cancelling')",
            )
            .bind(now.saturating_add(60_000))
            .bind(now)
            .bind(job_id.to_string())
            .bind(worker_id)
            .execute(&self.pool)
            .await?,
        )
    }

    pub async fn finish_generation_job(
        &self,
        input: FinishGenerationJobInput<'_>,
    ) -> Result<i64, AppError> {
        if !matches!(input.status, "succeeded" | "failed" | "cancelled") {
            return Err(AppError::BadRequest(
                "invalid terminal generation status".into(),
            ));
        }
        if input.status == "succeeded" {
            if input.billed_units <= 0 || input.error_code.is_some() || input.assets.is_empty() {
                return Err(AppError::BadRequest(
                    "a successful generation requires billed units and archived assets".into(),
                ));
            }
            if input.assets.iter().any(|asset| {
                asset.index < 0
                    || asset.object_locator.trim().is_empty()
                    || asset.mime_type.trim().is_empty()
                    || asset.size_bytes <= 0
                    || asset.filename.trim().is_empty()
            }) {
                return Err(AppError::BadRequest(
                    "a successful generation contains an invalid archived asset".into(),
                ));
            }
            if input.assets.iter().enumerate().any(|(index, asset)| {
                input.assets[index + 1..]
                    .iter()
                    .any(|other| other.asset_id == asset.asset_id || other.index == asset.index)
            }) {
                return Err(AppError::BadRequest(
                    "a successful generation contains duplicate archived assets".into(),
                ));
            }
            if input.staged_assets.is_some_and(|staged| {
                staged.billed_units != input.billed_units || staged.assets != input.assets
            }) {
                return Err(AppError::BadRequest(
                    "a successful generation must match its staged asset manifest".into(),
                ));
            }
        } else {
            let allowed_billed_failure = input.error_code
                == Some("upstream_usage_exceeds_contract")
                && input.billed_units > 0;
            if (!allowed_billed_failure && input.billed_units != 0)
                || !input.assets.is_empty()
                || !input
                    .error_code
                    .is_some_and(is_allowed_generation_error_code)
            {
                return Err(AppError::BadRequest(
                    "a failed generation requires a fixed error code and valid billing units"
                        .into(),
                ));
            }
        }
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT j.status, j.lease_owner, j.tenant_id, j.key_id, j.driver, j.created_at, j.estimated_units, j.billed_units, j.cost_micros, j.result_json, j.error_code, j.staged_assets_json, j.reservation_id, j.billing_unit_snapshot, j.micros_per_unit_snapshot, r.account_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start, r.status AS reservation_status, r.actual_micros FROM generation_jobs j JOIN usage_reservations r ON r.id = j.reservation_id WHERE j.id = $1 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT j.status, j.lease_owner, j.tenant_id, j.key_id, j.driver, j.created_at, j.estimated_units, j.billed_units, j.cost_micros, j.result_json, j.error_code, j.staged_assets_json, j.reservation_id, j.billing_unit_snapshot, j.micros_per_unit_snapshot, r.account_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start, r.status AS reservation_status, r.actual_micros FROM generation_jobs j JOIN usage_reservations r ON r.id = j.reservation_id WHERE j.id = $1"
            }
        };
        let job = sqlx::query(select)
            .bind(input.job_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
        let current_status: String = job.try_get("status")?;
        let persisted_staged_assets = job
            .try_get::<Option<String>, _>("staged_assets_json")?
            .map(|value| serde_json::from_str::<GenerationStagedAssets>(&value))
            .transpose()
            .map_err(|_| AppError::Internal)?;
        let driver: String = job.try_get("driver")?;
        let estimated_units: i64 = job.try_get("estimated_units")?;
        if input.billed_units > estimated_units {
            return Err(AppError::BadRequest(
                "generation billed units exceed the reserved estimate".into(),
            ));
        }
        if input.status == "succeeded"
            && match driver.as_str() {
                "volcengine-seedance" => {
                    input.assets.len() != 1 || !input.assets[0].mime_type.starts_with("video/")
                }
                "comfyui" => !(1..=16).contains(&input.assets.len()),
                _ => true,
            }
        {
            return Err(AppError::BadRequest(
                "generation driver returned an invalid number of archived assets".into(),
            ));
        }

        let key_id = parse_uuid(job.try_get("key_id")?)?;
        let micros_per_unit: i64 = job.try_get("micros_per_unit_snapshot")?;
        let billing_unit: String = job.try_get("billing_unit_snapshot")?;
        let reservation = UsageReservation {
            id: parse_uuid(job.try_get("reservation_id")?)?,
            account_id: parse_uuid(job.try_get("account_id")?)?,
            key_id,
            reserved_micros: job.try_get("reserved_micros")?,
            input_micros_per_million: 0,
            output_micros_per_million: if billing_unit == "megapixel" {
                micros_per_unit
            } else {
                micros_per_unit
                    .checked_mul(1_000_000)
                    .ok_or(AppError::Internal)?
            },
            price_tiers: Vec::new(),
            rate_window_start: job.try_get("rate_window_start")?,
            reserved_tokens: job.try_get("reserved_tokens")?,
        };
        let usage = TokenUsage {
            output_tokens: input.billed_units,
            ..TokenUsage::default()
        };
        let expected_cost_micros = price_token_usage(&reservation, &usage)?;
        let result = (input.status == "succeeded")
            .then(|| safe_generation_result(&driver, input.billed_units, input.assets));
        let result_json = result
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| AppError::Internal)?;

        if matches!(
            current_status.as_str(),
            "succeeded" | "failed" | "cancelled"
        ) {
            let existing_billed_units: Option<i64> = job.try_get("billed_units")?;
            let existing_cost_micros: i64 = job.try_get("cost_micros")?;
            let existing_result_json: Option<String> = job.try_get("result_json")?;
            let existing_result = existing_result_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|_| AppError::Internal)?;
            let existing_error_code: Option<String> = job.try_get("error_code")?;
            let reservation_status: String = job.try_get("reservation_status")?;
            let actual_micros: Option<i64> = job.try_get("actual_micros")?;
            let staged_replay_matches = if input.status == "succeeded" {
                persisted_staged_assets.as_ref() == input.staged_assets
            } else {
                persisted_staged_assets.is_none()
            };
            let exact_terminal = current_status == input.status
                && existing_billed_units == Some(input.billed_units)
                && existing_cost_micros == expected_cost_micros
                && existing_result == result
                && existing_error_code.as_deref() == input.error_code
                && staged_replay_matches
                && reservation_status == "settled"
                && actual_micros == Some(existing_cost_micros);
            if !exact_terminal
                || !generation_assets_match(&mut transaction, input.job_id, input.assets).await?
            {
                return Err(AppError::Conflict(
                    "generation job already has a different terminal result".into(),
                ));
            }
            transaction.commit().await?;
            return Ok(existing_cost_micros);
        }
        if !matches!(
            current_status.as_str(),
            "queued" | "running" | "submitting" | "cancelling"
        ) {
            return Err(AppError::Internal);
        }
        let lease_owner: Option<String> = job.try_get("lease_owner")?;
        if lease_owner.as_deref() != Some(input.worker_id) {
            return Err(AppError::NotFound);
        }
        if persisted_staged_assets.as_ref() != input.staged_assets {
            return Err(AppError::NotFound);
        }

        let cost_micros =
            settle_token_usage_in_transaction(&mut transaction, &reservation, &usage, now).await?;
        if cost_micros != expected_cost_micros {
            return Err(AppError::Conflict(
                "generation reservation was settled for a different amount".into(),
            ));
        }
        let expected_staged_assets_json = input
            .staged_assets
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| AppError::Internal)?;
        let updated = sqlx::query("UPDATE generation_jobs SET status = $1, billed_units = $2, cost_micros = $3, result_json = $4, error_code = $5, completed_at = $6, updated_at = $7, lease_owner = NULL, lease_expires_at = NULL, staged_assets_json = CASE WHEN $1 = 'succeeded' THEN staged_assets_json ELSE NULL END WHERE id = $8 AND lease_owner = $9 AND status IN ('queued', 'running', 'submitting', 'cancelling') AND ((staged_assets_json IS NULL AND $10 IS NULL) OR staged_assets_json = $10)")
            .bind(input.status)
            .bind(input.billed_units)
            .bind(cost_micros)
            .bind(result_json)
            .bind(input.error_code)
            .bind(now)
            .bind(now)
            .bind(input.job_id.to_string())
            .bind(input.worker_id)
            .bind(expected_staged_assets_json)
            .execute(&mut *transaction)
            .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        if input.status == "succeeded" {
            insert_generation_assets_in_transaction(
                &mut transaction,
                input.job_id,
                input.assets,
                now,
            )
            .await?;
        } else {
            sqlx::query("DELETE FROM generation_assets WHERE job_id = $1")
                .bind(input.job_id.to_string())
                .execute(&mut *transaction)
                .await?;
            if let Some(staged) = input.staged_assets {
                let key = crate::archive_staging::ArchiveStagingKey::new(
                    ArchiveStagingOwner::GenerationJob(input.job_id),
                    ArchiveStagingPurpose::Assets,
                    staged.attempt_nonce,
                )?;
                super::cleanup_archive_staging_attempt_in_transaction(&mut transaction, key)
                    .await?;
            } else {
                super::cleanup_archive_staging_purpose_in_transaction(
                    &mut transaction,
                    ArchiveStagingOwner::GenerationJob(input.job_id),
                    ArchiveStagingPurpose::Assets,
                )
                .await?;
            }
        }
        aggregate_terminal_generation_job(&mut transaction, &input.job_id.to_string(), now).await?;
        let tenant_id: String = job.try_get("tenant_id")?;
        let key_id = key_id.to_string();
        let request_id = input.job_id.to_string();
        let event_id = Uuid::now_v7().to_string();
        if claim_request_event_locator(
            &mut transaction,
            &event_id,
            now,
            &tenant_id,
            &key_id,
            &request_id,
        )
        .await?
        {
            sqlx::query(
                "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) SELECT $1, tenant_id, key_id, id, $2, 'finished', 'generation', public_model, CASE WHEN status = 'succeeded' THEN 200 WHEN status = 'cancelled' THEN 499 ELSE 502 END, $3 - created_at, 0, 0, cost_micros, error_code FROM generation_jobs WHERE id = $4",
            )
            .bind(&event_id)
            .bind(now)
            .bind(now)
            .bind(&request_id)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(cost_micros)
    }
}

fn is_allowed_generation_error_code(error_code: &str) -> bool {
    matches!(
        error_code,
        "retry_exhausted"
            | "generation_timeout"
            | "generation_rejected"
            | "submission_outcome_unknown"
            | "generation_staging_lost"
            | "generation_asset_bytes_exceeded"
            | "upstream_usage_exceeds_contract"
            | "seedance_generation_failed"
            | "seedance_missing_asset"
            | "seedance_invalid_asset"
            | "comfyui_failed"
            | "comfyui_execution_error"
            | "comfyui_missing_assets"
            | "comfyui_asset_limit_exceeded"
            | "cancelled_by_user"
    )
}

fn safe_generation_result(
    driver: &str,
    billed_units: i64,
    assets: &[ArchivedGenerationAsset],
) -> serde_json::Value {
    let provider = match driver {
        "volcengine-seedance" => {
            serde_json::json!({"status": "succeeded", "duration": billed_units})
        }
        "comfyui" => serde_json::json!({"status": "success"}),
        _ => serde_json::json!({"status": "succeeded"}),
    };
    let assets = assets
        .iter()
        .map(|asset| GenerationAssetView {
            asset_id: asset.asset_id,
            index: asset.index,
            mime_type: asset.mime_type.clone(),
            size_bytes: asset.size_bytes,
            filename: asset.filename.clone(),
        })
        .collect::<Vec<_>>();
    serde_json::json!({"provider": provider, "assets": assets})
}

async fn insert_generation_assets_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    job_id: Uuid,
    assets: &[ArchivedGenerationAsset],
    now: i64,
) -> Result<(), AppError> {
    for asset in assets {
        let inserted = sqlx::query(
            "INSERT INTO generation_assets (id, job_id, asset_index, object_locator, mime_type, size_bytes, filename, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT(job_id, asset_index) DO NOTHING",
        )
        .bind(asset.asset_id.to_string())
        .bind(job_id.to_string())
        .bind(asset.index)
        .bind(&asset.object_locator)
        .bind(&asset.mime_type)
        .bind(asset.size_bytes)
        .bind(&asset.filename)
        .bind(now)
        .execute(&mut **transaction)
        .await?;
        if inserted.rows_affected() == 0
            && !generation_asset_matches(transaction, job_id, asset).await?
        {
            return Err(AppError::Conflict(
                "generation staged asset replay does not match archived metadata".into(),
            ));
        }
    }
    Ok(())
}

async fn generation_asset_matches(
    transaction: &mut Transaction<'_, Any>,
    job_id: Uuid,
    expected: &ArchivedGenerationAsset,
) -> Result<bool, AppError> {
    let row = sqlx::query(
        "SELECT id, object_locator, mime_type, size_bytes, filename FROM generation_assets WHERE job_id = $1 AND asset_index = $2",
    )
    .bind(job_id.to_string())
    .bind(expected.index)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    Ok(
        row.try_get::<String, _>("id")? == expected.asset_id.to_string()
            && row.try_get::<String, _>("object_locator")? == expected.object_locator
            && row.try_get::<String, _>("mime_type")? == expected.mime_type
            && row.try_get::<i64, _>("size_bytes")? == expected.size_bytes
            && row.try_get::<String, _>("filename")? == expected.filename,
    )
}

async fn generation_assets_match(
    transaction: &mut Transaction<'_, Any>,
    job_id: Uuid,
    expected: &[ArchivedGenerationAsset],
) -> Result<bool, AppError> {
    let rows = sqlx::query(
        "SELECT id, asset_index, object_locator, mime_type, size_bytes, filename FROM generation_assets WHERE job_id = $1 ORDER BY asset_index, id",
    )
    .bind(job_id.to_string())
    .fetch_all(&mut **transaction)
    .await?;
    if rows.len() != expected.len() {
        return Ok(false);
    }
    let mut expected = expected.iter().collect::<Vec<_>>();
    expected.sort_by_key(|asset| (asset.index, asset.asset_id));
    for (row, expected) in rows.iter().zip(expected) {
        let id: String = row.try_get("id")?;
        let index: i64 = row.try_get("asset_index")?;
        let object_locator: String = row.try_get("object_locator")?;
        let mime_type: String = row.try_get("mime_type")?;
        let size_bytes: i64 = row.try_get("size_bytes")?;
        let filename: String = row.try_get("filename")?;
        if id != expected.asset_id.to_string()
            || index != expected.index
            || object_locator != expected.object_locator
            || mime_type != expected.mime_type
            || size_bytes != expected.size_bytes
            || filename != expected.filename
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_generation_preparation_error(error_code: &str) -> Result<(), AppError> {
    if matches!(
        error_code,
        "generation_archive_failed"
            | "generation_archive_attach_failed"
            | "generation_archive_expired"
    ) {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "invalid generation preparation error code".into(),
        ))
    }
}

fn is_content_object_location(location: &str) -> bool {
    let Some((prefix, digest)) = location.rsplit_once('/') else {
        return false;
    };
    let Some(shard) = prefix.strip_prefix("objects/blake3/") else {
        return false;
    };
    shard.len() == 2
        && digest.len() == 64
        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        && digest.starts_with(shard)
}

fn is_indeterminate_database_error(error: &sqlx::Error) -> bool {
    // A server Database error is a definitive rejected statement/transaction.
    // Transport, protocol, pool and worker failures may occur after PostgreSQL
    // or SQLite applied an autocommit statement but before its ACK arrived.
    !matches!(error, sqlx::Error::Database(_))
}

async fn fail_preparing_generation_in_transaction(
    tx: &mut Transaction<'_, Any>,
    job_id: Uuid,
    error_code: &str,
    now: i64,
) -> Result<bool, AppError> {
    validate_generation_preparation_error(error_code)?;
    let row = sqlx::query(
        "SELECT j.status, j.created_at, j.tenant_id, j.key_id, j.public_model, r.id AS reservation_id, r.account_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start, r.status AS reservation_status, r.actual_micros FROM generation_jobs j JOIN usage_reservations r ON r.id = j.reservation_id WHERE j.id = $1",
    )
    .bind(job_id.to_string())
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    if row.try_get::<String, _>("status")? != "preparing" {
        return Ok(false);
    }
    let reservation_status: String = row.try_get("reservation_status")?;
    let actual_micros: Option<i64> = row.try_get("actual_micros")?;
    let key_id = parse_uuid(row.try_get("key_id")?)?;
    let reservation = UsageReservation {
        id: parse_uuid(row.try_get("reservation_id")?)?,
        account_id: parse_uuid(row.try_get("account_id")?)?,
        key_id,
        reserved_micros: row.try_get("reserved_micros")?,
        input_micros_per_million: 0,
        output_micros_per_million: 0,
        price_tiers: Vec::new(),
        rate_window_start: row.try_get("rate_window_start")?,
        reserved_tokens: row.try_get("reserved_tokens")?,
    };
    if reservation_status == "reserved" {
        settle_token_usage_in_transaction(tx, &reservation, &TokenUsage::default(), now).await?;
    } else if actual_micros != Some(0) {
        return Err(AppError::Internal);
    }
    let failed = sqlx::query(
        "UPDATE generation_jobs SET status = 'failed', billed_units = 0, cost_micros = 0, error_code = $1, completed_at = $2, updated_at = $3, lease_owner = NULL, lease_expires_at = NULL WHERE id = $4 AND status = 'preparing'",
    )
    .bind(error_code)
    .bind(now)
    .bind(now)
    .bind(job_id.to_string())
    .execute(&mut **tx)
    .await?;
    if failed.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "generation request archive owner changed during failure settlement".into(),
        ));
    }
    aggregate_terminal_generation_job(tx, &job_id.to_string(), now).await?;
    let event_id = Uuid::now_v7().to_string();
    let tenant_id: String = row.try_get("tenant_id")?;
    let key_id = key_id.to_string();
    let request_id = job_id.to_string();
    if claim_request_event_locator(tx, &event_id, now, &tenant_id, &key_id, &request_id).await? {
        sqlx::query(
            "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) VALUES ($1, $2, $3, $4, $5, 'finished', 'generation', $6, 502, $7, 0, 0, 0, $8)",
        )
        .bind(&event_id)
        .bind(&tenant_id)
        .bind(&key_id)
        .bind(&request_id)
        .bind(now)
        .bind(row.try_get::<String, _>("public_model")?)
        .bind(now.saturating_sub(row.try_get("created_at")?))
        .bind(error_code)
        .execute(&mut **tx)
        .await?;
    }
    Ok(true)
}

fn generation_job_view(row: AnyRow) -> Result<GenerationJobView, AppError> {
    let result_json: Option<String> = row.try_get("result_json")?;
    let result = result_json
        .map(|value| {
            serde_json::from_str::<serde_json::Value>(&value).map_err(|_| AppError::Internal)
        })
        .transpose()?;
    let assets = result
        .as_ref()
        .and_then(|value| value.get("assets"))
        .cloned()
        .map(serde_json::from_value::<Vec<GenerationAssetView>>)
        .transpose()
        .map_err(|_| AppError::Internal)?
        .unwrap_or_default();
    Ok(GenerationJobView {
        job_id: parse_uuid(row.try_get("id")?)?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        completed_at: row.try_get("completed_at")?,
        model: row.try_get("public_model")?,
        driver: row.try_get("driver")?,
        billing_unit: row.try_get("billing_unit_snapshot")?,
        status: row.try_get("status")?,
        upstream_job_id: row.try_get("upstream_job_id")?,
        estimated_units: row.try_get("estimated_units")?,
        billed_units: row.try_get("billed_units")?,
        cost: micros_to_decimal_string(row.try_get("cost_micros")?),
        error_code: row.try_get("error_code")?,
        result,
        assets,
    })
}

fn generation_update_claimed(result: AnyQueryResult) -> Result<(), AppError> {
    if result.rows_affected() == 1 {
        Ok(())
    } else {
        Err(AppError::NotFound)
    }
}
