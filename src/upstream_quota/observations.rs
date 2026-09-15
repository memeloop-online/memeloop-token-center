//! Cross-process quota evidence. This read path never performs supplier I/O.
use crate::{AppState, error::AppError, provider::AuthorizedUpstreamCandidate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RoutingQuotaObservation {
    pub account_id: Uuid,
    pub generation: i64,
    pub config_revision: i64,
    pub provider: String,
    pub observed_at: i64,
    pub valid_until: i64,
    pub windows: Vec<RoutingQuotaWindow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RoutingQuotaWindow {
    pub id: String,
    pub period_seconds: Option<i64>,
    pub reset_at: Option<i64>,
    pub reset_is_estimated: bool,
    pub remaining_fraction: Option<f64>,
    pub exhausted: Option<bool>,
}

pub(crate) async fn routing_observations(
    state: &AppState,
    tenant_id: Uuid,
    candidates: &[AuthorizedUpstreamCandidate],
    now_ms: i64,
) -> Result<BTreeMap<(Uuid, i64), RoutingQuotaObservation>, AppError> {
    state
        .db
        .routing_quota_observations(tenant_id, candidates, now_ms)
        .await
}
