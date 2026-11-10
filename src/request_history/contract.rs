use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("invalid request history configuration or query: {0}")]
    Invalid(&'static str),
    #[error("request history unavailable")]
    Unavailable,
    #[error("request history HTTP request rejected (status {status}, ClickHouse code {code:?})")]
    Rejected { status: u16, code: Option<u32> },
    #[error("history changed during verification; retry after export catches up")]
    Changed,
    #[error("request history verification mismatch")]
    Mismatch,
    #[error("request history verification mismatch: {0}")]
    Verification(&'static str),
    #[error(transparent)]
    Database(Box<crate::error::AppError>),
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl From<crate::error::AppError> for HistoryError {
    fn from(error: crate::error::AppError) -> Self {
        Self::Database(Box::new(error))
    }
}

impl From<HistoryError> for crate::error::AppError {
    fn from(error: HistoryError) -> Self {
        match error {
            HistoryError::Database(error) => *error,
            HistoryError::Invalid(message) => Self::BadRequest(message.into()),
            _ => Self::internal("request_history_unavailable"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HistoryIdentity {
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    pub created_at: i64,
    pub request_id: Uuid,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HistorySnapshot {
    pub identity: HistoryIdentity,
    pub revision: i64,
    pub filter_cost_micros: Option<i64>,
    pub payload: Option<Value>,
    pub statistics: Option<HistoryStatistics>,
    /// Private locator/analysis metadata; never part of the public RequestView.
    #[serde(default)]
    pub read_model: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HistoryStatistics {
    pub status: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_known_read_tokens: Option<i64>,
    pub cache_known_input_tokens: Option<i64>,
    pub cost_micros: Option<i64>,
    pub currency: Option<String>,
}

impl HistorySnapshot {
    pub fn row(&self) -> Result<Value, HistoryError> {
        let payload = self.payload.as_ref().unwrap_or(&Value::Null);
        let cost = payload["billing"]["cost"]
            .as_str()
            .map(str::parse::<Decimal>)
            .transpose()
            .map_err(|_| HistoryError::Invalid("stored cost"))?
            .map(|cost| {
                cost.checked_mul(Decimal::from(1_000_000))
                    .ok_or(HistoryError::Invalid("stored cost overflow"))?
                    .to_i64()
                    .ok_or(HistoryError::Invalid("stored cost overflow"))
            })
            .transpose()?;
        let outcome = match payload["lifecycle_state"].as_str() {
            Some("succeeded") => "succeeded",
            Some("failed") => "failed",
            Some("cancelled") => "cancelled",
            _ => "pending",
        };
        let filter_status = if self.identity.source == "generation" {
            match outcome {
                "succeeded" => "success",
                "failed" | "cancelled" => "error",
                _ => "pending",
            }
        } else {
            match payload["status_code"].as_i64() {
                None => "pending",
                Some(200..=399) => "success",
                Some(400..) => "error",
                _ => "other",
            }
        };
        let fact = &self.read_model["fact"];
        let dimension = |name: &str| fact.get(name).unwrap_or(&payload[name]).clone();
        let uuid_dimension = |name: &str| {
            let value = dimension(name);
            if value.as_str().is_some_and(str::is_empty) {
                Value::Null
            } else {
                value
            }
        };
        Ok(json!({
            "tenant_id": self.identity.tenant_id,
            "key_id": self.identity.key_id,
            "created_at": self.identity.created_at,
            "request_id": self.identity.request_id,
            "source": self.identity.source,
            "revision": self.revision,
            "deleted": u8::from(self.payload.is_none()),
            "snapshot_hash": canonical_hash(self)?,
            "stats_status": self.statistics.as_ref().map(|value| value.status.as_str()).unwrap_or("pending"),
            "model": payload["model"].as_str().unwrap_or_default(),
            "protocol": payload["protocol"].as_str().unwrap_or_default(),
            "outcome": outcome,
            "filter_status": filter_status,
            "filter_cost_micros": self.filter_cost_micros,
            "stats_filter_cost_micros": self.statistics.as_ref().and_then(|s|s.cost_micros),
            "stats_model": dimension("model").as_str().unwrap_or_default(),
            "stats_protocol": dimension("protocol").as_str().unwrap_or_default(),
            "stats_error_code": dimension("error_code"),
            "stats_upstream_account_id": uuid_dimension("upstream_account_id"),
            "stats_route_id": uuid_dimension("route_id"),
            "stats_duration_ms": dimension("duration_ms"),
            "settlement_state": payload["operational_terminal"]["financial_state"].as_str()
                .unwrap_or("unknown"),
            "error_code": payload["error_code"],
            "upstream_account_id": payload["upstream_account_id"],
            "route_id": payload["route_id"],
            "duration_ms": payload["duration_ms"],
            "cost_micros": cost,
            "currency": payload["billing"]["currency"],
            "input_tokens": payload["input_tokens"],
            "output_tokens": payload["output_tokens"],
            "cached_input_tokens": payload["cached_input_tokens"],
            "cache_write_tokens": payload["cache_write_tokens"],
            "generation_units": payload["usage"]["generation"]["billed_units"],
            "billing_unit": payload["usage"]["generation"]["billing_unit"],
            "key_alias": payload["credential_identity"]["key_alias"],
            "principal": payload["credential_identity"]["principal_external_id"],
            "session_id": payload["session_context"]["session_id"],
            "task_kind": payload["session_context"]["task_kind"],
            "snapshot": serde_json::to_string(self)?,
        }))
    }
}

pub(crate) fn canonical_hash(value: &impl Serialize) -> Result<String, HistoryError> {
    let value = serde_json::to_value(value)?;
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?)))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HistoryCursor {
    pub before_created_at: i64,
    pub before_id: Uuid,
}

#[derive(Clone, Debug, Serialize)]
pub struct HistoryPage {
    pub requests: Vec<Value>,
    pub next_cursor: Option<HistoryCursor>,
}

impl HistoryPage {
    pub fn into_api_response(self, paged: bool) -> Result<Value, HistoryError> {
        Ok(if paged {
            serde_json::to_value(self)?
        } else {
            serde_json::to_value(self.requests)?
        })
    }
}
