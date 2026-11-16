//! SQL acknowledgement of semantic content is distinct from financial ACK and
//! from object upload. No locator, receipt digest, claim or balance is changed.
use super::*;

#[cfg(test)]
tokio::task_local! {
    static COVERAGE_FAULT: std::cell::Cell<Option<&'static str>>;
}

impl Database {
    #[cfg(test)]
    pub(crate) async fn with_semantic_coverage_fault<T>(
        stage: &'static str,
        future: impl std::future::Future<Output = T>,
    ) -> T {
        COVERAGE_FAULT
            .scope(std::cell::Cell::new(Some(stage)), future)
            .await
    }

    #[cfg(test)]
    pub(crate) fn semantic_coverage_test_boundary(stage: &'static str) -> Result<(), AppError> {
        if COVERAGE_FAULT
            .try_with(|fault| {
                if fault.get() == Some(stage) {
                    fault.set(None);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false)
        {
            return Err(AppError::storage("injected_semantic_coverage_boundary"));
        }
        Ok(())
    }

    pub(crate) async fn retain_semantic_coverage_obligation(
        &self,
        owner: Uuid,
        request: Uuid,
        publication: &crate::archive::SemanticPublication,
    ) -> Result<bool, AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let row = sqlx::query("SELECT semantic_snapshot_json FROM conversation_projection_outbox WHERE request_id=$1 AND projected_at IS NULL AND COALESCE(terminal_lease_owner,lease_owner)=$2")
            .bind(request.to_string()).bind(owner.to_string()).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let old: String = row.try_get("semantic_snapshot_json")?;
        let mut value: serde_json::Value = serde_json::from_str(&old)
            .map_err(|_| AppError::storage("semantic_snapshot_invalid"))?;
        value["semantic_coverage_pending"] = serde_json::to_value(publication)
            .map_err(|_| AppError::storage("semantic_publication_invalid"))?;
        let updated = sqlx::query("UPDATE conversation_projection_outbox SET semantic_snapshot_json=$1 WHERE request_id=$2 AND projected_at IS NULL AND COALESCE(terminal_lease_owner,lease_owner)=$3 AND semantic_snapshot_json=$4")
            .bind(value.to_string()).bind(request.to_string()).bind(owner.to_string()).bind(old).execute(&mut *tx).await?.rows_affected()==1;
        if updated {
            tx.commit().await?;
        } else {
            tx.rollback().await?;
        }
        Ok(updated)
    }

    pub(crate) async fn recover_semantic_coverage(&self) -> Result<(), AppError> {
        let rows = sqlx::query("SELECT request_id,tenant_id,semantic_snapshot_json FROM conversation_projection_outbox WHERE projected_at IS NOT NULL AND lease_owner='semantic-coverage-v2' ORDER BY observed_at,request_id LIMIT 4")
            .fetch_all(&self.pool).await?;
        let archive = self
            .archive
            .as_ref()
            .ok_or_else(|| AppError::storage("semantic_archive_unavailable"))?;
        for row in rows {
            let request: String = row.try_get("request_id")?;
            let tenant: String = row.try_get("tenant_id")?;
            let old: String = row.try_get("semantic_snapshot_json")?;
            let value: serde_json::Value = serde_json::from_str(&old)
                .map_err(|_| AppError::storage("semantic_snapshot_invalid"))?;
            let publication: crate::archive::SemanticPublication =
                serde_json::from_value(value["semantic_coverage_pending"].clone())
                    .map_err(|_| AppError::storage("semantic_publication_invalid"))?;
            if publication.tenant_id != parse_uuid(tenant.clone())?
                || publication.request_id != parse_uuid(request.clone())?
            {
                return Err(AppError::storage("semantic_publication_owner_mismatch"));
            }
            let manifest = archive
                .get_semantic_manifest_streamed(publication.tenant_id, &publication.manifest)
                .await?;
            let Some(head) = manifest.semantic_head else {
                continue;
            };
            let proof = self
                .semantic_archive_binding(
                    parse_uuid(tenant)?,
                    parse_uuid(request.clone())?,
                    &head.node_hash,
                )
                .await?
                .ok_or_else(|| AppError::storage("semantic_binding_missing"))?;
            archive
                .confirm_semantic_coverage(&publication, &proof)
                .await?;
            let next = if manifest.raw_sources.is_some() {
                Some("semantic-raw-retire-v1")
            } else {
                None
            };
            sqlx::query("UPDATE conversation_projection_outbox SET lease_owner=$1 WHERE request_id=$2 AND projected_at IS NOT NULL AND lease_owner='semantic-coverage-v2' AND semantic_snapshot_json=$3")
                .bind(next).bind(request).bind(old).execute(&self.pool).await?;
        }
        Ok(())
    }

    /// Resume retirement independently from projection and finance. The
    /// outbox marker is cleared only after every exact root is a verified
    /// reference-free marker; failures and commit-unknown retain retry duty.
    pub(crate) async fn recover_semantic_raw_retirement(&self) -> Result<(), AppError> {
        let archive = self.body_archive()?;
        let rows = sqlx::query("SELECT request_id,tenant_id,semantic_snapshot_json FROM conversation_projection_outbox WHERE projected_at IS NOT NULL AND lease_owner='semantic-raw-retire-v1' ORDER BY COALESCE(lease_expires_at,0),observed_at,request_id LIMIT 4")
            .fetch_all(&self.pool).await?;
        for row in rows {
            let request: String = row.try_get("request_id")?;
            let tenant: String = row.try_get("tenant_id")?;
            let old: String = row.try_get("semantic_snapshot_json")?;
            let request_id = parse_uuid(request.clone())?;
            let tenant_id = parse_uuid(tenant)?;
            let pending: serde_json::Value = serde_json::from_str(&old)
                .map_err(|_| AppError::storage("semantic_snapshot_invalid"))?;
            let publication: crate::archive::SemanticPublication =
                serde_json::from_value(pending["semantic_coverage_pending"].clone())
                    .map_err(|_| AppError::storage("semantic_publication_invalid"))?;
            if publication.request_id != request_id || publication.tenant_id != tenant_id {
                return Err(AppError::storage("semantic_publication_owner_mismatch"));
            }
            // Reuse the original outbox retry ordering field, so a protected
            // or failing oldest row cannot starve all later retirements.
            sqlx::query("UPDATE conversation_projection_outbox SET lease_expires_at=$1 WHERE request_id=$2 AND projected_at IS NOT NULL AND lease_owner='semantic-raw-retire-v1' AND semantic_snapshot_json=$3")
                .bind(unix_millis()).bind(&request).bind(&old).execute(&self.pool).await?;
            // These short metadata reads end before object-store I/O. A late
            // publisher is fenced separately, and CAS bytes remain readable.
            let busy: i64 = sqlx::query_scalar("SELECT CAST(CASE WHEN EXISTS(SELECT 1 FROM request_archive_spools WHERE request_id=$1 AND (state IN ('capturing','pending','uploading') OR lease_owner IS NOT NULL)) OR EXISTS(SELECT 1 FROM response_archive_spools WHERE request_id=$1 AND (state IN ('capturing','pending','uploading') OR lease_owner IS NOT NULL)) OR EXISTS(SELECT 1 FROM archive_staging_attempts WHERE owner_id=$1 AND state='writing') THEN 1 ELSE 0 END AS BIGINT)")
                .bind(&request).fetch_one(&self.pool).await?;
            if busy != 0 || archive.has_carrier_owner_for_request(request_id).await? {
                continue;
            }
            let manifest = archive
                .get_semantic_manifest_streamed(tenant_id, &publication.manifest)
                .await?;
            let Some(head) = manifest.semantic_head else {
                continue;
            };
            let proof = self
                .semantic_archive_binding(tenant_id, request_id, &head.node_hash)
                .await?
                .ok_or_else(|| AppError::storage("semantic_binding_missing"))?;
            archive
                .retire_covered_raw_roots(&publication, &proof)
                .await?;
            sqlx::query("UPDATE conversation_projection_outbox SET lease_owner=NULL,lease_expires_at=NULL WHERE request_id=$1 AND projected_at IS NOT NULL AND lease_owner='semantic-raw-retire-v1' AND semantic_snapshot_json=$2")
                .bind(request).bind(old).execute(&self.pool).await?;
        }
        Ok(())
    }

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
