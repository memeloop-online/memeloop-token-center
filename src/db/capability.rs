//! Versioned, binary-owned schema capabilities. Reports are observations, never
//! authorization tokens or substitutes for migration ledger entries.
use serde::{Deserialize, Serialize};
pub(super) mod catalog;
mod history;
pub mod permissions;
mod sqlite;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityRole {
    Gateway,
    Control,
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
    Linear166,
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
    pub role_operations: Vec<(CapabilityRole, Vec<permissions::RelationOperations>)>,
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
    pub available_capabilities: Vec<String>,
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
        role_operations: [
            CapabilityRole::Gateway,
            CapabilityRole::Worker,
            CapabilityRole::Control,
            CapabilityRole::HistoryReader,
            CapabilityRole::Maintenance,
        ]
        .into_iter()
        .map(|role| (role, permissions::requirements_at(role, 166)))
        .collect(),
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
            CapabilityNode {
                id: "oauth.rejected_generation.v1",
                semantic_version: "1.0.0",
                prerequisites: &["schema.legacy163.v1"],
                migration_source_digest: blake3::hash(include_bytes!(
                    "../../migrations/common/0166_oauth_rejected_generation.sql"
                ))
                .to_hex()
                .to_string(),
                columns: [
                    ("upstream_account_id", "text", false),
                    ("credential_generation", "bigint", false),
                    ("observed_at", "bigint", false),
                    ("verification_started_at", "bigint", true),
                    ("verification_finished_at", "bigint", true),
                    ("verification_error_code", "text", true),
                ]
                .into_iter()
                .map(|(name, postgres_type, nullable)| ColumnContract {
                    table: "upstream_oauth_rejected_generations",
                    name,
                    postgres_type,
                    nullable,
                })
                .collect(),
            },
        ],
    }
}

impl CapabilityReport {
    pub fn require(&self, id: &str) -> Result<(), sqlx::Error> {
        if self
            .available_capabilities
            .iter()
            .any(|available| available == id)
        {
            Ok(())
        } else {
            Err(sqlx::Error::Protocol(format!(
                "required capability unavailable: {id}"
            )))
        }
    }
}

/// Separate known Rust conversion receipts without weakening SQL sequence checks.
fn sql_schema_rows(rows: Vec<sqlx::any::AnyRow>) -> Result<Vec<sqlx::any::AnyRow>, sqlx::Error> {
    use sqlx::Row;
    let mut schema = Vec::with_capacity(rows.len());
    for row in rows {
        if !crate::db::migrations::is_data_conversion_receipt(
            row.try_get("version")?,
            &row.try_get::<String, _>("name")?,
        ) {
            schema.push(row);
        }
    }
    Ok(schema)
}
