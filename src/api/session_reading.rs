//! The context reader walks immutable prefix nodes in bounded pages. Signed
//! continuations authorize only a previously visited ancestor or item; every
//! HTTP request also rechecks current caller and retained session ownership.
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{authenticate_downstream, management_tenant, require_service};
use crate::{
    AppState, crypto,
    db::{SessionContextNode, SessionContextScope},
    error::AppError,
};

const CURSOR_DOMAIN: &str = "mtc-session-context-v1:";
const MAX_CURSOR_BYTES: usize = 4_096;

#[derive(Debug, Deserialize)]
pub(super) struct ContextQuery {
    tenant_external_id: Option<String>,
    key_id: Option<Uuid>,
    cursor: Option<String>,
    #[serde(default)]
    details: bool,
    #[serde(default)]
    response_only: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Position {
    Page {
        head: String,
    },
    Content {
        node: String,
        atom: String,
        offset: i64,
    },
    Text {
        node: String,
        atom: String,
        offset: i64,
    },
    ResponseText {
        atom: String,
        offset: i64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct Cursor {
    scope: SessionContextScope,
    position: Position,
}

#[derive(Serialize)]
pub(super) struct ContextPage {
    contract_version: &'static str,
    request_id: Uuid,
    session_id: Uuid,
    representation: &'static str,
    items: Vec<ContextItem>,
    next_cursor: Option<String>,
    response: ResponseSummary,
}

#[derive(Serialize)]
struct ResponseSummary {
    state: String,
    text: Option<String>,
    text_cursor: Option<String>,
    node_id: Option<String>,
    total_chars: i64,
    error_code: Option<String>,
}

#[derive(Serialize)]
struct ContextItem {
    /// Stable node identity, not a global text hash: repeated messages at
    /// different positions remain separate occurrences.
    node_id: String,
    depth: i64,
    role: String,
    kind: String,
    phase: Option<String>,
    text: Option<String>,
    text_complete: bool,
    source_chars: Option<i64>,
    content_cursor: String,
    text_cursor: Option<String>,
}

#[derive(Serialize)]
pub(super) struct ContextContent {
    contract_version: &'static str,
    representation: &'static str,
    node_id: String,
    /// Offsets count Unicode scalar values, not UTF-8 bytes or UTF-16 units.
    offset_chars: i64,
    total_chars: i64,
    content: String,
    next_cursor: Option<String>,
}

fn invalid_cursor() -> AppError {
    AppError::BadRequest("invalid or mismatched session context cursor".into())
}

fn encode_cursor(
    scope: &SessionContextScope,
    position: Position,
    pepper: &[u8],
) -> Result<String, AppError> {
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&Cursor {
            scope: scope.clone(),
            position,
        })
        .map_err(|_| AppError::Internal)?,
    );
    let (tag, _) = crypto::hash_credential(&format!("{CURSOR_DOMAIN}{payload}"), pepper);
    Ok(format!("{payload}.{}", URL_SAFE_NO_PAD.encode(tag)))
}

fn decode_cursor(
    encoded: &str,
    scope: &SessionContextScope,
    pepper: &[u8],
) -> Result<Position, AppError> {
    if encoded.len() > MAX_CURSOR_BYTES {
        return Err(invalid_cursor());
    }
    let (payload, signature) = encoded.split_once('.').ok_or_else(invalid_cursor)?;
    let tag = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| invalid_cursor())?;
    if !crypto::verify_credential(&format!("{CURSOR_DOMAIN}{payload}"), pepper, &tag) {
        return Err(invalid_cursor());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| invalid_cursor())?;
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| invalid_cursor())?;
    if cursor.scope != *scope {
        return Err(invalid_cursor());
    }
    Ok(cursor.position)
}

async fn operator_scope(
    state: &AppState,
    headers: &HeaderMap,
    query: &ContextQuery,
    session_id: Uuid,
    request_id: Uuid,
) -> Result<SessionContextScope, AppError> {
    let service = require_service(headers, state, "requests:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id.clone())?
        .ok_or_else(|| AppError::BadRequest("tenant_external_id is required".into()))?;
    let key_id = query
        .key_id
        .ok_or_else(|| AppError::BadRequest("key_id is required".into()))?;
    state
        .db
        .session_context_scope(Some(&tenant), None, key_id, session_id, request_id)
        .await
}

async fn self_scope(
    state: &AppState,
    headers: &HeaderMap,
    session_id: Uuid,
    request_id: Uuid,
) -> Result<SessionContextScope, AppError> {
    let key = authenticate_downstream(headers, state).await?;
    state
        .db
        .session_context_scope(
            None,
            Some(key.tenant_id),
            key.key_id,
            session_id,
            request_id,
        )
        .await
}

pub(super) async fn internal_session_context(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((session_id, request_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<ContextQuery>,
) -> Result<Json<ContextPage>, AppError> {
    let scope = operator_scope(&state, &headers, &query, session_id, request_id).await?;
    context_page(&state, scope, query).await.map(Json)
}

pub(super) async fn self_session_context(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((session_id, request_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<ContextQuery>,
) -> Result<Json<ContextPage>, AppError> {
    let scope = self_scope(&state, &headers, session_id, request_id).await?;
    context_page(&state, scope, query).await.map(Json)
}

pub(super) async fn internal_session_context_content(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((session_id, request_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<ContextQuery>,
) -> Result<Json<ContextContent>, AppError> {
    let scope = operator_scope(&state, &headers, &query, session_id, request_id).await?;
    context_content(&state, scope, query).await.map(Json)
}

pub(super) async fn self_session_context_content(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((session_id, request_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<ContextQuery>,
) -> Result<Json<ContextContent>, AppError> {
    let scope = self_scope(&state, &headers, session_id, request_id).await?;
    context_content(&state, scope, query).await.map(Json)
}

async fn context_page(
    state: &AppState,
    scope: SessionContextScope,
    query: ContextQuery,
) -> Result<ContextPage, AppError> {
    let pepper = state.config.key_pepper.as_bytes();
    let head = match query.cursor.as_deref() {
        None => scope.leaf.clone(),
        Some(cursor) => match decode_cursor(cursor, &scope, pepper)? {
            Position::Page { head } => Some(head),
            _ => return Err(invalid_cursor()),
        },
    };
    let page = state
        .db
        .session_context_page(
            &scope,
            if query.response_only {
                None
            } else {
                head.as_deref()
            },
        )
        .await?;
    let summary = state.db.session_response_summary(&scope).await?;
    let text_cursor = summary
        .atom
        .as_ref()
        .map(|atom| {
            encode_cursor(
                &scope,
                Position::ResponseText {
                    atom: atom.clone(),
                    offset: 0,
                },
                pepper,
            )
        })
        .transpose()?;
    let items = page
        .nodes
        .into_iter()
        .map(|node| project_node(node, &scope, query.details, pepper))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ContextPage {
        contract_version: "session_context_v1",
        request_id: scope.request_id,
        session_id: scope.session_id,
        representation: "semantic_context",
        items,
        response: ResponseSummary {
            state: summary.state,
            text: summary.text,
            text_cursor,
            node_id: summary.atom,
            total_chars: summary.total_chars,
            error_code: summary.error_code,
        },
        next_cursor: page
            .next_head
            .map(|head| encode_cursor(&scope, Position::Page { head }, pepper))
            .transpose()?,
    })
}

async fn context_content(
    state: &AppState,
    scope: SessionContextScope,
    query: ContextQuery,
) -> Result<ContextContent, AppError> {
    let pepper = state.config.key_pepper.as_bytes();
    let position = decode_cursor(
        query.cursor.as_deref().ok_or_else(invalid_cursor)?,
        &scope,
        pepper,
    )?;
    let (node, atom, offset, text_only, response_text) = match position {
        Position::Content { node, atom, offset } => (node, atom, offset, false, false),
        Position::Text { node, atom, offset } => (node, atom, offset, true, false),
        Position::ResponseText { atom, offset } => (atom.clone(), atom, offset, true, true),
        _ => return Err(invalid_cursor()),
    };
    let (content, total_chars) = if response_text {
        state
            .db
            .session_response_text(&scope, &atom, offset)
            .await?
    } else if text_only {
        state
            .db
            .session_context_text(&scope, &node, &atom, offset)
            .await?
    } else {
        state
            .db
            .session_context_content(&scope, &node, &atom, offset)
            .await?
    };
    if offset > total_chars {
        return Err(invalid_cursor());
    }
    let next = offset
        .checked_add(content.chars().count() as i64)
        .ok_or_else(invalid_cursor)?;
    if next < total_chars && content.is_empty() {
        return Err(AppError::Conflict(
            "retained context content is incomplete".into(),
        ));
    }
    Ok(ContextContent {
        contract_version: "session_context_content_v1",
        representation: if text_only {
            "message_text"
        } else {
            "semantic_context"
        },
        node_id: node.clone(),
        offset_chars: offset,
        total_chars,
        content,
        next_cursor: if next < total_chars {
            Some(encode_cursor(
                &scope,
                if response_text {
                    Position::ResponseText { atom, offset: next }
                } else if text_only {
                    Position::Text {
                        node,
                        atom,
                        offset: next,
                    }
                } else {
                    Position::Content {
                        node,
                        atom,
                        offset: next,
                    }
                },
                pepper,
            )?)
        } else {
            None
        },
    })
}

fn project_node(
    node: SessionContextNode,
    scope: &SessionContextScope,
    details: bool,
    pepper: &[u8],
) -> Result<ContextItem, AppError> {
    let phase = node
        .content
        .as_ref()
        .and_then(|value| value.get("phase").or_else(|| value.get("channel")))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let raw_text = node.content.as_ref().and_then(message_text);
    let text = match (node.role.as_str(), details, phase.as_deref()) {
        ("user", _, _)
        | ("assistant", true, _)
        | ("assistant", false, Some("final" | "final_answer")) => raw_text.clone(),
        ("assistant", false, None) => raw_text.as_deref().map(last_paragraph),
        _ => None,
    };
    let text_complete = text.is_some() && text == raw_text;
    let text_cursor = if matches!(node.role.as_str(), "user" | "assistant")
        && matches!(node.kind.as_str(), "message" | "agent_message")
    {
        Some(encode_cursor(
            scope,
            Position::Text {
                node: node.hash.clone(),
                atom: node.atom.clone(),
                offset: 0,
            },
            pepper,
        )?)
    } else {
        None
    };
    let content_cursor = encode_cursor(
        scope,
        Position::Content {
            node: node.hash.clone(),
            atom: node.atom,
            offset: 0,
        },
        pepper,
    )?;
    Ok(ContextItem {
        node_id: node.hash,
        depth: node.depth,
        role: node.role,
        kind: node.kind,
        phase,
        text,
        text_complete,
        source_chars: node.content_chars,
        content_cursor,
        text_cursor,
    })
}

fn message_text(value: &Value) -> Option<String> {
    let content = value.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let parts = content.as_array()?;
    let mut text = String::new();
    let mut found = false;
    for part in parts {
        // Tool results can appear in a user-role Anthropic content array.
        // Only explicitly textual blocks participate in the initial page.
        if !matches!(
            part.get("type").and_then(Value::as_str),
            Some("text" | "input_text" | "output_text")
        ) {
            continue;
        }
        if let Some(part) = part.get("text").and_then(Value::as_str) {
            text.push_str(part);
            found = true;
        }
    }
    found.then_some(text)
}

fn last_paragraph(value: &str) -> String {
    let value = value.trim_end();
    let mut start = 0;
    let mut position = 0;
    for line in value.split_inclusive('\n') {
        position += line.len();
        if line.trim().is_empty() {
            start = position;
        }
    }
    value[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope() -> SessionContextScope {
        SessionContextScope {
            tenant_id: Uuid::now_v7(),
            key_id: Uuid::now_v7(),
            session_id: Uuid::now_v7(),
            request_id: Uuid::now_v7(),
            leaf: Some("leaf".into()),
        }
    }

    #[test]
    fn cursors_bind_every_authorization_dimension_and_cannot_be_forged() {
        let original = scope();
        let token = encode_cursor(
            &original,
            Position::Page {
                head: "parent".into(),
            },
            b"pepper",
        )
        .unwrap();
        assert!(matches!(
            decode_cursor(&token, &original, b"pepper").unwrap(),
            Position::Page { .. }
        ));
        for changed in [
            SessionContextScope {
                tenant_id: Uuid::now_v7(),
                ..original.clone()
            },
            SessionContextScope {
                key_id: Uuid::now_v7(),
                ..original.clone()
            },
            SessionContextScope {
                session_id: Uuid::now_v7(),
                ..original.clone()
            },
            SessionContextScope {
                request_id: Uuid::now_v7(),
                ..original.clone()
            },
            SessionContextScope {
                leaf: Some("other".into()),
                ..original.clone()
            },
        ] {
            assert!(decode_cursor(&token, &changed, b"pepper").is_err());
        }
        assert!(decode_cursor(&token, &original, b"another-pepper").is_err());
        assert!(decode_cursor(&format!("{token}x"), &original, b"pepper").is_err());
    }

    #[test]
    fn overview_extracts_only_visible_text_never_tools_or_reasoning() {
        let value = json!({"content":[
            {"type":"input_text","text":"完整问题"},
            {"type":"tool_result","content":"secret tool output"},
            {"type":"thinking","text":"hidden reasoning"},
            {"type":"text","text":"第二部分"}
        ]});
        assert_eq!(message_text(&value).as_deref(), Some("完整问题第二部分"));
        assert_eq!(last_paragraph("first\n\nlast\nline\n"), "last\nline");
        assert_eq!(last_paragraph("first\r\n \r\n末段"), "末段");
    }
}
