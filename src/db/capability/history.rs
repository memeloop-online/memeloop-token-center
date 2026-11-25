//! Read-only declaration of baseline receipt metadata. This capability is
//! independent of proxy started-marker storage and never executes migration DDL.
use super::{CapabilityNode, ColumnContract};

pub(super) fn baseline_receipt() -> CapabilityNode {
    CapabilityNode {
        id: "history.baseline_receipt.v1",
        semantic_version: "1.0.0",
        prerequisites: &["schema.legacy163.v1"],
        migration_source_digest: blake3::hash(include_bytes!(
            "../../../migrations/common/0165_history_watermark_proof.sql"
        ))
        .to_hex()
        .to_string(),
        columns: [
            ("read_model_version", "bigint"),
            ("activation_epoch", "bigint"),
            ("chunk_count", "bigint"),
            ("manifest_hash", "text"),
            ("sources_complete", "text"),
            ("parity_json", "text"),
            ("target_identity", "text"),
        ]
        .into_iter()
        .map(|(name, postgres_type)| ColumnContract {
            table: "request_history_baseline_runs",
            name,
            postgres_type,
            nullable: true,
        })
        .collect(),
    }
}
