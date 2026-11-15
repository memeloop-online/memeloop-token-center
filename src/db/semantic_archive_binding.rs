//! SQL acknowledgement of semantic content is distinct from financial ACK and
//! from object upload. No locator, receipt digest, claim or balance is changed.
use super::*;

impl Database {
    pub(crate) async fn published_semantic_revision(
        &self,
        scope: &SessionContextScope,
    ) -> Result<Option<u64>, AppError> {
        let leaf: Option<String> = sqlx::query_scalar("SELECT leaf_node_hash FROM conversation_observations WHERE tenant_id = $1 AND request_id = $2 AND key_id = $3")
            .bind(scope.tenant_id.to_string()).bind(scope.request_id.to_string()).bind(scope.key_id.to_string())
            .fetch_optional(&self.pool).await?.flatten();
        let Some(leaf) = leaf else {
            return Ok(None);
        };
        let archive = self
            .archive
            .as_ref()
            .ok_or_else(|| AppError::storage("semantic_archive_unavailable"))?;
        // The SQL connection is returned before any object-store operation.
        archive
            .published_semantic_revision(scope.tenant_id, scope.request_id, &leaf)
            .await
    }

    pub(crate) async fn semantic_archive_binding(
        &self,
        tenant: Uuid,
        request: Uuid,
        expected_node: &str,
    ) -> Result<Option<crate::archive::SemanticBindingProof>, AppError> {
        let row = sqlx::query(
            "SELECT p.projected_at, o.leaf_node_hash FROM conversation_projection_outbox p
             JOIN conversation_observations o ON o.request_id = p.request_id
             WHERE p.request_id = $1 AND p.tenant_id = $2 AND p.projected_at IS NOT NULL
               AND p.statistics_outcome = 'session_body' AND o.leaf_node_hash = $3",
        )
        .bind(request.to_string())
        .bind(tenant.to_string())
        .bind(expected_node)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(crate::archive::SemanticBindingProof {
                tenant_id: tenant,
                request_id: request,
                node_hash: row.try_get("leaf_node_hash")?,
                projected_at: row.try_get("projected_at")?,
            })
        })
        .transpose()
    }
}
