use super::{authenticate_downstream, management_tenant, require_service};
use crate::{
    AppState,
    db::session_tasks::{SessionIdentity, SessionNameUpdate},
    error::AppError,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ParentsRequest {
    tenant_external_id: Option<String>,
    parents: Vec<crate::db::session_tasks::ThreadParent>,
}

pub(super) async fn parents(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ParentsRequest>,
) -> Result<Json<Value>, AppError> {
    let tenant = tenant(&state, &headers, request.tenant_external_id, true).await?;
    Ok(Json(
        state
            .db
            .import_session_parents(tenant, &request.parents)
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskRequest {
    tenant_external_id: Option<String>,
    identities: Vec<SessionIdentity>,
    scope: String,
    #[serde(default = "default_task_timezone")]
    timezone: String,
    timezone_offset_minutes: Option<i32>,
    #[serde(default)]
    include_subtrees: bool,
}

fn default_task_timezone() -> String {
    "UTC".into()
}

impl TaskRequest {
    fn options(&self) -> crate::db::session_tasks::SessionTaskOptions {
        crate::db::session_tasks::SessionTaskOptions {
            timezone: self.timezone.clone(),
            timezone_offset_minutes: self.timezone_offset_minutes,
            include_subtrees: self.include_subtrees,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResolveRequest {
    tenant_external_id: Option<String>,
    explicit_session_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RelationshipQuery {
    tenant_external_id: Option<String>,
    key_id: Uuid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RelationshipChange {
    target: SessionIdentity,
    kind: String,
    evidence: Option<String>,
}

async fn tenant(
    state: &AppState,
    headers: &HeaderMap,
    external: Option<String>,
    write: bool,
) -> Result<Uuid, AppError> {
    let service = require_service(
        headers,
        state,
        if write { "keys:write" } else { "requests:read" },
    )
    .await?;
    let external = management_tenant(&service, external)?
        .ok_or_else(|| AppError::BadRequest("tenant_external_id is required".into()))?;
    state.db.session_task_tenant(&external).await
}

pub(super) async fn resolve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ResolveRequest>,
) -> Result<Json<Value>, AppError> {
    let tenant = tenant(&state, &headers, request.tenant_external_id, false).await?;
    Ok(Json(
        state
            .db
            .resolve_session_threads(tenant, None, &request.explicit_session_ids)
            .await?,
    ))
}

pub(super) async fn analyze(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<TaskRequest>,
) -> Result<Json<Value>, AppError> {
    let options = request.options();
    let tenant = tenant(&state, &headers, request.tenant_external_id, false).await?;
    Ok(Json(
        state
            .db
            .analyze_session_tasks(tenant, &request.identities, &request.scope, None, &options)
            .await?,
    ))
}

pub(super) async fn self_analyze(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<TaskRequest>,
) -> Result<Json<Value>, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(
        state
            .db
            .analyze_session_tasks(
                key.tenant_id,
                &request.identities,
                &request.scope,
                Some(key.key_id),
                &request.options(),
            )
            .await?,
    ))
}

pub(super) async fn relationships(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<Uuid>,
    Query(query): Query<RelationshipQuery>,
) -> Result<Json<Value>, AppError> {
    let tenant = tenant(&state, &headers, query.tenant_external_id, false).await?;
    Ok(Json(
        state
            .db
            .session_relationships(
                tenant,
                &SessionIdentity {
                    key_id: query.key_id,
                    session_id,
                },
                None,
            )
            .await?,
    ))
}

pub(super) async fn put_relationship(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<Uuid>,
    Query(query): Query<RelationshipQuery>,
    Json(change): Json<RelationshipChange>,
) -> Result<Json<Value>, AppError> {
    change_relationship(&state, &headers, session_id, query, change, false).await
}

pub(super) async fn delete_relationship(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<Uuid>,
    Query(query): Query<RelationshipQuery>,
    Json(change): Json<RelationshipChange>,
) -> Result<Json<Value>, AppError> {
    change_relationship(&state, &headers, session_id, query, change, true).await
}

async fn change_relationship(
    state: &AppState,
    headers: &HeaderMap,
    session_id: Uuid,
    query: RelationshipQuery,
    change: RelationshipChange,
    remove: bool,
) -> Result<Json<Value>, AppError> {
    let tenant = tenant(state, headers, query.tenant_external_id, true).await?;
    state
        .db
        .change_session_relationship(
            tenant,
            &SessionIdentity {
                key_id: query.key_id,
                session_id,
            },
            &change.target,
            &change.kind,
            change.evidence.as_deref(),
            remove,
        )
        .await?;
    Ok(Json(json!({"ok":true})))
}

pub(super) async fn name(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<Uuid>,
    Query(query): Query<RelationshipQuery>,
    Json(update): Json<SessionNameUpdate>,
) -> Result<Json<crate::db::SessionTitleView>, AppError> {
    let tenant = tenant(&state, &headers, query.tenant_external_id, true).await?;
    Ok(Json(
        state
            .db
            .update_session_name(
                tenant,
                &SessionIdentity {
                    key_id: query.key_id,
                    session_id,
                },
                update,
            )
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TitleBatchRequest {
    tenant_external_id: Option<String>,
    identities: Vec<SessionIdentity>,
}

pub(super) async fn titles(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<TitleBatchRequest>,
) -> Result<Json<Value>, AppError> {
    require_service(&headers, &state, "filter_assistant:execute").await?;
    let tenant = tenant(&state, &headers, request.tenant_external_id, true).await?;
    if request.identities.is_empty() {
        return Err(AppError::BadRequest("identities must be nonempty".into()));
    }
    Ok(Json(
        state
            .db
            .enqueue_unnamed_session_titles(tenant, &request.identities)
            .await?,
    ))
}
