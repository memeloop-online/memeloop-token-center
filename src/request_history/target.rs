//! Non-secret identity of the actual history table, independent of endpoint or
//! credentials. Local tables use their UUID; unproven replica authority is rejected.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct HistoryTargetIdentity {
    pub(crate) database: String,
    pub(crate) table: String,
    pub(crate) authority: String,
    pub(crate) schema_hash: String,
}
impl ClickHouse {
    pub(crate) async fn target_identity(&self) -> Result<HistoryTargetIdentity, HistoryError> {
        #[derive(Deserialize)]
        struct Table {
            database: String,
            name: String,
            uuid: String,
            engine: String,
        }
        let rows:Vec<Table>=self.rows(query::BoundQuery::new("SELECT database,name,toString(uuid) AS uuid,engine FROM system.tables WHERE database=currentDatabase() AND name='request_history_v1' FORMAT JSONEachRow")).await?;
        let [table] = rows.as_slice() else {
            return Err(HistoryError::Mismatch);
        };
        #[derive(Serialize, Deserialize)]
        struct Column {
            name: String,
            #[serde(rename = "type")]
            kind: String,
            default_kind: String,
            default_expression: String,
        }
        let columns:Vec<Column>=self.rows(query::BoundQuery::new("SELECT name,type,default_kind,default_expression FROM system.columns WHERE database=currentDatabase() AND table='request_history_v1' ORDER BY position FORMAT JSONEachRow")).await?;
        if columns.is_empty() {
            return Err(HistoryError::Mismatch);
        }
        // A Keeper path alone cannot distinguish independent Keeper clusters.
        // This proof currently supports local table UUID authority only.
        if table.engine.starts_with("Replicated") {
            return Err(HistoryError::Invalid(
                "replicated history target requires verified cluster authority",
            ));
        }
        if table.uuid == "00000000-0000-0000-0000-000000000000" {
            return Err(HistoryError::Mismatch);
        }
        let authority = format!("table:{}", table.uuid);
        Ok(HistoryTargetIdentity {
            database: table.database.clone(),
            table: table.name.clone(),
            authority,
            schema_hash: canonical_hash(&columns)?,
        })
    }
}
