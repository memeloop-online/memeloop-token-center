use super::{
    reader::{self, Position},
    *,
};
use crate::archive::semantic_reader::Selection;

pub(super) async fn read(
    state: &AppState,
    scope: &SessionContextScope,
    position: &Continuation,
) -> Result<Response, AppError> {
    let Some(Position::Media {
        ordinal,
        part_index,
    }) = &position.reader
    else {
        return Err(invalid());
    };
    let projection =
        reader::project(state, scope, position, Selection::Media { part: *ordinal }).await?;
    let part = projection.parts.first().ok_or_else(invalid)?;
    if part.part_index != *part_index {
        return Err(invalid());
    }
    let object = CasArchiveObject {
        object_locator: part.locator.clone().ok_or_else(invalid)?,
        blake3_digest: part.digest.clone().ok_or_else(invalid)?,
        size_bytes: part.byte_length.ok_or_else(invalid)?,
    };
    let download = state
        .archive
        .open_tenant_content(scope.tenant_id, &object)
        .await?;
    Response::builder()
        .header(
            header::CONTENT_TYPE,
            part.media_type
                .as_deref()
                .unwrap_or("application/octet-stream"),
        )
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(Body::from_stream(download.stream))
        .map_err(|_| AppError::internal("semantic_media_response"))
}
