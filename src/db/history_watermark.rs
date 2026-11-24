//! One cutover proof shared by verification and switching. Source capture and
//! immutable ACK remain the only producer and delivery authority.
use super::*;
use crate::request_history::{HistoryError, READ_MODEL_VERSION};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct HistoryWatermarkProof {
    evidence_kind: String,
    read_model_version: i64,
    activation_epoch: i64,
    baseline_owner: String,
    baseline_epoch: i64,
    baseline_ceiling: i64,
    baseline_manifest: String,
    watermark: i64,
    target_identity: crate::request_history::HistoryTargetIdentity,
}

impl Database {
    pub(crate) async fn history_check_export_target(
        &self,
        client: &crate::request_history::ClickHouse,
    ) -> Result<(), HistoryError> {
        if !matches!(self.backend, DatabaseBackend::PostgreSql) {
            return Ok(());
        }
        let bound:Option<String>=sqlx::query_scalar("SELECT r.target_identity FROM request_history_baseline_runs r JOIN request_history_activation a ON a.id=1 JOIN request_history_delivery_control d ON d.id=1 WHERE r.target_identity IS NOT NULL AND ((r.owner=d.owner AND r.epoch=d.epoch AND r.status='active') OR (r.owner=a.baseline_owner AND r.epoch=a.baseline_epoch AND r.status='source_complete')) ORDER BY CASE WHEN r.owner=d.owner AND r.epoch=d.epoch THEN 0 ELSE 1 END LIMIT 1").fetch_optional(&self.pool).await?.flatten();
        if let Some(bound) = bound {
            let expected: crate::request_history::HistoryTargetIdentity =
                serde_json::from_str(&bound)?;
            if expected != client.target_identity().await? {
                return Err(HistoryError::Mismatch);
            }
        }
        Ok(())
    }
    pub(crate) fn history_uses_watermark_proof(&self) -> bool {
        matches!(self.backend, DatabaseBackend::PostgreSql)
    }

    // An unsuccessful proof is NotReady (Changed), never a newly accepted W.
    pub(crate) async fn history_verify_watermark(
        &self,
        client: &crate::request_history::ClickHouse,
    ) -> Result<Value, HistoryError> {
        let target = client.target_identity().await?;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut tx = self.begin_write_transaction().await?;
            let prior: Option<String> = sqlx::query_scalar(
                "SELECT verification_json FROM request_history_state WHERE id=1 FOR UPDATE NOWAIT",
            )
            .fetch_one(&mut *tx)
            .await?;
            let prior = prior
                .as_deref()
                .and_then(|value| serde_json::from_str::<HistoryWatermarkProof>(value).ok());
            let (proof, ready) = self
                .history_watermark_proof_in_transaction(&mut tx, prior.as_ref(), &target)
                .await?;
            let mut report = serde_json::to_value(&proof)?;
            report["cutover_ready"] = Value::Bool(ready);
            sqlx::query(
            "UPDATE request_history_state SET verified_revision=$1,verification_json=$2 WHERE id=1",
        )
        .bind(if ready { Some(proof.watermark) } else { None })
        .bind(serde_json::to_string(&report)?)
        .execute(&mut *tx)
        .await?;
            tx.commit().await?;
            Ok(report)
        })
        .await
        .map_err(|_| HistoryError::Changed)?
    }

    pub(crate) async fn history_switch_watermark(
        &self,
        client: &crate::request_history::ClickHouse,
    ) -> Result<(), HistoryError> {
        let target = client.target_identity().await?;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut tx = self.begin_write_transaction().await?;
            let saved: Option<String> = sqlx::query_scalar(
                "SELECT verification_json FROM request_history_state WHERE id=1 FOR UPDATE NOWAIT",
            )
            .fetch_one(&mut *tx)
            .await?;
            let proof: HistoryWatermarkProof =
                serde_json::from_str(saved.as_deref().ok_or(HistoryError::Changed)?)?;
            let verified: Option<i64> = sqlx::query_scalar(
                "SELECT verified_revision FROM request_history_state WHERE id=1",
            )
            .fetch_one(&mut *tx)
            .await?;
            let (_, ready) = self
                .history_watermark_proof_in_transaction(&mut tx, Some(&proof), &target)
                .await?;
            if !ready || verified != Some(proof.watermark) {
                return Err(HistoryError::Changed);
            }
            sqlx::query("UPDATE request_history_state SET mode='clickhouse' WHERE id=1")
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok(())
        })
        .await
        .map_err(|_| HistoryError::Changed)?
    }

    async fn history_watermark_proof_in_transaction(
        &self,
        tx: &mut Transaction<'_, Any>,
        saved: Option<&HistoryWatermarkProof>,
        target: &crate::request_history::HistoryTargetIdentity,
    ) -> Result<(HistoryWatermarkProof, bool), HistoryError> {
        if !matches!(self.backend, DatabaseBackend::PostgreSql) {
            return Err(HistoryError::Invalid(
                "versioned baseline watermarks require PostgreSQL",
            ));
        }
        // Bound the entire held-lock statement budget, not only acquisition.
        sqlx::query("SET LOCAL statement_timeout='3s'")
            .execute(&mut **tx)
            .await?;
        // Every capture producer/backfill holds this row FOR SHARE before
        // allocating a revision. NOWAIT does not queue behind a request writer.
        let mode: String = sqlx::query_scalar(
            "SELECT mode FROM request_history_state WHERE id=1 FOR UPDATE NOWAIT",
        )
        .fetch_one(&mut **tx)
        .await?;
        if mode != "shadow" {
            return Err(HistoryError::Changed);
        }
        let fenced:bool=sqlx::query_scalar("SELECT owner IS NOT NULL OR ceiling IS NOT NULL FROM request_history_delivery_control WHERE id=1 FOR SHARE NOWAIT").fetch_one(&mut **tx).await?;
        if fenced {
            return Err(HistoryError::Changed);
        }
        let unfinished: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM request_history_backfill WHERE done=0)",
        )
        .fetch_one(&mut **tx)
        .await?;
        if unfinished {
            return Err(HistoryError::Changed);
        }
        let baseline=sqlx::query("SELECT a.epoch AS activation_epoch,a.baseline_owner,a.baseline_epoch,r.ceiling,r.read_model_version,r.activation_epoch AS source_activation_epoch,r.chunk_count,r.manifest_hash,r.sources_complete,r.parity_json,r.target_identity FROM request_history_activation a JOIN request_history_baseline_runs r ON r.owner=a.baseline_owner AND r.epoch=a.baseline_epoch WHERE a.id=1 AND a.phase IN ('drained','legacy') AND r.status='source_complete' FOR SHARE OF a,r NOWAIT").fetch_optional(&mut **tx).await?.ok_or(HistoryError::Changed)?;
        let bound_target: Option<String> = baseline.try_get("target_identity")?;
        if bound_target
            .as_deref()
            .map(serde_json::from_str::<crate::request_history::HistoryTargetIdentity>)
            .transpose()?
            .as_ref()
            != Some(target)
        {
            return Err(HistoryError::Mismatch);
        }
        let version: Option<i64> = baseline.try_get("read_model_version")?;
        let activation: i64 = baseline.try_get("activation_epoch")?;
        let source_activation: Option<i64> = baseline.try_get("source_activation_epoch")?;
        let chunks: Option<i64> = baseline.try_get("chunk_count")?;
        let manifest: Option<String> = baseline.try_get("manifest_hash")?;
        let sources: Option<String> = baseline.try_get("sources_complete")?;
        let parity: Option<String> = baseline.try_get("parity_json")?;
        if version != Some(READ_MODEL_VERSION)
            || source_activation != Some(activation)
            || chunks.is_none_or(|n| n < 0)
            || manifest.as_ref().is_none_or(|h| h.len() != 64)
            || sources.as_deref() != Some(r#"["native","generation","archive","event"]"#)
        {
            return Err(HistoryError::Changed);
        }
        // This must be issued by the consistent source baseline's real surface
        // comparisons, not inferred from a list of identity readback hashes.
        let receipts = sqlx::query("SELECT COUNT(*) AS count,COALESCE(MIN(ordinal),0) AS first,COALESCE(MAX(ordinal),-1) AS last FROM request_history_baseline_chunks WHERE owner=$1 AND epoch=$2")
            .bind(baseline.try_get::<String,_>("baseline_owner")?).bind(baseline.try_get::<i64,_>("baseline_epoch")?).fetch_one(&mut **tx).await?;
        let expected = chunks.unwrap();
        if receipts.try_get::<i64, _>("count")? != expected
            || (expected > 0
                && (receipts.try_get::<i64, _>("first")? != 0
                    || receipts.try_get::<i64, _>("last")? != expected - 1))
        {
            return Err(HistoryError::Changed);
        }
        let parity: Value = serde_json::from_str(parity.as_deref().ok_or(HistoryError::Changed)?)?;
        let owner: String = baseline.try_get("baseline_owner")?;
        let epoch: i64 = baseline.try_get("baseline_epoch")?;
        let ceiling: i64 = baseline.try_get("ceiling")?;
        if parity["read_model_version"].as_i64() != version
            || parity["activation_epoch"].as_i64() != Some(activation)
            || parity["baseline_owner"].as_str() != Some(owner.as_str())
            || parity["baseline_epoch"].as_i64() != Some(epoch)
            || parity["ceiling"].as_i64() != Some(ceiling)
            || parity["manifest_hash"].as_str() != manifest.as_deref()
            || parity["surface_parity"].as_bool() != Some(true)
            || parity["query_version"].as_i64() != Some(1)
            || parity["target_identity"] != serde_json::to_value(target)?
        {
            return Err(HistoryError::Changed);
        }
        let watermark = if let Some(saved) = saved {
            saved.watermark
        } else {
            // Coalesced export rows cannot be used to prove historical delivery.
            // Reject a cut until every current head has immutable provenance or
            // an exact compact ACK; later writes may then safely replace export.
            let unfrozen:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM request_history_export e WHERE NOT EXISTS(SELECT 1 FROM request_history_immutable_outbox o WHERE o.revision=e.revision AND o.source=e.source AND o.request_id=e.request_id AND o.tenant_id=e.tenant_id AND o.key_id=e.key_id AND o.created_at=e.created_at) AND NOT EXISTS(SELECT 1 FROM request_history_identities i WHERE i.source=e.source AND i.request_id=e.request_id AND i.tenant_id=e.tenant_id AND i.key_id=e.key_id AND i.created_at=e.created_at AND i.head_revision=e.revision AND i.acknowledged_revision=e.revision AND LENGTH(i.acknowledged_hash)=64))").fetch_one(&mut **tx).await?;
            if unfrozen {
                return Err(HistoryError::Changed);
            }
            sqlx::query_scalar("SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM request_history_export_revision_seq").fetch_one(&mut **tx).await?
        };
        let proof = HistoryWatermarkProof {
            evidence_kind: "versioned_baseline".into(),
            read_model_version: READ_MODEL_VERSION,
            activation_epoch: activation,
            baseline_owner: owner,
            baseline_epoch: epoch,
            baseline_ceiling: ceiling,
            baseline_manifest: manifest.unwrap(),
            watermark,
            target_identity: target.clone(),
        };
        if saved.is_some_and(|saved| saved != &proof) {
            return Err(HistoryError::Changed);
        }
        let pending:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM request_history_immutable_outbox WHERE revision<=$1) OR EXISTS(SELECT 1 FROM request_history_export WHERE revision<=$1)").bind(watermark).fetch_one(&mut **tx).await?;
        Ok((proof, !pending))
    }
}

#[cfg(test)]
mod tests;
