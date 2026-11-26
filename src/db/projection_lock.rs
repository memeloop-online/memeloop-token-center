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

/// Exclusively fences online projection writers for any aggregate replacement.
///
/// Take this at the owning transaction entry, before source/fact/session/budget locks.
/// Conversation identity changes and merges can rebuild even one session; shared mode
/// cannot protect their DELETE + INSERT snapshot from concurrent incremental writers.
/// Never upgrade an already shared transaction after it has acquired other locks.
/// This global gate pauses all projection writers until the replacement commits.
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

/// Background replacement writers yield instead of occupying a connection
/// while an online writer owns the same authoritative projection gate.
pub(crate) async fn try_lock_request_stats_projection_rebuild_in_transaction(
    transaction: &mut Transaction<'_, Any>,
) -> Result<bool, AppError> {
    if transaction.as_mut().backend_name() == "PostgreSQL" {
        return Ok(sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended('memeloop-token-center:request-stats', 734627102948314))")
            .fetch_one(&mut **transaction).await?);
    }
    Ok(true)
}
