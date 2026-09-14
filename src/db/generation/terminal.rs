use sqlx::{Any, Transaction};

use crate::error::AppError;

/// Publishes accounting evidence and statistics for every generation terminal
/// transition, including cancellation and preparation failure. Both effects
/// share the caller's transaction and retain their identities on replay.
pub(super) async fn publish_generation_terminal_effects(
    transaction: &mut Transaction<'_, Any>,
    job_id: &str,
    now: i64,
) -> Result<(), AppError> {
    super::super::billing::publish_generation_settlement_in_transaction(
        transaction,
        super::super::parse_uuid(job_id.to_owned())?,
    )
    .await?;
    super::aggregation::aggregate_terminal_generation_job(transaction, job_id, now).await
}
