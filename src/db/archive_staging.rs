//! Durable archive-staging state machine.
//!
//! PostgreSQL workers claim rows with `FOR UPDATE SKIP LOCKED`. SQLite uses WAL
//! plus `BEGIN IMMEDIATE` and remains a lightweight test/development backend.

use sqlx::{Any, Row, Transaction, any::AnyRow};
use uuid::Uuid;

use crate::archive_staging::{
    ARCHIVE_STAGING_CLAIM_BATCH, ARCHIVE_STAGING_CLEANUP_LEASE_MILLIS,
    ARCHIVE_STAGING_EMPTY_STABILITY_MILLIS, ARCHIVE_STAGING_STALE_DELETE_GRACE_MILLIS,
    ARCHIVE_STAGING_WRITE_LEASE_MILLIS, ArchiveStagingAttempt, ArchiveStagingCleanupErrorCode,
    ArchiveStagingCleanupLease, ArchiveStagingEmptyResult, ArchiveStagingIntentDigest,
    ArchiveStagingKey, ArchiveStagingLeaseOwner, ArchiveStagingOwner, ArchiveStagingReferenceProof,
    ArchiveStagingState, ArchiveStagingUnreferencedLease, ArchiveStagingWriteLease,
    BeginArchiveStagingInput, BeginArchiveStagingResult, cleanup_backoff_with_jitter,
    locator_matches_prefix, validate_bound_locator,
};
use crate::model::GenerationStagedAssets;

use super::{AppError, Database, DatabaseBackend};

impl Database {
    /// Creates a writing attempt or returns the exact idempotent replay.
    ///
    /// `lease_token` must be persisted by the caller before retrying. Reusing an
    /// attempt UUID with a different token or semantic digest is a conflict.
    pub async fn begin_archive_staging_attempt(
        &self,
        input: BeginArchiveStagingInput,
    ) -> Result<BeginArchiveStagingResult, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let now = archive_database_now(&mut transaction, self.backend).await?;
        let expires_at = now
            .checked_add(ARCHIVE_STAGING_WRITE_LEASE_MILLIS)
            .ok_or(AppError::Internal)?;
        let inserted = sqlx::query(
            "INSERT INTO archive_staging_attempts (attempt_id, owner_kind, owner_id, purpose, intent_digest, state, writer_owner, writer_token, lease_owner, lease_token, lease_expires_at, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, 'writing', $6, $7, $8, $9, $10, $11, $12) ON CONFLICT(attempt_id) DO NOTHING",
        )
        .bind(input.key.attempt_id.to_string())
        .bind(input.key.owner.kind())
        .bind(input.key.owner.id().to_string())
        .bind(input.key.purpose.as_str())
        .bind(input.intent_digest.as_str())
        .bind(input.lease_owner.as_str())
        .bind(input.lease_token.to_string())
        .bind(input.lease_owner.as_str())
        .bind(input.lease_token.to_string())
        .bind(expires_at)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        let lease = ArchiveStagingWriteLease {
            key: input.key,
            owner: input.lease_owner.clone(),
            token: input.lease_token,
            expires_at,
        };
        if inserted.rows_affected() == 1 {
            transaction.commit().await?;
            return Ok(BeginArchiveStagingResult::Created(lease));
        }

        let row = lock_attempt(&mut transaction, self.backend, input.key.attempt_id)
            .await?
            .ok_or(AppError::Internal)?;
        let stored = archive_attempt_from_row(&row)?;
        let stored_writer_owner: String = row.try_get("writer_owner")?;
        let stored_writer_token: String = row.try_get("writer_token")?;
        let stored_lease_expires_at: Option<i64> = row.try_get("lease_expires_at")?;
        let requested_token = input.lease_token.to_string();
        let exact_identity = stored.key == input.key
            && stored.intent_digest == input.intent_digest
            && stored_writer_owner == input.lease_owner.as_str()
            && stored_writer_token == requested_token;
        if !exact_identity {
            transaction.rollback().await?;
            return Err(AppError::Conflict(
                "archive staging attempt identity already exists".into(),
            ));
        }
        if stored.state == ArchiveStagingState::Writing
            && stored_lease_expires_at.is_some_and(|expiry| expiry > now)
        {
            let replay = ArchiveStagingWriteLease {
                expires_at: stored_lease_expires_at.ok_or(AppError::Internal)?,
                ..lease
            };
            transaction.commit().await?;
            return Ok(BeginArchiveStagingResult::Replayed(replay));
        }
        transaction.commit().await?;
        Ok(BeginArchiveStagingResult::Existing(stored))
    }

    /// Extends a live writing lease. Expiry is a hard fence and is never revived.
    pub async fn heartbeat_archive_staging_write(
        &self,
        lease: &mut ArchiveStagingWriteLease,
    ) -> Result<bool, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let now = archive_database_now(&mut transaction, self.backend).await?;
        let expires_at = now
            .checked_add(ARCHIVE_STAGING_WRITE_LEASE_MILLIS)
            .ok_or(AppError::Internal)?;
        let updated = sqlx::query(
            "UPDATE archive_staging_attempts SET lease_expires_at = $1, updated_at = $2 WHERE attempt_id = $3 AND state = 'writing' AND lease_owner = $4 AND lease_token = $5 AND lease_expires_at > $6",
        )
        .bind(expires_at)
        .bind(now)
        .bind(lease.key.attempt_id.to_string())
        .bind(lease.owner.as_str())
        .bind(lease.token.to_string())
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        if updated.rows_affected() == 1 {
            lease.expires_at = expires_at;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Atomically marks the prefix as referenced. Lifecycle code which commits
    /// an application locator should call the transaction variant below in the
    /// same transaction as that locator write.
    pub async fn bind_archive_staging_attempt(
        &self,
        lease: &ArchiveStagingWriteLease,
        locator: &str,
    ) -> Result<bool, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let bound = bind_archive_staging_attempt_in_transaction(
            &mut transaction,
            self.backend,
            lease,
            locator,
        )
        .await?;
        transaction.commit().await?;
        Ok(bound)
    }

    /// Releases a previously bound prefix after the owning locator/manifest is
    /// removed. Lifecycle code must use the transaction variant so the owner
    /// reference disappears atomically with this transition.
    pub async fn release_bound_archive_staging_attempt(
        &self,
        key: ArchiveStagingKey,
        locator: &str,
    ) -> Result<bool, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let released = release_bound_archive_staging_attempt_in_transaction(
            &mut transaction,
            self.backend,
            key,
            locator,
        )
        .await?;
        transaction.commit().await?;
        Ok(released)
    }

    /// Relinquishes a live writer and schedules cleanup. An expired writer must
    /// be promoted by the stale-attempt reaper instead of reviving its token.
    pub async fn abandon_archive_staging_attempt(
        &self,
        lease: &ArchiveStagingWriteLease,
    ) -> Result<bool, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let now = archive_database_now(&mut transaction, self.backend).await?;
        let updated = sqlx::query(
            "UPDATE archive_staging_attempts SET state = 'cleanup_pending', lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, next_cleanup_at = $1, updated_at = $2 WHERE attempt_id = $3 AND state = 'writing' AND lease_owner = $4 AND lease_token = $5 AND lease_expires_at > $6",
        )
        .bind(now)
        .bind(now)
        .bind(lease.key.attempt_id.to_string())
        .bind(lease.owner.as_str())
        .bind(lease.token.to_string())
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Promotes at most a bounded batch of expired writers. PostgreSQL locks
    /// candidates with `SKIP LOCKED`; SQLite serializes competing writers when
    /// the immediate transaction begins.
    pub async fn promote_stale_archive_staging_attempts(&self) -> Result<u64, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let now = archive_database_now(&mut transaction, self.backend).await?;
        let next_cleanup_at = now
            .checked_add(ARCHIVE_STAGING_STALE_DELETE_GRACE_MILLIS)
            .ok_or(AppError::Internal)?;
        let query = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT attempt_id FROM archive_staging_attempts WHERE state = 'writing' AND lease_expires_at <= $1 ORDER BY lease_expires_at, attempt_id FOR UPDATE SKIP LOCKED LIMIT $2"
            }
            DatabaseBackend::Sqlite => {
                "SELECT attempt_id FROM archive_staging_attempts WHERE state = 'writing' AND lease_expires_at <= $1 ORDER BY lease_expires_at, attempt_id LIMIT $2"
            }
        };
        let candidates: Vec<String> = sqlx::query_scalar(query)
            .bind(now)
            .bind(ARCHIVE_STAGING_CLAIM_BATCH)
            .fetch_all(&mut *transaction)
            .await?;
        let mut promoted = 0_u64;
        for attempt_id in candidates {
            promoted = promoted.saturating_add(
                sqlx::query(
                    "UPDATE archive_staging_attempts SET state = 'cleanup_pending', lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, next_cleanup_at = $1, updated_at = $2 WHERE attempt_id = $3 AND state = 'writing' AND lease_expires_at <= $4",
                )
                .bind(next_cleanup_at)
                .bind(now)
                .bind(attempt_id)
                .bind(now)
                .execute(&mut *transaction)
                .await?
                .rows_affected(),
            );
        }
        transaction.commit().await?;
        Ok(promoted)
    }

    /// Claims one cleanup candidate. Different PostgreSQL replicas receive
    /// disjoint rows; an expired cleanup token is fenced before it can mutate.
    pub async fn claim_archive_staging_cleanup(
        &self,
        owner: ArchiveStagingLeaseOwner,
    ) -> Result<Option<ArchiveStagingCleanupLease>, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let now = archive_database_now(&mut transaction, self.backend).await?;
        let query = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT * FROM archive_staging_attempts WHERE state = 'cleanup_pending' AND next_cleanup_at <= $1 AND (lease_expires_at IS NULL OR lease_expires_at <= $2) ORDER BY next_cleanup_at, attempt_id FOR UPDATE SKIP LOCKED LIMIT 1"
            }
            DatabaseBackend::Sqlite => {
                "SELECT * FROM archive_staging_attempts WHERE state = 'cleanup_pending' AND next_cleanup_at <= $1 AND (lease_expires_at IS NULL OR lease_expires_at <= $2) ORDER BY next_cleanup_at, attempt_id LIMIT 1"
            }
        };
        let Some(row) = sqlx::query(query)
            .bind(now)
            .bind(now)
            .fetch_optional(&mut *transaction)
            .await?
        else {
            transaction.commit().await?;
            return Ok(None);
        };
        let token = Uuid::now_v7();
        let expires_at = now
            .checked_add(ARCHIVE_STAGING_CLEANUP_LEASE_MILLIS)
            .ok_or(AppError::Internal)?;
        let attempt_id: String = row.try_get("attempt_id")?;
        let updated = sqlx::query(
            "UPDATE archive_staging_attempts SET lease_owner = $1, lease_token = $2, lease_expires_at = $3, updated_at = $4 WHERE attempt_id = $5 AND state = 'cleanup_pending' AND (lease_expires_at IS NULL OR lease_expires_at <= $6)",
        )
        .bind(owner.as_str())
        .bind(token.to_string())
        .bind(expires_at)
        .bind(now)
        .bind(&attempt_id)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(AppError::Internal);
        }
        let mut attempt = archive_attempt_from_row(&row)?;
        attempt.updated_at = now;
        transaction.commit().await?;
        Ok(Some(ArchiveStagingCleanupLease {
            attempt,
            owner,
            token,
            expires_at,
        }))
    }

    pub async fn heartbeat_archive_staging_cleanup(
        &self,
        lease: &mut ArchiveStagingCleanupLease,
    ) -> Result<bool, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let now = archive_database_now(&mut transaction, self.backend).await?;
        let expires_at = now
            .checked_add(ARCHIVE_STAGING_CLEANUP_LEASE_MILLIS)
            .ok_or(AppError::Internal)?;
        let updated = sqlx::query(
            "UPDATE archive_staging_attempts SET lease_expires_at = $1, updated_at = $2 WHERE attempt_id = $3 AND state = 'cleanup_pending' AND lease_owner = $4 AND lease_token = $5 AND lease_expires_at > $6",
        )
        .bind(expires_at)
        .bind(now)
        .bind(lease.attempt.key.attempt_id.to_string())
        .bind(lease.owner.as_str())
        .bind(lease.token.to_string())
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        if updated.rows_affected() == 1 {
            lease.expires_at = expires_at;
            lease.attempt.updated_at = now;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Proves that no normalized application locator references this exact
    /// prefix (or a descendant segment). A discovered reference keeps the row
    /// cleanup-pending with a fixed error and backoff; it is never guessed into
    /// `bound`, and the worker must not touch object storage.
    pub async fn prove_archive_staging_unreferenced(
        &self,
        lease: ArchiveStagingCleanupLease,
    ) -> Result<ArchiveStagingReferenceProof, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let now = archive_database_now(&mut transaction, self.backend).await?;
        let locked =
            lock_and_verify_cleanup_lease(&mut transaction, self.backend, &lease, now).await?;
        let prefix = lease.canonical_prefix();
        let referenced =
            archive_staging_reference(&mut transaction, lease.attempt.key, &prefix).await?;
        if let Some(locator) = referenced {
            let old_failures: i64 = locked.try_get("cleanup_failures")?;
            let failures = old_failures.saturating_add(1).min(63);
            let failure_count = u32::try_from(failures).map_err(|_| AppError::Internal)?;
            let next_cleanup_at = now
                .checked_add(cleanup_backoff_with_jitter(
                    lease.attempt.key.attempt_id,
                    failure_count,
                ))
                .ok_or(AppError::Internal)?;
            let protected = sqlx::query(
                "UPDATE archive_staging_attempts SET lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, empty_observed_at = NULL, cleanup_failures = $1, next_cleanup_at = $2, last_error_code = 'reference_present', updated_at = $3 WHERE attempt_id = $4 AND state = 'cleanup_pending' AND lease_owner = $5 AND lease_token = $6 AND lease_expires_at > $7",
            )
            .bind(failures)
            .bind(next_cleanup_at)
            .bind(now)
            .bind(lease.attempt.key.attempt_id.to_string())
            .bind(lease.owner.as_str())
            .bind(lease.token.to_string())
            .bind(now)
            .execute(&mut *transaction)
            .await?;
            if protected.rows_affected() != 1 {
                transaction.rollback().await?;
                return Err(AppError::Conflict(
                    "archive staging cleanup lease was fenced".into(),
                ));
            }
            transaction.commit().await?;
            return Ok(ArchiveStagingReferenceProof::Protected { locator });
        }
        transaction.commit().await?;
        Ok(ArchiveStagingReferenceProof::Unreferenced(
            ArchiveStagingUnreferencedLease {
                lease,
                proved_at: now,
            },
        ))
    }

    /// Records an empty listing. The first observation releases the lease and
    /// schedules a stable-window retry; only a separately claimed and freshly
    /// proven second observation can move the attempt to `cleaned`.
    pub async fn record_archive_staging_empty(
        &self,
        proof: ArchiveStagingUnreferencedLease,
    ) -> Result<ArchiveStagingEmptyResult, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let now = archive_database_now(&mut transaction, self.backend).await?;
        let row = lock_and_verify_cleanup_lease(&mut transaction, self.backend, &proof.lease, now)
            .await?;
        if proof.proved_at > now || proof.proved_at < proof.lease.attempt.updated_at {
            transaction.rollback().await?;
            return Err(AppError::Conflict(
                "archive staging reference proof is stale".into(),
            ));
        }
        let first_observed_at: Option<i64> = row.try_get("empty_observed_at")?;
        if let Some(first_observed_at) = first_observed_at {
            let confirm_after = first_observed_at
                .checked_add(ARCHIVE_STAGING_EMPTY_STABILITY_MILLIS)
                .ok_or(AppError::Internal)?;
            if now < confirm_after {
                transaction.rollback().await?;
                return Err(AppError::Conflict(
                    "archive staging empty confirmation is not stable yet".into(),
                ));
            }
            let cleaned = sqlx::query(
                "UPDATE archive_staging_attempts SET state = 'cleaned', lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, next_cleanup_at = NULL, last_error_code = NULL, cleaned_at = $1, updated_at = $2 WHERE attempt_id = $3 AND state = 'cleanup_pending' AND lease_owner = $4 AND lease_token = $5 AND lease_expires_at > $6 AND empty_observed_at = $7",
            )
            .bind(now)
            .bind(now)
            .bind(proof.lease.attempt.key.attempt_id.to_string())
            .bind(proof.lease.owner.as_str())
            .bind(proof.lease.token.to_string())
            .bind(now)
            .bind(first_observed_at)
            .execute(&mut *transaction)
            .await?;
            if cleaned.rows_affected() != 1 {
                transaction.rollback().await?;
                return Err(AppError::Conflict(
                    "archive staging cleanup lease was fenced".into(),
                ));
            }
            transaction.commit().await?;
            return Ok(ArchiveStagingEmptyResult::Cleaned);
        }

        let confirm_after = now
            .checked_add(ARCHIVE_STAGING_EMPTY_STABILITY_MILLIS)
            .ok_or(AppError::Internal)?;
        let observed = sqlx::query(
            "UPDATE archive_staging_attempts SET lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, empty_observed_at = $1, next_cleanup_at = $2, last_error_code = NULL, updated_at = $3 WHERE attempt_id = $4 AND state = 'cleanup_pending' AND lease_owner = $5 AND lease_token = $6 AND lease_expires_at > $7 AND empty_observed_at IS NULL",
        )
        .bind(now)
        .bind(confirm_after)
        .bind(now)
        .bind(proof.lease.attempt.key.attempt_id.to_string())
        .bind(proof.lease.owner.as_str())
        .bind(proof.lease.token.to_string())
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if observed.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(AppError::Conflict(
                "archive staging cleanup lease was fenced".into(),
            ));
        }
        transaction.commit().await?;
        Ok(ArchiveStagingEmptyResult::FirstObservation { confirm_after })
    }

    /// Releases a failed cleanup with a fixed low-cardinality code and bounded
    /// exponential backoff. Any earlier empty observation is invalidated.
    pub async fn record_archive_staging_cleanup_failure(
        &self,
        lease: &ArchiveStagingCleanupLease,
        code: ArchiveStagingCleanupErrorCode,
    ) -> Result<i64, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let now = archive_database_now(&mut transaction, self.backend).await?;
        let row = lock_and_verify_cleanup_lease(&mut transaction, self.backend, lease, now).await?;
        let old_failures: i64 = row.try_get("cleanup_failures")?;
        let failures = old_failures.saturating_add(1).min(63);
        let failure_count = u32::try_from(failures).map_err(|_| AppError::Internal)?;
        let next_cleanup_at = now
            .checked_add(cleanup_backoff_with_jitter(
                lease.attempt.key.attempt_id,
                failure_count,
            ))
            .ok_or(AppError::Internal)?;
        let updated = sqlx::query(
            "UPDATE archive_staging_attempts SET lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, empty_observed_at = NULL, cleanup_failures = $1, next_cleanup_at = $2, last_error_code = $3, updated_at = $4 WHERE attempt_id = $5 AND state = 'cleanup_pending' AND lease_owner = $6 AND lease_token = $7 AND lease_expires_at > $8",
        )
        .bind(failures)
        .bind(next_cleanup_at)
        .bind(code.as_str())
        .bind(now)
        .bind(lease.attempt.key.attempt_id.to_string())
        .bind(lease.owner.as_str())
        .bind(lease.token.to_string())
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(AppError::Conflict(
                "archive staging cleanup lease was fenced".into(),
            ));
        }
        transaction.commit().await?;
        Ok(next_cleanup_at)
    }

    pub async fn archive_staging_attempt(
        &self,
        attempt_id: Uuid,
    ) -> Result<Option<ArchiveStagingAttempt>, AppError> {
        let row = sqlx::query("SELECT * FROM archive_staging_attempts WHERE attempt_id = $1")
            .bind(attempt_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| archive_attempt_from_row(&row)).transpose()
    }

    pub(crate) async fn archive_staging_readiness_check(&self) -> Result<(), sqlx::Error> {
        let migration: Option<i64> =
            sqlx::query_scalar("SELECT version FROM schema_migrations WHERE version = 35")
                .fetch_optional(&self.pool)
                .await?;
        if migration != Some(35) {
            return Err(sqlx::Error::Protocol(
                "archive staging schema migration v35 is not applied".into(),
            ));
        }
        sqlx::query(
            "SELECT attempt_id, owner_kind, purpose, state, lease_token, next_cleanup_at FROM archive_staging_attempts LIMIT 0",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(())
    }
}

pub(super) async fn bind_archive_staging_attempt_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    lease: &ArchiveStagingWriteLease,
    locator: &str,
) -> Result<bool, AppError> {
    validate_bound_locator(lease.key, locator)?;
    let now = archive_database_now(transaction, backend).await?;
    let updated = sqlx::query(
        "UPDATE archive_staging_attempts SET state = 'bound', lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, bound_locator = $1, bound_at = $2, updated_at = $3 WHERE attempt_id = $4 AND state = 'writing' AND lease_owner = $5 AND lease_token = $6 AND lease_expires_at > $7",
    )
    .bind(locator)
    .bind(now)
    .bind(now)
    .bind(lease.key.attempt_id.to_string())
    .bind(lease.owner.as_str())
    .bind(lease.token.to_string())
    .bind(now)
    .execute(&mut **transaction)
    .await?;
    if updated.rows_affected() == 1 {
        return Ok(true);
    }
    let existing = lock_attempt(transaction, backend, lease.key.attempt_id).await?;
    let Some(existing) = existing else {
        return Ok(false);
    };
    Ok(archive_staging_key_from_row(&existing)? == lease.key
        && existing.try_get::<String, _>("state")? == "bound"
        && existing
            .try_get::<Option<String>, _>("bound_locator")?
            .as_deref()
            == Some(locator))
}

pub(super) async fn release_bound_archive_staging_attempt_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    key: ArchiveStagingKey,
    locator: &str,
) -> Result<bool, AppError> {
    validate_bound_locator(key, locator)?;
    let now = archive_database_now(transaction, backend).await?;
    let updated = sqlx::query(
        "UPDATE archive_staging_attempts SET state = 'cleanup_pending', bound_locator = NULL, next_cleanup_at = $1, empty_observed_at = NULL, last_error_code = NULL, updated_at = $2 WHERE attempt_id = $3 AND state = 'bound' AND bound_locator = $4",
    )
    .bind(now)
    .bind(now)
    .bind(key.attempt_id.to_string())
    .bind(locator)
    .execute(&mut **transaction)
    .await?;
    if updated.rows_affected() == 1 {
        return Ok(true);
    }
    let existing = lock_attempt(transaction, backend, key.attempt_id).await?;
    let Some(existing) = existing else {
        return Ok(false);
    };
    let state: String = existing.try_get("state")?;
    Ok(archive_staging_key_from_row(&existing)? == key
        && matches!(state.as_str(), "cleanup_pending" | "cleaned")
        && existing.try_get::<Option<i64>, _>("bound_at")?.is_some())
}

async fn archive_staging_reference(
    transaction: &mut Transaction<'_, Any>,
    key: ArchiveStagingKey,
    prefix: &str,
) -> Result<Option<String>, AppError> {
    let owner_id = key.owner.id().to_string();
    let segment_prefix = format!("{prefix}/");

    match key.owner {
        ArchiveStagingOwner::ProxyRequest(_) | ArchiveStagingOwner::SynchronousRequest(_) => {
            if let Some(row) = sqlx::query(
                "SELECT request_object, response_object FROM request_records WHERE id = $1 LIMIT 1",
            )
            .bind(&owner_id)
            .fetch_optional(&mut **transaction)
            .await?
            {
                let request: String = row.try_get("request_object")?;
                if locator_matches_prefix(&request, prefix) {
                    return Ok(Some(request));
                }
                let response: Option<String> = row.try_get("response_object")?;
                if let Some(response) = response
                    && locator_matches_prefix(&response, prefix)
                {
                    return Ok(Some(response));
                }
            }
        }
        ArchiveStagingOwner::GenerationJob(_) => {
            let row = sqlx::query(
                "SELECT request_object, staged_assets_json FROM generation_jobs WHERE id = $1",
            )
            .bind(&owner_id)
            .fetch_optional(&mut **transaction)
            .await?;
            if let Some(row) = row {
                let request: String = row.try_get("request_object")?;
                if locator_matches_prefix(&request, prefix) {
                    return Ok(Some(request));
                }
                if let Some(manifest) = row.try_get::<Option<String>, _>("staged_assets_json")? {
                    let manifest: GenerationStagedAssets =
                        serde_json::from_str(&manifest).map_err(|_| AppError::Internal)?;
                    if manifest.attempt_nonce == key.attempt_id {
                        return Ok(Some(prefix.to_owned()));
                    }
                    if let Some(asset) = manifest
                        .assets
                        .into_iter()
                        .find(|asset| locator_matches_prefix(&asset.object_locator, prefix))
                    {
                        return Ok(Some(asset.object_locator));
                    }
                }
            }
        }
    }

    let asset_reference_query = match key.owner {
        ArchiveStagingOwner::ProxyRequest(_) | ArchiveStagingOwner::SynchronousRequest(_) => {
            "SELECT object_locator FROM generation_assets WHERE request_id = $1 AND (object_locator = $2 OR SUBSTR(object_locator, 1, LENGTH($2) + 1) = $3) LIMIT 1"
        }
        ArchiveStagingOwner::GenerationJob(_) => {
            "SELECT object_locator FROM generation_assets WHERE job_id = $1 AND (object_locator = $2 OR SUBSTR(object_locator, 1, LENGTH($2) + 1) = $3) LIMIT 1"
        }
    };
    if let Some(locator) = sqlx::query_scalar(asset_reference_query)
        .bind(&owner_id)
        .bind(prefix)
        .bind(&segment_prefix)
        .fetch_optional(&mut **transaction)
        .await?
    {
        return Ok(Some(locator));
    }

    if matches!(key.owner, ArchiveStagingOwner::SynchronousRequest(_)) {
        let response: Option<String> = sqlx::query_scalar(
            "SELECT response_object FROM synchronous_image_idempotency WHERE request_id = $1 AND response_object IS NOT NULL AND (response_object = $2 OR SUBSTR(response_object, 1, LENGTH($2) + 1) = $3) LIMIT 1",
        )
        .bind(owner_id)
        .bind(prefix)
        .bind(segment_prefix)
        .fetch_optional(&mut **transaction)
        .await?;
        if let Some(response) = response {
            return Ok(Some(response));
        }
    }

    Ok(None)
}

async fn lock_attempt(
    transaction: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    attempt_id: Uuid,
) -> Result<Option<AnyRow>, AppError> {
    let query = match backend {
        DatabaseBackend::PostgreSql => {
            "SELECT * FROM archive_staging_attempts WHERE attempt_id = $1 FOR UPDATE"
        }
        DatabaseBackend::Sqlite => "SELECT * FROM archive_staging_attempts WHERE attempt_id = $1",
    };
    Ok(sqlx::query(query)
        .bind(attempt_id.to_string())
        .fetch_optional(&mut **transaction)
        .await?)
}

async fn lock_and_verify_cleanup_lease(
    transaction: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    lease: &ArchiveStagingCleanupLease,
    now: i64,
) -> Result<AnyRow, AppError> {
    let row = lock_attempt(transaction, backend, lease.attempt.key.attempt_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let state: String = row.try_get("state")?;
    let owner: Option<String> = row.try_get("lease_owner")?;
    let token: Option<String> = row.try_get("lease_token")?;
    let expires_at: Option<i64> = row.try_get("lease_expires_at")?;
    if state != "cleanup_pending"
        || owner.as_deref() != Some(lease.owner.as_str())
        || token.as_deref() != Some(lease.token.to_string().as_str())
        || expires_at.is_none_or(|expiry| expiry <= now)
    {
        return Err(AppError::Conflict(
            "archive staging cleanup lease was fenced".into(),
        ));
    }
    let stored_key = archive_staging_key_from_row(&row)?;
    if stored_key != lease.attempt.key {
        return Err(AppError::Internal);
    }
    Ok(row)
}

fn archive_attempt_from_row(row: &AnyRow) -> Result<ArchiveStagingAttempt, AppError> {
    let cleanup_failures: i64 = row.try_get("cleanup_failures")?;
    Ok(ArchiveStagingAttempt {
        key: archive_staging_key_from_row(row)?,
        intent_digest: ArchiveStagingIntentDigest::new(row.try_get::<String, _>("intent_digest")?)
            .map_err(|_| AppError::Internal)?,
        state: ArchiveStagingState::parse(&row.try_get::<String, _>("state")?)?,
        bound_locator: row.try_get("bound_locator")?,
        bound_at: row.try_get("bound_at")?,
        empty_observed_at: row.try_get("empty_observed_at")?,
        cleanup_failures: u32::try_from(cleanup_failures).map_err(|_| AppError::Internal)?,
        next_cleanup_at: row.try_get("next_cleanup_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        cleaned_at: row.try_get("cleaned_at")?,
    })
}

fn archive_staging_key_from_row(row: &AnyRow) -> Result<ArchiveStagingKey, AppError> {
    ArchiveStagingKey::from_database(
        &row.try_get::<String, _>("owner_kind")?,
        &row.try_get::<String, _>("owner_id")?,
        &row.try_get::<String, _>("purpose")?,
        &row.try_get::<String, _>("attempt_id")?,
    )
}

async fn archive_database_now(
    transaction: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
) -> Result<i64, AppError> {
    let query = match backend {
        DatabaseBackend::PostgreSql => {
            "SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT)"
        }
        DatabaseBackend::Sqlite => {
            "SELECT CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)"
        }
    };
    let now: i64 = sqlx::query_scalar(query)
        .fetch_one(&mut **transaction)
        .await?;
    if now < 0 {
        return Err(AppError::Internal);
    }
    Ok(now)
}
