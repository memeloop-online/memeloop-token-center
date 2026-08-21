use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::UpstreamCredential;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpstreamAccountView {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub tenant_external_id: Option<String>,
    pub name: String,
    pub driver: String,
    pub auth_kind: String,
    /// How this provider was connected. This is presentation metadata only;
    /// every connection method uses the same stable upstream account model.
    pub connection_method: String,
    pub credential_generation: i64,
    pub status: String,
    pub config: Value,
    pub credential_expires_at: Option<i64>,
    /// Server-derived lifecycle capabilities. Clients must use these instead
    /// of inferring actions from `auth_kind` or `connection_method`.
    pub can_refresh: bool,
    pub can_rotate: bool,
    pub can_reauthorize: bool,
    /// Number of model routes that still reference this stable upstream
    /// identity, including disabled routes retained for audit purposes.
    pub route_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelRouteView {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub tenant_external_id: Option<String>,
    pub public_model: String,
    pub upstream_account_id: Uuid,
    pub upstream_model: String,
    pub protocol: String,
    pub priority: i64,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug)]
pub struct ResolvedUpstream {
    pub route_id: Uuid,
    pub account_id: Uuid,
    pub driver: String,
    pub base_url: String,
    pub config: Value,
    pub upstream_model: String,
    pub credential: UpstreamCredential,
}
