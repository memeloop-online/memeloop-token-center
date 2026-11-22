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
    if !matches!(
        field.as_str(),
        "text" | "arguments" | "arguments_text" | "result_content"
    ) {
        return Err(invalid());
    }
    let projection = reader::project(
        state,
        scope,
        position,
        Selection::Content {
            part: *ordinal,
            field: field.clone(),
            offset: *offset,
        },
    )
    .await?;
    if projection.format.as_ref() != Some(format) {
        return Err(invalid());
    }
    let next = if projection.complete {
        None
    } else {
        if projection.text.is_empty() {
            return Err(AppError::storage("semantic_content_progress_invalid"));
        }
        Some(reader::token(
            state,
            position,
            Position::Content {
                ordinal: *ordinal,
                part_index: *part_index,
                field: field.clone(),
                format: format.clone(),
                offset: offset
                    .checked_add(projection.text.len() as u64)
                    .ok_or_else(invalid)?,
            },
        )?)
    };
    Ok(reader::response(
        serde_json::json!({"manifest_revision":position.revision.to_string(),"instance_id":position.instance_id,"part_index":part_index,"field":field,"format":format,"byte_offset":offset,"text":projection.text,"next_cursor":next,"complete":projection.complete}),
    ))
}
