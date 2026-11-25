//! Resolve spool candidates through the request locator before planning body SQL.
use super::*;

impl Database {
    pub(super) async fn partitioned_archive_spool_candidate(
        &self,
        tx: &mut Transaction<'_, Any>,
        purpose: BufferedArchivePurpose,
        now: i64,
    ) -> Result<Option<AnyRow>, AppError> {
        let mut after: Option<(i64, String)> = None;
        loop {
            // This page touches only the non-partitioned recovery journal.
            // Its cursor is traversal batching, not a limit on eligible work.
            let cursor = if after.is_some() {
                "AND (CASE WHEN s.state='pending' THEN s.next_attempt_at ELSE s.lease_expires_at END, s.request_id) > ($2, $3)"
            } else {
                ""
            };
            let page_sql = format!(
                "SELECT s.request_id, s.tenant_id, s.reservation_id, CASE WHEN s.state='pending' THEN s.next_attempt_at ELSE s.lease_expires_at END AS claim_order FROM response_archive_spools s WHERE s.cleaned_at IS NULL AND s.expires_at>$1 AND ((s.state='pending' AND s.next_attempt_at<=$1) OR (s.state='uploading' AND s.lease_expires_at<=$1)) {cursor} ORDER BY claim_order, s.request_id LIMIT 64"
            );
            let mut query =
                sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, &page_sql))).bind(now);
            if let Some((order, request_id)) = &after {
                query = query.bind(*order).bind(request_id);
            }
            let candidates = query.fetch_all(&mut **tx).await?;
            if candidates.is_empty() {
                return Ok(None);
            }
            for candidate in candidates {
                let request_id: String = candidate.try_get("request_id")?;
                after = Some((candidate.try_get("claim_order")?, request_id));
                let identity = identity_from_row(&candidate)?;
                let Some(locator) = self
                    .authoritative_request_locator_in_transaction(
                        tx,
                        identity.request_id,
                        identity.tenant_id,
                    )
                    .await?
                else {
                    // A missing authority never permits a parent-wide lookup.
                    continue;
                };
                let created_at = locator.created_at_predicate(self.backend, "r.created_at", "$6");
                // The typed catalog timestamp is the sole SQL literal. All
                // identities and eligibility values remain bound. Disabling
                // caching preserves plan-time pruning for this exact request.
                let sql = format!(
                    "SELECT s.*, CAST(CASE WHEN r.completed_at IS NOT NULL AND r.status_code BETWEEN 200 AND 399 AND COALESCE(r.error_code,'')='' THEN 1 ELSE 0 END AS BIGINT) AS successful_terminal FROM response_archive_spools s JOIN request_records r ON {created_at} AND r.id=s.request_id AND r.tenant_id=s.tenant_id AND r.reservation_id=s.reservation_id AND r.key_id=$5 WHERE s.request_id=$1 AND s.tenant_id=$2 AND s.reservation_id=$3 AND s.cleaned_at IS NULL AND s.expires_at>$4 AND ((s.state='pending' AND s.next_attempt_at<=$4) OR (s.state='uploading' AND s.lease_expires_at<=$4)) AND (r.response_object='gap://' || s.request_id || '/response' OR r.response_object IS NULL OR r.response_object='') FOR UPDATE OF s SKIP LOCKED"
                );
                let row = sqlx::query(sqlx::AssertSqlSafe(spool_sql(purpose, &sql)))
                    .persistent(false)
                    .bind(identity.request_id.to_string())
                    .bind(identity.tenant_id.to_string())
                    .bind(identity.reservation_id.to_string())
                    .bind(now)
                    .bind(locator.key_id.to_string())
                    .fetch_optional(&mut **tx)
                    .await?;
                if row.is_some() {
                    return Ok(row);
                }
            }
        }
    }
}
