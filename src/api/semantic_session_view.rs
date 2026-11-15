//! Authorized semantic manifest pages. Continuations never authorize a caller:
//! ownership is rechecked on each request before verifying the signed position.
use super::{authenticate_downstream, management_tenant, require_service};
use crate::{
    AppState, archive::CasArchiveObject, crypto, db::SessionContextScope, error::AppError,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Deserialize)]
pub(super) struct SemanticViewQuery {
    tenant_external_id: Option<String>,
    key_id: Option<Uuid>,
    cursor: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Continuation {
    tenant_id: Uuid,
    key_id: Uuid,
    session_id: Uuid,
    request_id: Uuid,
    revision: u64,
    page: CasArchiveObject,
}
const DOMAIN: &str = "mtc-semantic-view-v2:";
fn invalid() -> AppError {
    AppError::BadRequest("invalid or mismatched semantic view cursor".into())
}
fn encode(value: &Continuation, pepper: &[u8]) -> Result<String, AppError> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| AppError::internal("semantic_view_cursor_encode"))?;
    let payload = URL_SAFE_NO_PAD.encode(bytes);
    let (signature, _) = crypto::hash_credential(&format!("{DOMAIN}{payload}"), pepper);
    Ok(format!("{payload}.{}", URL_SAFE_NO_PAD.encode(signature)))
}
fn decode(
    value: &str,
    scope: &SessionContextScope,
    pepper: &[u8],
) -> Result<Continuation, AppError> {
    if value.len() > 16384 {
        return Err(invalid());
    }
    let (payload, signature) = value.split_once('.').ok_or_else(invalid)?;
    let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| invalid())?;
    if !crypto::verify_credential(&format!("{DOMAIN}{payload}"), pepper, &signature) {
        return Err(invalid());
    }
    let value: Continuation =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).map_err(|_| invalid())?)
            .map_err(|_| invalid())?;
    if value.tenant_id != scope.tenant_id
        || value.key_id != scope.key_id
        || value.session_id != scope.session_id
        || value.request_id != scope.request_id
        || value.revision == 0
    {
        return Err(invalid());
    }
    Ok(value)
}
async fn page(
    state: &AppState,
    scope: SessionContextScope,
    query: SemanticViewQuery,
) -> Result<Json<Value>, AppError> {
    let revision = state.db.published_semantic_revision(&scope).await?;
    let continuation = query
        .cursor
        .as_deref()
        .map(|value| decode(value, &scope, state.config.key_pepper.as_bytes()))
        .transpose()?;
    if continuation
        .as_ref()
        .is_some_and(|value| value.revision != revision)
    {
        return Err(AppError::Conflict(
            "semantic manifest revision changed; refresh this view".into(),
        ));
    }
    let page = state
        .archive
        .read_semantic_view_page(
            scope.tenant_id,
            scope.request_id,
            revision,
            continuation.as_ref().map(|value| &value.page),
        )
        .await?;
    let cursor = page
        .previous
        .as_ref()
        .map(|previous| {
            encode(
                &Continuation {
                    tenant_id: scope.tenant_id,
                    key_id: scope.key_id,
                    session_id: scope.session_id,
                    request_id: scope.request_id,
                    revision,
                    page: previous.clone(),
                },
                state.config.key_pepper.as_bytes(),
            )
        })
        .transpose()?;
    let mut body =
        serde_json::to_value(page).map_err(|_| AppError::internal("semantic_view_page_encode"))?;
    let object = body
        .as_object_mut()
        .ok_or_else(|| AppError::internal("semantic_view_page_shape"))?;
    object.remove("previous");
    object.insert(
        "next_cursor".into(),
        serde_json::to_value(cursor)
            .map_err(|_| AppError::internal("semantic_view_cursor_shape"))?,
    );
    Ok(Json(body))
}
pub(super) async fn internal_semantic_view(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((session_id, request_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<SemanticViewQuery>,
) -> Result<Json<Value>, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id.clone())?
        .ok_or_else(|| AppError::BadRequest("tenant_external_id is required".into()))?;
    let key = query
        .key_id
        .ok_or_else(|| AppError::BadRequest("key_id is required".into()))?;
    let scope = state
        .db
        .session_context_scope(Some(&tenant), None, key, session_id, request_id)
        .await?;
    page(&state, scope, query).await
}
pub(super) async fn self_semantic_view(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((session_id, request_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<SemanticViewQuery>,
) -> Result<Json<Value>, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let scope = state
        .db
        .session_context_scope(
            None,
            Some(key.tenant_id),
            key.key_id,
            session_id,
            request_id,
        )
        .await?;
    page(&state, scope, query).await
}
