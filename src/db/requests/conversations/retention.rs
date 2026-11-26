//! Outbox retention must not discard the last archive recovery obligation.
use super::*;

impl Database {
    pub(super) async fn delete_durably_retired_conversation_outbox(
        &self,
        limit: i64,
        cutoff: i64,
    ) -> Result<u64, AppError> {
        if limit <= 0 {
            return Ok(0);
        }
        let rows = sqlx::query("SELECT p.request_id,p.tenant_id,p.key_id,p.projected_at,p.semantic_snapshot_json,p.semantic_payload_snapshot_json,o.leaf_node_hash FROM conversation_projection_outbox p JOIN conversation_observations o ON o.request_id=p.request_id JOIN conversation_clusters c ON c.id=o.cluster_id AND c.tenant_id=p.tenant_id AND c.key_id=p.key_id WHERE p.projected_at IS NOT NULL AND p.projected_at<$1 AND p.statistics_outcome='session_body' AND p.lease_owner IS NULL AND p.lease_expires_at IS NULL AND p.terminal_lease_owner IS NULL AND p.terminal_lease_expires_at IS NULL AND o.leaf_node_hash IS NOT NULL ORDER BY p.projected_at,p.request_id LIMIT $2")
            .bind(cutoff).bind(limit).fetch_all(&self.pool).await?;
        let archive = self.body_archive()?;
        let mut deleted = 0;
        // No SQL transaction or connection survives these object reads.
        for row in rows {
            let request: String = row.try_get("request_id")?;
            let tenant: String = row.try_get("tenant_id")?;
            let key: String = row.try_get("key_id")?;
            let leaf: String = row.try_get("leaf_node_hash")?;
            let projected: i64 = row.try_get("projected_at")?;
            let snapshot: Option<String> = row.try_get("semantic_snapshot_json")?;
            let payload: Option<String> = row.try_get("semantic_payload_snapshot_json")?;
            let tenant_id = parse_uuid(tenant.clone())?;
            let request_id = parse_uuid(request.clone())?;
            let authority = match archive
                .semantic_archive_authority(tenant_id, request_id, &leaf)
                .await
            {
                Ok(Some(authority)) => authority,
                _ => continue,
            };
            let mut retired = true;
            for side in ["request", "response"] {
                if !matches!(
                    archive
                        .raw_reader_authority(
                            tenant_id,
                            request_id,
                            side,
                            None,
                            None,
                            Some(&authority)
                        )
                        .await,
                    Ok(crate::archive::RawReaderAuthority::Canonical(_))
                ) {
                    retired = false;
                    break;
                }
            }
            if !retired {
                continue;
            }
            // Revalidate the exact completed generation and both lease families
            // after I/O. NULL alone never supplied the completion proof above.
            deleted += sqlx::query("DELETE FROM conversation_projection_outbox WHERE request_id=$1 AND tenant_id=$2 AND key_id=$3 AND projected_at=$4 AND projected_at<$5 AND statistics_outcome='session_body' AND lease_owner IS NULL AND lease_expires_at IS NULL AND terminal_lease_owner IS NULL AND terminal_lease_expires_at IS NULL AND (semantic_snapshot_json=$6 OR (semantic_snapshot_json IS NULL AND $6 IS NULL)) AND (semantic_payload_snapshot_json=$7 OR (semantic_payload_snapshot_json IS NULL AND $7 IS NULL)) AND EXISTS(SELECT 1 FROM conversation_observations o JOIN conversation_clusters c ON c.id=o.cluster_id WHERE o.request_id=$1 AND o.leaf_node_hash=$8 AND c.tenant_id=$2 AND c.key_id=$3)")
                .bind(&request).bind(&tenant).bind(&key).bind(projected).bind(cutoff)
                .bind(snapshot).bind(payload).bind(&leaf).execute(&self.pool).await?.rows_affected();
        }
        Ok(deleted)
    }
}
