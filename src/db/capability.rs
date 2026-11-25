//! Versioned, binary-owned schema capabilities. Reports are observations, never
//! authorization tokens or substitutes for migration ledger entries.
use serde::{Deserialize, Serialize};
pub(super) mod catalog;
mod history;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityRole {
    Gateway,
    Worker,
    HistoryReader,
    Maintenance,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaTrack {
    Legacy163,
    Linear164,
    Linear165,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ColumnContract {
    pub table: &'static str,
    pub name: &'static str,
    pub postgres_type: &'static str,
    pub nullable: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct CapabilityNode {
    pub id: &'static str,
    pub semantic_version: &'static str,
    pub prerequisites: &'static [&'static str],
    pub migration_source_digest: String,
    pub columns: Vec<ColumnContract>,
}
#[derive(Clone, Debug, Serialize)]
pub struct CapabilityManifest {
    pub format_version: u32,
    pub nodes: Vec<CapabilityNode>,
}
#[derive(Clone, Debug, Serialize)]
pub struct CatalogSignature {
    pub object: String,
    pub signature: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct CapabilityReport {
    pub manifest_digest: String,
    pub database_identity: String,
    pub role: CapabilityRole,
    pub track: SchemaTrack,
    pub object_signatures: Vec<CatalogSignature>,
    pub verified_at: i64,
}
impl CapabilityManifest {
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        serde_json::to_vec(self).map(|bytes| blake3::hash(&bytes).to_hex().to_string())
    }
}
/// Static source authority; deployment configuration cannot replace it.
pub fn manifest() -> CapabilityManifest {
    CapabilityManifest {
        format_version: 1,
        nodes: vec![
            CapabilityNode {
                id: "schema.legacy163.v1",
                semantic_version: "1.0.0",
                prerequisites: &[],
                migration_source_digest: blake3::hash(include_bytes!(
                    "../../migrations/common/0163_history_event_backfill_cursor.sql"
                ))
                .to_hex()
                .to_string(),
                columns: vec![],
            },
            CapabilityNode {
                id: "proxy.started_pending.v1",
                semantic_version: "1.0.0",
                prerequisites: &["schema.legacy163.v1"],
                migration_source_digest: blake3::hash(include_bytes!(
                    "../../migrations/common/0164_proxy_started_obligation.sql"
                ))
                .to_hex()
                .to_string(),
                columns: vec![ColumnContract {
                    table: "request_records",
                    name: "started_event_pending",
                    postgres_type: "bigint",
                    nullable: false,
                }],
            },
            history::baseline_receipt(),
        ],
    }
}
