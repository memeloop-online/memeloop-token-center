use serde::Serialize;
use uuid::Uuid;

/// One concrete, currently usable provider candidate behind a model route that
/// the credential may use. Callers use it only to derive downstream model
/// capabilities, without exposing provider or route identifiers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantedModelCapabilitySource {
    pub public_model: String,
    pub protocol: String,
    pub driver: String,
    pub config_json: String,
}

#[derive(Clone, Copy, Debug)]
pub struct RouteSelectionOptions {
    pub upstream_account_hint: Option<Uuid>,
    pub selection_seed: Uuid,
}

#[derive(Clone, Debug, Default)]
pub struct ReplaceRouteRoutingInput {
    pub tenant_external_id: String,
    pub upstream_account_ids: Vec<Uuid>,
    pub included_provider_group_ids: Vec<Uuid>,
    pub excluded_provider_group_ids: Vec<Uuid>,
    pub route_group_ids: Vec<Uuid>,
    pub route_group_names: Vec<String>,
    pub granted_credential_ids: Vec<Uuid>,
    pub expected_updated_at: i64,
    pub expected_grant_revision: i64,
    pub custom_model_confirmed: bool,
}

#[derive(Clone, Debug)]
pub struct CreateRoutedModelRouteInput {
    pub tenant_external_id: String,
    pub public_model: String,
    pub upstream_model: String,
    pub protocol: String,
    pub priority: i64,
    pub upstream_account_ids: Vec<Uuid>,
    pub included_provider_group_ids: Vec<Uuid>,
    pub excluded_provider_group_ids: Vec<Uuid>,
    pub route_group_ids: Vec<Uuid>,
    pub route_group_names: Vec<String>,
    pub granted_credential_ids: Vec<Uuid>,
    pub custom_model_confirmed: bool,
}

#[derive(Clone, Debug)]
pub struct UpdateRoutedModelRouteInput {
    pub tenant_external_id: String,
    pub public_model: String,
    pub upstream_model: String,
    pub protocol: String,
    pub priority: i64,
    pub upstream_account_ids: Vec<Uuid>,
    pub included_provider_group_ids: Vec<Uuid>,
    pub excluded_provider_group_ids: Vec<Uuid>,
    pub route_group_ids: Vec<Uuid>,
    pub route_group_names: Vec<String>,
    pub granted_credential_ids: Vec<Uuid>,
    pub expected_updated_at: i64,
    pub expected_grant_revision: i64,
    pub custom_model_confirmed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct RouteRoutingView {
    pub route_id: Uuid,
    pub upstream_account_ids: Vec<Uuid>,
    pub included_provider_group_ids: Vec<Uuid>,
    pub excluded_provider_group_ids: Vec<Uuid>,
    pub route_group_ids: Vec<Uuid>,
    pub granted_credential_ids: Vec<Uuid>,
    pub candidate_upstream_account_ids: Vec<Uuid>,
    pub updated_at: i64,
    pub grant_revision: i64,
    pub custom_model_confirmed: bool,
}

#[derive(Clone, Debug)]
pub struct ReplaceCredentialRoutingInput {
    pub tenant_external_id: String,
    pub route_ids: Vec<Uuid>,
    pub route_group_ids: Vec<Uuid>,
    pub expected_grant_revision: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CredentialRoutingView {
    pub key_id: Uuid,
    pub route_ids: Vec<Uuid>,
    pub route_group_ids: Vec<Uuid>,
    pub effective_route_ids: Vec<Uuid>,
    pub updated_at: i64,
    pub grant_revision: i64,
}
