//! Exact-request read-only inventory; no table scan, mutations, or GC authority.
use super::*;

impl Database {
    /// Callers select an explicit tenant-scoped batch. SQL metadata is released
    /// before object I/O. Full-root reachability and writer fencing are separate
    /// prerequisites; this method only measures already retired source objects.
    pub async fn inventory_retired_raw_batch(
        &self,
        tenant: Uuid,
        requests: &[Uuid],
    ) -> Result<crate::archive::RawGcCandidateBatch, AppError> {
        let archive = self
            .archive
            .as_ref()
            .ok_or_else(|| AppError::storage("semantic_archive_unavailable"))?;
        let mut candidates = Vec::new();
        let mut pending = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for request in requests {
            if !seen.insert(*request) {
                continue;
            }
            let leaf: Option<String> = sqlx::query_scalar(
                "SELECT o.leaf_node_hash FROM conversation_observations o JOIN conversation_projection_outbox p ON p.request_id=o.request_id WHERE o.request_id=$1 AND p.tenant_id=$2 AND p.projected_at IS NOT NULL AND p.statistics_outcome='session_body'",
            ).bind(request.to_string()).bind(tenant.to_string())
                .fetch_optional(&self.pool).await?.flatten();
            let Some(leaf) = leaf else {
                pending.push(*request);
                continue;
            };
            let Some(authority) = archive
                .semantic_archive_authority(tenant, *request, &leaf)
                .await?
            else {
                pending.push(*request);
                continue;
            };
            if authority.coverage.projection_state
                != crate::archive::SemanticProjectionState::Complete
                || !authority.coverage.observed_terminal
                || !authority.coverage.gaps.is_empty()
            {
                pending.push(*request);
                continue;
            }
            let retired = archive
                .inventory_retired_raw(&authority.publication)
                .await?;
            if retired.is_empty() {
                pending.push(*request);
            }
            candidates.extend(retired);
        }
        crate::archive::RawGcCandidateBatch::from_candidates(candidates, pending)
    }
}
