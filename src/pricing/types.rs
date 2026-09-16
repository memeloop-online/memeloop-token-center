use rust_decimal::Decimal;
use serde::Serialize;

use crate::model::ModelPriceView;

#[derive(Clone, Debug)]
pub(super) struct RemotePrice {
    pub(super) source: &'static str,
    pub(super) source_model_id: String,
    pub(super) input_per_million: Decimal,
    pub(super) cached_input_per_million: Option<Decimal>,
    pub(super) cache_write_per_million: Option<Decimal>,
    pub(super) output_per_million: Decimal,
    pub(super) service_tier: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSourceResult {
    pub source: String,
    pub models: usize,
    pub skipped: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncCandidate {
    pub source_model_id: String,
    pub source: String,
    pub reason: String,
    pub input_per_million: String,
    pub output_per_million: String,
    pub service_tier: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SyncCandidateSet {
    pub model: String,
    pub candidates: Vec<SyncCandidate>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPriceSyncResult {
    pub source: String,
    pub sources: Vec<String>,
    pub imported: usize,
    pub matched: Vec<String>,
    pub candidates: Vec<SyncCandidateSet>,
    pub unmatched: Vec<String>,
    pub preserved: Vec<String>,
    pub source_results: Vec<SyncSourceResult>,
    pub prices: Vec<ModelPriceView>,
}
