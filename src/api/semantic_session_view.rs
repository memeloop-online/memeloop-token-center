//! Authorized semantic manifest pages. Continuations never authorize a caller:
//! ownership is rechecked on each request before verifying the signed position.
use super::{authenticate_downstream, management_tenant, require_service};
use crate::{
    AppState, archive::CasArchiveObject, crypto, db::SessionContextScope, error::AppError,
};
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

mod content;
mod media;
mod parts;
mod reader;

#[derive(Deserialize)]
pub(super) struct SemanticViewQuery {
    tenant_external_id: Option<String>,
    key_id: Option<Uuid>,
    cursor: Option<String>,
    atom_cursor: Option<String>,
    parts_cursor: Option<String>,
    content_cursor: Option<String>,
    media_cursor: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Continuation {
    tenant_id: Uuid,
    key_id: Uuid,
    session_id: Uuid,
    request_id: Uuid,
    revision: u64,
    page: Option<CasArchiveObject>,
    instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reader: Option<reader::Position>,
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
fn wire_page(
    mut body: Value,
    revision: u64,
    cursor: Option<String>,
    atom_tokens: Vec<String>,
) -> Result<Value, AppError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| AppError::internal("semantic_view_page_shape"))?;
    object.remove("previous");
    object.insert(
        "manifest_revision".into(),
        Value::String(revision.to_string()),
    );
    object.insert("status".into(), Value::String("published".into()));
    if let Some(instances) = object.get_mut("instances").and_then(Value::as_array_mut) {
        for (instance, token) in instances.iter_mut().zip(atom_tokens) {
            if let Some(instance) = instance.as_object_mut() {
                instance.remove("body");
                instance.insert("atom_cursor".into(), Value::String(token));
            }
        }
    }
    object.insert(
        "next_cursor".into(),
        serde_json::to_value(cursor)
            .map_err(|_| AppError::internal("semantic_view_cursor_shape"))?,
    );
    Ok(body)
}
async fn page(
    state: &AppState,
    scope: SessionContextScope,
    query: SemanticViewQuery,
) -> Result<Response, AppError> {
    if [
        query.cursor.as_ref(),
        query.atom_cursor.as_ref(),
        query.parts_cursor.as_ref(),
        query.content_cursor.as_ref(),
        query.media_cursor.as_ref(),
    ]
    .into_iter()
    .flatten()
    .count()
        > 1
    {
        return Err(invalid());
    }
    let revision = state.db.published_semantic_revision(&scope).await?;
    let Some(revision) = revision else {
        if query.cursor.is_some()
            || query.atom_cursor.is_some()
            || query.parts_cursor.is_some()
            || query.content_cursor.is_some()
            || query.media_cursor.is_some()
        {
            return Err(AppError::Conflict(
                "semantic publication is no longer bound; refresh this view".into(),
            ));
        }
        return Ok((
            [(header::CACHE_CONTROL, "private, no-store")],
            Json(serde_json::json!({
                "status": "pending", "manifest_revision": null,
                "coverage": {"observed_terminal": false, "projection_state": "partial",
                             "gaps": ["semantic_publication_pending"], "omissions": []},
                "instances": [], "next_cursor": null
            })),
        )
            .into_response());
    };
    if query.cursor.is_some() && query.atom_cursor.is_some() {
        return Err(invalid());
    }
    if query.parts_cursor.is_some()
        || query.content_cursor.is_some()
        || query.media_cursor.is_some()
    {
        return reader::dispatch(state, &scope, revision, &query).await;
    }
    if let Some(token) = query.atom_cursor.as_deref() {
        let position = decode(token, &scope, state.config.key_pepper.as_bytes())?;
        if position.revision != revision {
            return Err(AppError::Conflict(
                "semantic manifest revision changed; refresh this view".into(),
            ));
        }
        if position.reader.is_some() {
            return Err(invalid());
        }
        let instance = position.instance_id.as_deref().ok_or_else(invalid)?;
        let download = state
            .archive
            .open_semantic_view_atom(
                scope.tenant_id,
                scope.request_id,
                revision,
                position.page.as_ref(),
                instance,
            )
            .await?;
        return Ok((
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "private, no-store"),
            ],
            Body::from_stream(download.stream),
        )
            .into_response());
    }
    let continuation = query
        .cursor
        .as_deref()
        .map(|value| decode(value, &scope, state.config.key_pepper.as_bytes()))
        .transpose()?;
    if continuation.as_ref().is_some_and(|value| {
        value.revision != revision || value.instance_id.is_some() || value.reader.is_some()
    }) {
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
            continuation.as_ref().and_then(|value| value.page.as_ref()),
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
                    page: Some(previous.clone()),
                    instance_id: None,
                    reader: None,
                },
                state.config.key_pepper.as_bytes(),
            )
        })
        .transpose()?;
    let atom_tokens = page
        .instances
        .iter()
        .map(|instance| {
            encode(
                &Continuation {
                    tenant_id: scope.tenant_id,
                    key_id: scope.key_id,
                    session_id: scope.session_id,
                    request_id: scope.request_id,
                    revision,
                    page: continuation.as_ref().and_then(|value| value.page.clone()),
                    instance_id: Some(instance.instance_id.clone()),
                    reader: None,
                },
                state.config.key_pepper.as_bytes(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let body = wire_page(
        serde_json::to_value(page).map_err(|_| AppError::internal("semantic_view_page_encode"))?,
        revision,
        cursor,
        atom_tokens,
    )?;
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(body)).into_response())
}
pub(super) async fn internal_semantic_view(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((session_id, request_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<SemanticViewQuery>,
) -> Result<Response, AppError> {
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
) -> Result<Response, AppError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> SessionContextScope {
        SessionContextScope {
            tenant_id: Uuid::new_v4(),
            key_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
            leaf: None,
        }
    }

    #[test]
    fn semantic_revision_wire_string_preserves_full_u64() {
        let value = wire_page(serde_json::json!({
            "manifest_revision": u64::MAX,
            "previous": {"object_locator": "private-page"},
            "instances": [{"instance_id": "one", "role": "user", "body": {"object_locator": "private-atom"}}]
        }), u64::MAX, Some("signed-page".into()), vec!["signed-atom".into()]).unwrap();
        assert_eq!(value["manifest_revision"], "18446744073709551615");
        assert_eq!(value["instances"][0]["role"], "user");
        assert_eq!(value["instances"][0]["atom_cursor"], "signed-atom");
        assert!(value.get("previous").is_none());
        assert!(value["instances"][0].get("body").is_none());
        let encoded = serde_json::to_string(&value).unwrap();
        assert!(!encoded.contains("private-page"));
        assert!(!encoded.contains("private-atom"));
    }

    #[test]
    fn semantic_cursor_cannot_cross_scope_or_be_tampered() {
        let original = scope();
        let value = Continuation {
            tenant_id: original.tenant_id,
            key_id: original.key_id,
            session_id: original.session_id,
            request_id: original.request_id,
            revision: 7,
            page: None,
            instance_id: Some("occurrence-1".into()),
            reader: None,
        };
        let token = encode(&value, b"test-pepper").unwrap();
        assert_eq!(
            decode(&token, &original, b"test-pepper").unwrap().revision,
            7
        );
        assert!(decode(&token, &scope(), b"test-pepper").is_err());
        assert!(decode(&token, &original, b"other-pepper").is_err());
        let (payload, signature) = token.split_once('.').unwrap();
        let mut changed = URL_SAFE_NO_PAD.decode(payload).unwrap();
        changed.push(b' ');
        let forged = format!("{}.{}", URL_SAFE_NO_PAD.encode(changed), signature);
        assert!(decode(&forged, &original, b"test-pepper").is_err());
    }
}

#[cfg(test)]
mod reader_cursor_tests {
    use super::*;
    fn scope() -> SessionContextScope {
        SessionContextScope {
            tenant_id: Uuid::now_v7(),
            key_id: Uuid::now_v7(),
            session_id: Uuid::now_v7(),
            request_id: Uuid::now_v7(),
            leaf: None,
        }
    }
    #[test]
    fn typed_cursor_preserves_occurrence_field_offset_and_scope() {
        let scope = scope();
        let mut position = Continuation {
            tenant_id: scope.tenant_id,
            key_id: scope.key_id,
            session_id: scope.session_id,
            request_id: scope.request_id,
            revision: 9,
            page: None,
            instance_id: Some("occurrence-a".into()),
            reader: Some(reader::Position::Content {
                ordinal: 2,
                part_index: 7,
                field: "arguments".into(),
                format: "json".into(),
                offset: 65535,
            }),
        };
        let a = encode(&position, b"test").unwrap();
        position.instance_id = Some("occurrence-b".into());
        let b = encode(&position, b"test").unwrap();
        assert_ne!(a, b);
        let decoded = decode(&a, &scope, b"test").unwrap();
        assert_eq!(decoded.instance_id.as_deref(), Some("occurrence-a"));
        assert!(matches!(
            decoded.reader,
            Some(reader::Position::Content {
                ordinal: 2,
                part_index: 7,
                offset: 65535,
                ..
            })
        ));
        let mut other_scope = scope.clone();
        other_scope.key_id = Uuid::now_v7();
        assert!(decode(&a, &other_scope, b"test").is_err());
    }
}

#[cfg(test)]
mod http_contract;
