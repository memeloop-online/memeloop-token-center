//! Compact, revisioned routing contributions from the verified history read model.
use super::{ClickHouse, HistoryError, HistorySnapshot, query::BoundQuery};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) const ALLOCATION_PAGE: usize = 256;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct AllocationContribution {
    pub tenant_id: Uuid,
    pub source: String,
    pub request_id: Uuid,
    pub revision: i64,
    pub created_at: i64,
    pub account_id: String,
    pub requests: i64,
    pub tokens: i64,
    pub covered: i64,
}

impl AllocationContribution {
    pub(crate) fn from_snapshot(snapshot: &HistorySnapshot) -> Option<Self> {
        if !matches!(snapshot.identity.source.as_str(), "native" | "generation") {
            return None;
        }
        let fact = &snapshot.read_model["fact"];
        let settled = snapshot.payload.is_some() && fact["settled_fact"] == true;
        let number = |field: &str| fact[field].as_i64().unwrap_or_default().max(0);
        let tokens = if settled && snapshot.identity.source == "native" {
            number("input_tokens")
                .saturating_sub(number("cached_input_tokens"))
                .saturating_sub(number("cache_write_tokens"))
                .max(0)
                .saturating_add(number("output_tokens"))
        } else {
            0
        };
        Some(Self {
            tenant_id: snapshot.identity.tenant_id,
            source: snapshot.identity.source.clone(),
            request_id: snapshot.identity.request_id,
            revision: snapshot.revision,
            created_at: snapshot.identity.created_at,
            account_id: if settled {
                fact["upstream_account_id"]
                    .as_str()
                    .unwrap_or_default()
                    .into()
            } else {
                String::new()
            },
            requests: i64::from(settled),
            tokens,
            covered: i64::from(settled || snapshot.payload.is_none()),
        })
    }
}

#[derive(Clone, Default)]
pub(crate) struct AllocationCursor {
    pub created_at: i64,
    pub key_id: String,
    pub request_id: String,
    pub source: String,
}

#[derive(Deserialize)]
pub(crate) struct AllocationHistoryRow {
    #[serde(flatten)]
    pub contribution: AllocationContribution,
    pub key_id: String,
}

impl ClickHouse {
    /// Bootstrap reads one compact FINAL page; subsequent changes arrive through export ACKs.
    pub(crate) async fn allocation_page(
        &self,
        tenant: Uuid,
        from: i64,
        to: i64,
        cursor: &AllocationCursor,
    ) -> Result<Vec<AllocationHistoryRow>, HistoryError> {
        let mut query = BoundQuery::new("");
        let tenant = query.bind(tenant, "UUID");
        let from = query.bind(from, "Int64");
        let to = query.bind(to, "Int64");
        let created = query.bind(cursor.created_at, "Int64");
        let key = query.bind(&cursor.key_id, "String");
        let request = query.bind(&cursor.request_id, "String");
        let source = query.bind(&cursor.source, "String");
        query.sql = format!(
            "SELECT tenant_id,source,request_id,toInt64(revision) AS revision,created_at,toString(key_id) AS key_id,
                if(settled,JSONExtractString(fact,'upstream_account_id'),'') AS account_id,
                toInt64(settled) AS requests,
                if(settled AND source='native',
                    greatest(JSONExtractInt(fact,'input_tokens')-greatest(JSONExtractInt(fact,'cached_input_tokens'),0)-greatest(JSONExtractInt(fact,'cache_write_tokens'),0),0)
                    +greatest(JSONExtractInt(fact,'output_tokens'),0),0) AS tokens,
                toInt64(settled OR deleted=1) AS covered
             FROM (
                SELECT *,JSONExtractRaw(snapshot,'read_model','fact') AS fact,
                    deleted=0 AND JSONExtractBool(fact,'settled_fact') AS settled
                FROM request_history_v1 FINAL
                WHERE tenant_id={tenant} AND created_at>={from} AND created_at<={to}
                    AND source IN ('native','generation')
                    AND (created_at,toString(key_id),toString(request_id),source)>({created},{key},{request},{source})
             )
             ORDER BY created_at,key_id,request_id,source LIMIT {ALLOCATION_PAGE} FORMAT JSONEachRow"
        );
        self.rows(query).await
    }
}
