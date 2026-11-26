//! Retention scans metadata first; missing proof never discards recovery work.
use super::*;

type Position = (i64, String);
type Scan = (Option<Position>, Option<Position>);
const PAGE: i64 = 32;
const RECEIPT_BYTES: i64 = 8192;

#[cfg(test)]
tokio::task_local! {
    static BEFORE_FENCE: std::cell::RefCell<Option<std::sync::Arc<(tokio::sync::Notify, tokio::sync::Notify)>>>;
}

#[cfg(test)]
pub(super) async fn with_retention_fence_gate<T>(
    gate: std::sync::Arc<(tokio::sync::Notify, tokio::sync::Notify)>,
    work: impl std::future::Future<Output = T>,
) -> T {
    BEFORE_FENCE
        .scope(std::cell::RefCell::new(Some(gate)), work)
        .await
}

impl Database {
    pub(crate) async fn delete_projected_conversation_outbox_page(
        &self,
        limit: i64,
        scan: &mut Scan,
    ) -> Result<u64, AppError> {
        let cutoff = unix_millis().saturating_sub(CONVERSATION_PROJECTION_RETENTION_MS);
        self.delete_retired_outbox_page(limit, cutoff, scan).await
    }

    pub(super) async fn delete_durably_retired_conversation_outbox(
        &self,
        limit: i64,
        cutoff: i64,
    ) -> Result<u64, AppError> {
        self.delete_retired_outbox_page(limit, cutoff, &mut (None, None))
            .await
    }

    pub(super) async fn delete_retired_outbox_page(
        &self,
        limit: i64,
        cutoff: i64,
        scan: &mut Scan,
    ) -> Result<u64, AppError> {
        let Ok(archive) = self.body_archive() else {
            return Ok(0);
        };
        if limit <= 0 {
            return Ok(0);
        }
        if scan.1.is_none() {
            scan.1 = sqlx::query_as("SELECT projected_at,request_id FROM conversation_projection_outbox WHERE projected_at<$1 ORDER BY projected_at DESC,request_id DESC LIMIT 1")
                .bind(cutoff).fetch_optional(&self.pool).await?;
        }
        let Some((upper_time, upper_id)) = scan.1.clone() else {
            return Ok(0);
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut deleted = 0;
        let mut visited = 0;
        let mut missing = 0_u64;
        let mut unavailable = 0_u64;
        while visited < limit && std::time::Instant::now() < deadline {
            let (after_time, after_id) = scan.0.clone().unwrap_or((i64::MIN, String::new()));
            // Never materialize snapshot TOAST in the page query.
            let rows = sqlx::query("SELECT request_id,tenant_id,key_id,projected_at FROM conversation_projection_outbox WHERE projected_at<$1 AND (projected_at>$2 OR (projected_at=$2 AND request_id>$3)) AND (projected_at<$4 OR (projected_at=$4 AND request_id<=$5)) ORDER BY projected_at,request_id LIMIT $6")
                .bind(cutoff).bind(after_time).bind(after_id).bind(upper_time).bind(&upper_id).bind(PAGE.min(limit-visited)).fetch_all(&self.pool).await?;
            if rows.is_empty() {
                *scan = (None, None);
                break;
            }
            for row in rows {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                let request: String = row.try_get("request_id")?;
                let tenant: String = row.try_get("tenant_id")?;
                let key: String = row.try_get("key_id")?;
                let projected: i64 = row.try_get("projected_at")?;
                scan.0 = Some((projected, request.clone()));
                visited += 1;
                // Completed receipts are small. Oversized legacy snapshots stay
                // protected, and are never fetched in full to test eligibility.
                let candidate=sqlx::query("SELECT SUBSTR(COALESCE(p.semantic_snapshot_json,p.semantic_payload_snapshot_json),1,$4) AS receipt,o.leaf_node_hash FROM conversation_projection_outbox p JOIN conversation_observations o ON o.request_id=p.request_id JOIN conversation_clusters c ON c.id=o.cluster_id AND c.tenant_id=p.tenant_id AND c.key_id=p.key_id WHERE p.request_id=$1 AND p.tenant_id=$2 AND p.key_id=$3 AND p.statistics_outcome='session_body' AND p.lease_owner IS NULL AND p.lease_expires_at IS NULL AND p.terminal_lease_owner IS NULL AND p.terminal_lease_expires_at IS NULL AND LENGTH(COALESCE(p.semantic_snapshot_json,p.semantic_payload_snapshot_json))<=$4 AND o.leaf_node_hash IS NOT NULL")
                    .bind(&request).bind(&tenant).bind(&key).bind(RECEIPT_BYTES).fetch_optional(&self.pool).await?;
                let Some(candidate) = candidate else {
                    missing += 1;
                    continue;
                };
                let receipt: String = candidate.try_get("receipt")?;
                let leaf: String = candidate.try_get("leaf_node_hash")?;
                let publication = serde_json::from_str::<serde_json::Value>(&receipt)
                    .ok()
                    .and_then(|v| {
                        serde_json::from_value::<crate::archive::SemanticPublication>(
                            v.get("semantic_coverage_pending")?.clone(),
                        )
                        .ok()
                    });
                let Some(publication) = publication else {
                    missing += 1;
                    continue;
                };
                let tenant_id = parse_uuid(tenant.clone())?;
                let request_id = parse_uuid(request.clone())?;
                if publication.tenant_id != tenant_id || publication.request_id != request_id {
                    missing += 1;
                    continue;
                }
                let authority = match archive
                    .semantic_archive_authority(tenant_id, request_id, &leaf)
                    .await
                {
                    Ok(Some(a)) => a,
                    Ok(None) => {
                        missing += 1;
                        continue;
                    }
                    Err(_) => {
                        unavailable += 1;
                        continue;
                    }
                };
                if serde_json::to_vec(&publication).ok()
                    != serde_json::to_vec(&authority.publication).ok()
                {
                    missing += 1;
                    continue;
                }
                let manifest = match archive
                    .get_semantic_manifest_streamed(tenant_id, &publication.manifest)
                    .await
                {
                    Ok(m) => m,
                    Err(_) => {
                        unavailable += 1;
                        continue;
                    }
                };
                let Some(sources) = manifest.raw_sources else {
                    missing += 1;
                    continue;
                };
                let Some(response) = sources.response else {
                    missing += 1;
                    continue;
                };
                let mut covered = true;
                for (side, source) in [("request", &sources.request), ("response", &response)] {
                    match archive
                        .raw_reader_authority(
                            tenant_id,
                            request_id,
                            side,
                            Some(&source.object_locator),
                            Some(source),
                            Some(&authority),
                        )
                        .await
                    {
                        Ok(crate::archive::RawReaderAuthority::Canonical(_)) => {}
                        Ok(_) => {
                            missing += 1;
                            covered = false;
                            break;
                        }
                        Err(_) => {
                            unavailable += 1;
                            covered = false;
                            break;
                        }
                    }
                }
                if !covered {
                    continue;
                }
                #[cfg(test)]
                if let Some(gate) = BEFORE_FENCE
                    .try_with(|slot| slot.borrow_mut().take())
                    .ok()
                    .flatten()
                {
                    gate.0.notify_one();
                    gate.1.notified().await;
                }
                // The existing outbox row is the publisher/claim fence. Its
                // generation is revalidated under the same row lock used by
                // projection. A later enqueue creates its own pending snapshot
                // and lease through the original INSERT, never an old ACK.
                let mut tx = self.begin_write_transaction().await?;
                let lock = if matches!(self.backend, DatabaseBackend::PostgreSql) {
                    " FOR UPDATE SKIP LOCKED"
                } else {
                    ""
                };
                let locked=sqlx::query(sqlx::AssertSqlSafe(format!("SELECT request_id FROM conversation_projection_outbox WHERE request_id=$1 AND tenant_id=$2 AND key_id=$3 AND projected_at=$4 AND statistics_outcome='session_body' AND lease_owner IS NULL AND lease_expires_at IS NULL AND terminal_lease_owner IS NULL AND terminal_lease_expires_at IS NULL AND COALESCE(semantic_snapshot_json,semantic_payload_snapshot_json)=$5{lock}")))
                    .bind(&request).bind(&tenant).bind(&key).bind(projected).bind(&receipt).fetch_optional(&mut *tx).await?;
                if locked.is_none() {
                    tx.rollback().await?;
                    continue;
                }
                // Raw producers have independent durable ownership; until that
                // ownership is terminal it is not safe to drop this bridge.
                let busy:i64=sqlx::query_scalar("SELECT CAST(CASE WHEN EXISTS(SELECT 1 FROM request_archive_spools WHERE request_id=$1 AND (state IN ('capturing','pending','uploading') OR lease_owner IS NOT NULL)) OR EXISTS(SELECT 1 FROM response_archive_spools WHERE request_id=$1 AND (state IN ('capturing','pending','uploading') OR lease_owner IS NOT NULL)) OR EXISTS(SELECT 1 FROM archive_staging_attempts WHERE owner_id=$1 AND state='writing') THEN 1 ELSE 0 END AS BIGINT)")
                    .bind(&request).fetch_one(&mut *tx).await?;
                if busy != 0 {
                    tx.rollback().await?;
                    continue;
                }
                deleted+=sqlx::query("DELETE FROM conversation_projection_outbox WHERE request_id=$1 AND tenant_id=$2 AND key_id=$3 AND projected_at=$4 AND COALESCE(semantic_snapshot_json,semantic_payload_snapshot_json)=$5 AND EXISTS(SELECT 1 FROM conversation_observations o JOIN conversation_clusters c ON c.id=o.cluster_id WHERE o.request_id=$1 AND o.leaf_node_hash=$6 AND c.tenant_id=$2 AND c.key_id=$3)")
                    .bind(&request).bind(&tenant).bind(&key).bind(projected).bind(&receipt).bind(&leaf).execute(&mut *tx).await?.rows_affected();
                tx.commit().await?;
            }
        }
        tracing::debug!(
            visited,
            deleted,
            missing,
            unavailable,
            phase = "conversation_outbox_retention",
            "conversation archive retention page finished"
        );
        Ok(deleted)
    }
}
