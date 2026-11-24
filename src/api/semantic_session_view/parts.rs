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
    let start = match position.reader {
        None => 0,
        Some(Position::Parts { start }) => start,
        _ => return Err(invalid()),
    };
    let projection = reader::project(state, scope, position, Selection::Parts { start }).await?;
    let mut parts = Vec::new();
    for part in &projection.parts {
        let mut value =
            serde_json::to_value(part).map_err(|_| AppError::internal("semantic_parts_encode"))?;
        let mut fields = Vec::new();
        for field in &part.fields {
            let mut wire = serde_json::json!({"name":field.name,"format":field.format,"content_cursor":reader::token(state,position,Position::Content{ordinal:part.ordinal,part_index:part.part_index,field:field.name.clone(),format:field.format.clone(),offset:0})?});
            if let Some(preview) = &field.preview {
                let next = if preview.complete {
                    None
                } else {
                    Some(reader::token(
                        state,
                        position,
                        Position::Content {
                            ordinal: part.ordinal,
                            part_index: part.part_index,
                            field: field.name.clone(),
                            format: field.format.clone(),
                            offset: preview.text.len() as u64,
                        },
                    )?)
                };
                wire["preview"] = serde_json::json!({"text":preview.text,"byte_offset":0,"complete":preview.complete,"next_cursor":next});
            }
            fields.push(wire);
        }
        value["fields"] = Value::Array(fields);
        if part.locator.is_some() {
            value["media"] = serde_json::json!({"media_type":part.media_type,"byte_length":part.byte_length,"read_cursor":reader::token(state,position,Position::Media{ordinal:part.ordinal,part_index:part.part_index})?});
        }
        if part.fields.is_empty() && part.locator.is_none() {
            value["completeness"] = Value::String("unsupported".into());
        }
        parts.push(value);
    }
    let next = if projection.more {
        Some(reader::token(
            state,
            position,
            Position::Parts {
                start: start
                    .checked_add(projection.parts.len())
                    .ok_or_else(invalid)?,
            },
        )?)
    } else {
        None
    };
    Ok(reader::response(
        serde_json::json!({"manifest_revision":position.revision.to_string(),"instance_id":position.instance_id,"role":projection.role,"parts":parts,"next_cursor":next}),
    ))
}
