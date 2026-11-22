use super::{
    reader::{self, Position},
    *,
};
use crate::archive::semantic_reader::{self, Selection};
use futures_util::StreamExt;

pub(super) async fn read(
    state: &AppState,
    scope: &SessionContextScope,
    position: &Continuation,
) -> Result<Response, AppError> {
    let Some(Position::Content {
        ordinal,
        part_index,
        field,
        format,
        offset,
    }) = &position.reader
    else {
        return Err(invalid());
    };
    let expected = match field.as_str() {
        "text" | "arguments_text" => "text",
        "arguments" | "result_content" => "json",
        _ => return Err(invalid()),
    };
    if format != expected {
        return Err(invalid());
    }
    let download = state
        .archive
        .open_semantic_view_atom(
            scope.tenant_id,
            scope.request_id,
            position.revision,
            position.page.as_ref(),
            position.instance_id.as_deref().ok_or_else(invalid)?,
        )
        .await?;
    let selection = Selection::Content {
        part: *ordinal,
        field: field.clone(),
        offset: *offset,
    };
    let original = position.clone();
    let state = state.clone();
    let (ordinal, part_index, field, format) =
        (*ordinal, *part_index, field.clone(), format.clone());
    let frames = semantic_reader::content_stream(download,selection).map(move |piece| {
        let piece = piece?;
        let next = if piece.complete {None} else {Some(reader::token(&state,&original,Position::Content{ordinal,part_index,field:field.clone(),format:format.clone(),offset:piece.offset.checked_add(piece.text.len() as u64).ok_or_else(||std::io::Error::other("content offset overflow"))?}).map_err(|_|std::io::Error::other("cursor encode failed"))?)};
        let mut line = serde_json::to_vec(&serde_json::json!({"manifest_revision":original.revision.to_string(),"instance_id":original.instance_id,"part_index":part_index,"field":field,"format":format,"byte_offset":piece.offset,"text":piece.text,"next_cursor":next,"complete":piece.complete})).map_err(std::io::Error::other)?;
        line.push(b'\n');
        Ok::<_,std::io::Error>(bytes::Bytes::from(line))
    });
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-ndjson"),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        Body::from_stream(frames),
    )
        .into_response())
}
