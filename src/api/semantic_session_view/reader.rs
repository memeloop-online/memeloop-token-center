use super::*;
use crate::archive::semantic_reader::{self, Projection, Selection};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Position {
    Parts {
        start: usize,
    },
    Content {
        ordinal: usize,
        part_index: usize,
        field: String,
        format: String,
        offset: u64,
    },
    Media {
        ordinal: usize,
        part_index: usize,
    },
}
pub(super) fn token(
    state: &AppState,
    original: &Continuation,
    reader: Position,
) -> Result<String, AppError> {
    let mut position = original.clone();
    position.reader = Some(reader);
    encode(&position, state.config.key_pepper.as_bytes())
}
pub(super) fn response(body: Value) -> Response {
    ([(header::CACHE_CONTROL, "private, no-store")], Json(body)).into_response()
}
pub(super) async fn project(
    state: &AppState,
    scope: &SessionContextScope,
    position: &Continuation,
    selection: Selection,
) -> Result<Projection, AppError> {
    let instance = position.instance_id.as_deref().ok_or_else(invalid)?;
    let download = state
        .archive
        .open_semantic_view_atom(
            scope.tenant_id,
            scope.request_id,
            position.revision,
            position.page.as_ref(),
            instance,
        )
        .await?;
    semantic_reader::project(download, selection).await
}
pub(super) async fn dispatch(
    state: &AppState,
    scope: &SessionContextScope,
    revision: u64,
    query: &SemanticViewQuery,
) -> Result<Response, AppError> {
    let cursor = query
        .parts_cursor
        .as_ref()
        .or(query.content_cursor.as_ref())
        .or(query.media_cursor.as_ref())
        .ok_or_else(invalid)?;
    let position = decode(cursor, scope, state.config.key_pepper.as_bytes())?;
    if position.revision != revision {
        return Err(AppError::Conflict(
            "semantic manifest revision changed; refresh this view".into(),
        ));
    }
    if position.instance_id.is_none() {
        return Err(invalid());
    }
    if query.parts_cursor.is_some() {
        parts::read(state, scope, &position).await
    } else if query.content_cursor.is_some() {
        content::read(state, scope, &position).await
    } else {
        media::read(state, scope, &position).await
    }
}
