mod aggregation;
mod jobs;
mod synchronous;

use sqlx::{Any, Transaction};

use crate::{
    archive_staging::{ArchiveStagingKey, ArchiveStagingOwner, ArchiveStagingPurpose},
    error::AppError,
};

use super::unix_millis;
pub(super) use aggregation::aggregate_terminal_generation_job;

pub use jobs::{
    AttachGenerationJobResult, CreateGenerationJobInput, CreateGenerationJobResult,
    FinishGenerationJobInput, StartGenerationJobInput,
};
pub use synchronous::{
    AttachSynchronousImageRequestObject, FinishSynchronousImageRequest,
    FinishSynchronousImageResult, GenerationJobIdempotency, StartSynchronousImageRequest,
    StartSynchronousImageResult, SynchronousImageIdempotencyClaim,
};

pub(super) async fn cleanup_archive_staging_attempt_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    key: ArchiveStagingKey,
) -> Result<bool, AppError> {
    let now = unix_millis();
    let updated = sqlx::query(
        "UPDATE archive_staging_attempts SET state = 'cleanup_pending', bound_locator = NULL, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, next_cleanup_at = $1, empty_observed_at = NULL, last_error_code = NULL, updated_at = $2 WHERE attempt_id = $3 AND owner_kind = $4 AND owner_id = $5 AND purpose = $6 AND state IN ('writing', 'bound')",
    )
    .bind(now)
    .bind(now)
    .bind(key.attempt_id.to_string())
    .bind(key.owner.kind())
    .bind(key.owner.id().to_string())
    .bind(key.purpose.as_str())
    .execute(&mut **transaction)
    .await?;
    Ok(updated.rows_affected() == 1)
}

pub(super) async fn cleanup_archive_staging_purpose_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    owner: ArchiveStagingOwner,
    purpose: ArchiveStagingPurpose,
) -> Result<u64, AppError> {
    let now = unix_millis();
    let updated = sqlx::query(
        "UPDATE archive_staging_attempts SET state = 'cleanup_pending', bound_locator = NULL, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, next_cleanup_at = $1, empty_observed_at = NULL, last_error_code = NULL, updated_at = $2 WHERE owner_kind = $3 AND owner_id = $4 AND purpose = $5 AND state IN ('writing', 'bound')",
    )
    .bind(now)
    .bind(now)
    .bind(owner.kind())
    .bind(owner.id().to_string())
    .bind(purpose.as_str())
    .execute(&mut **transaction)
    .await?;
    Ok(updated.rows_affected())
}

pub(super) async fn cleanup_writing_archive_staging_purpose_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    owner: ArchiveStagingOwner,
    purpose: ArchiveStagingPurpose,
) -> Result<u64, AppError> {
    let now = unix_millis();
    let updated = sqlx::query(
        "UPDATE archive_staging_attempts SET state = 'cleanup_pending', lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, next_cleanup_at = $1, empty_observed_at = NULL, last_error_code = NULL, updated_at = $2 WHERE owner_kind = $3 AND owner_id = $4 AND purpose = $5 AND state = 'writing'",
    )
    .bind(now)
    .bind(now)
    .bind(owner.kind())
    .bind(owner.id().to_string())
    .bind(purpose.as_str())
    .execute(&mut **transaction)
    .await?;
    Ok(updated.rows_affected())
}
