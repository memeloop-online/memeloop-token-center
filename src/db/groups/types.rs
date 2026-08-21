use serde::Serialize;
use uuid::Uuid;

/// The three control-plane group families.
///
/// Provider groups select upstream candidates and route groups grant model
/// routes. Credential groups are presentation-only categorization and must
/// never participate in authorization or candidate resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupKind {
    Provider,
    Route,
    Credential,
}

impl GroupKind {
    pub(super) fn tables(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Provider => (
                "provider_groups",
                "upstream_account_provider_groups",
                "provider_group_id",
            ),
            Self::Route => (
                "route_groups",
                "model_route_group_memberships",
                "route_group_id",
            ),
            Self::Credential => (
                "credential_groups",
                "credential_group_memberships",
                "credential_group_id",
            ),
        }
    }

    pub(super) fn member_column(self) -> &'static str {
        match self {
            Self::Provider => "upstream_account_id",
            Self::Route => "model_route_id",
            Self::Credential => "key_id",
        }
    }

    pub(super) fn member_table(self) -> &'static str {
        match self {
            Self::Provider => "upstream_accounts",
            Self::Route => "model_routes",
            Self::Credential => "key_records",
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Route => "route",
            Self::Credential => "credential",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct GroupView {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub tenant_external_id: String,
    pub name: String,
    pub member_ids: Vec<Uuid>,
    pub member_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug)]
pub struct CreateGroupInput {
    pub tenant_external_id: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct UpdateGroupInput {
    pub tenant_external_id: String,
    pub name: String,
    pub expected_updated_at: i64,
}

#[derive(Clone, Debug)]
pub struct ReplaceGroupMembersInput {
    pub tenant_external_id: String,
    pub member_ids: Vec<Uuid>,
    pub expected_updated_at: i64,
}
