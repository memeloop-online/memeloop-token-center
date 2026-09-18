use sqlx::{Any, Transaction};

use crate::error::AppError;

async fn lock_projection_source_table_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    statement: &'static str,
) -> Result<(), AppError> {
    if transaction.as_mut().backend_name() == "PostgreSQL" {
        sqlx::query(statement).execute(&mut **transaction).await?;
    }
    Ok(())
}

pub(crate) async fn lock_request_records_projection_source_in_transaction(
    transaction: &mut Transaction<'_, Any>,
) -> Result<(), AppError> {
    lock_projection_source_table_in_transaction(
        transaction,
        "LOCK TABLE request_records IN ROW EXCLUSIVE MODE",
    )
    .await
}

pub(crate) async fn lock_generation_jobs_projection_source_in_transaction(
    transaction: &mut Transaction<'_, Any>,
) -> Result<(), AppError> {
    lock_projection_source_table_in_transaction(
        transaction,
        "LOCK TABLE generation_jobs IN ROW EXCLUSIVE MODE",
    )
    .await
}

const REQUEST_STATS_PROJECTION_WRITER_LOCK_SQL: &str = "SELECT pg_advisory_xact_lock_shared(hashtextextended('memeloop-token-center:request-stats', 734627102948314))";
const REQUEST_STATS_PROJECTION_REBUILD_LOCK_SQL: &str = "SELECT pg_advisory_xact_lock(hashtextextended('memeloop-token-center:request-stats', 734627102948314))";

/// Joins the online observability-writer cohort on PostgreSQL.
///
/// Every live writer takes this shared lock before session, conversation, key-budget, settlement,
/// source-table, fact, or aggregate locks. Shared mode keeps independent requests concurrent while
/// excluding fact backfills and full projection rebuilds for their complete source snapshot and
/// replacement transaction. SQLite already serializes these writers with `BEGIN IMMEDIATE`.
pub(crate) async fn lock_request_stats_projection_writer_in_transaction(
    transaction: &mut Transaction<'_, Any>,
) -> Result<(), AppError> {
    if transaction.as_mut().backend_name() == "PostgreSQL" {
        // Keep this identity aligned with scripts/maintenance/reconcile-observability-day.sql.
        sqlx::query(REQUEST_STATS_PROJECTION_WRITER_LOCK_SQL)
            .execute(&mut **transaction)
            .await?;
    }
    Ok(())
}

/// Exclusively fences online projection writers for a fact backfill or full rebuild.
pub(crate) async fn lock_request_stats_projection_rebuild_in_transaction(
    transaction: &mut Transaction<'_, Any>,
) -> Result<(), AppError> {
    if transaction.as_mut().backend_name() == "PostgreSQL" {
        sqlx::query(REQUEST_STATS_PROJECTION_REBUILD_LOCK_SQL)
            .execute(&mut **transaction)
            .await?;
    }
    Ok(())
}
