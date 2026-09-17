use sqlx::{Any, Transaction};

use crate::error::AppError;

/// Serializes fact snapshots with every online observability projection writer on PostgreSQL.
///
/// Online writers acquire this after source, settlement, or outbox ownership locks and before their
/// first fact or aggregate mutation. Backfills acquire it before locking candidate facts, while
/// source-pruning maintenance locks its source tables before taking this lock. Since the backfill
/// does not acquire source, settlement, account, outbox, or conversation locks, this one-way order
/// cannot form a lock cycle. SQLite already serializes these writers with `BEGIN IMMEDIATE`.
pub(crate) async fn lock_request_stats_projection_in_transaction(
    transaction: &mut Transaction<'_, Any>,
) -> Result<(), AppError> {
    if transaction.as_mut().backend_name() == "PostgreSQL" {
        // Keep this identity aligned with scripts/maintenance/reconcile-observability-day.sql.
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('memeloop-token-center:request-stats', 734627102948314))",
        )
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}
