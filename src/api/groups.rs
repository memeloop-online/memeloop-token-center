use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;
use uuid::Uuid;

use super::{require_service, require_service_tenant};
use crate::{
    AppState,
    db::{CreateGroupInput, GroupKind, ReplaceGroupMembersInput, UpdateGroupInput},
    error::AppError,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GroupListQuery {
    tenant_external_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateGroupRequest {
    tenant_external_id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdateGroupRequest {
    tenant_external_id: String,
    name: String,
    expected_updated_at: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeleteGroupQuery {
    tenant_external_id: String,
    expected_updated_at: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReplaceGroupMembersRequest {
    tenant_external_id: String,
    member_ids: Vec<Uuid>,
    expected_updated_at: i64,
}

macro_rules! group_handlers {
    ($list:ident, $create:ident, $update:ident, $delete:ident, $members:ident, $kind:expr, $read_scope:literal, $write_scope:literal) => {
        pub(super) async fn $list(
            State(state): State<AppState>,
            headers: HeaderMap,
            Query(query): Query<GroupListQuery>,
        ) -> Result<impl IntoResponse, AppError> {
            let service = require_service(&headers, &state, $read_scope).await?;
            require_service_tenant(&service, &query.tenant_external_id)?;
            Ok(Json(
                state
                    .db
                    .list_groups($kind, &query.tenant_external_id)
                    .await?,
            ))
        }

        pub(super) async fn $create(
            State(state): State<AppState>,
            headers: HeaderMap,
            Json(body): Json<CreateGroupRequest>,
        ) -> Result<impl IntoResponse, AppError> {
            let service = require_service(&headers, &state, $write_scope).await?;
            require_service_tenant(&service, &body.tenant_external_id)?;
            Ok((
                StatusCode::CREATED,
                Json(
                    state
                        .db
                        .create_group(
                            $kind,
                            CreateGroupInput {
                                tenant_external_id: body.tenant_external_id,
                                name: body.name,
                            },
                        )
                        .await?,
                ),
            ))
        }

        pub(super) async fn $update(
            State(state): State<AppState>,
            headers: HeaderMap,
            Path(group_id): Path<Uuid>,
            Json(body): Json<UpdateGroupRequest>,
        ) -> Result<impl IntoResponse, AppError> {
            let service = require_service(&headers, &state, $write_scope).await?;
            require_service_tenant(&service, &body.tenant_external_id)?;
            Ok(Json(
                state
                    .db
                    .update_group(
                        $kind,
                        group_id,
                        UpdateGroupInput {
                            tenant_external_id: body.tenant_external_id,
                            name: body.name,
                            expected_updated_at: body.expected_updated_at,
                        },
                    )
                    .await?,
            ))
        }

        pub(super) async fn $delete(
            State(state): State<AppState>,
            headers: HeaderMap,
            Path(group_id): Path<Uuid>,
            Query(query): Query<DeleteGroupQuery>,
        ) -> Result<impl IntoResponse, AppError> {
            let service = require_service(&headers, &state, $write_scope).await?;
            require_service_tenant(&service, &query.tenant_external_id)?;
            state
                .db
                .delete_group(
                    $kind,
                    group_id,
                    &query.tenant_external_id,
                    query.expected_updated_at,
                )
                .await?;
            Ok(StatusCode::NO_CONTENT)
        }

        pub(super) async fn $members(
            State(state): State<AppState>,
            headers: HeaderMap,
            Path(group_id): Path<Uuid>,
            Json(body): Json<ReplaceGroupMembersRequest>,
        ) -> Result<impl IntoResponse, AppError> {
            let service = require_service(&headers, &state, $write_scope).await?;
            require_service_tenant(&service, &body.tenant_external_id)?;
            Ok(Json(
                state
                    .db
                    .replace_group_members(
                        $kind,
                        group_id,
                        ReplaceGroupMembersInput {
                            tenant_external_id: body.tenant_external_id,
                            member_ids: body.member_ids,
                            expected_updated_at: body.expected_updated_at,
                        },
                    )
                    .await?,
            ))
        }
    };
}

group_handlers!(
    list_provider_groups,
    create_provider_group,
    update_provider_group,
    delete_provider_group,
    replace_provider_group_members,
    GroupKind::Provider,
    "routes:read",
    "routes:write"
);
group_handlers!(
    list_route_groups,
    create_route_group,
    update_route_group,
    delete_route_group,
    replace_route_group_members,
    GroupKind::Route,
    "routes:read",
    "routes:write"
);
group_handlers!(
    list_credential_groups,
    create_credential_group,
    update_credential_group,
    delete_credential_group,
    replace_credential_group_members,
    GroupKind::Credential,
    "keys:read",
    "keys:write"
);
