use serde_json::Value;

use crate::AppState;

pub(super) const MAX_ARCHIVE_DETAIL_BODY: usize = 1024 * 1024;
const MAX_ARCHIVE_DETAIL_JSON_DEPTH: usize = 64;
const MAX_ARCHIVE_DETAIL_JSON_NODES: usize = 16 * 1024;
const MAX_ARCHIVE_DETAIL_JSON_STRING_BYTES: usize = 256 * 1024;
const MAX_ARCHIVE_DETAIL_JSON_ARRAY_ITEMS: usize = 8 * 1024;
const MAX_ARCHIVE_DETAIL_JSON_OBJECT_FIELDS: usize = 4 * 1024;

pub(super) async fn request_detail(
    state: &AppState,
    refs: crate::model::RequestArchiveRefs,
) -> crate::model::RequestDetail {
    let request = archive_value(state, &refs.request_object).await;
    let response = match refs.response_object.as_deref() {
        Some(location) => archive_value(state, location).await,
        None => match refs.response_json {
            Some(value) if json_value_structure_is_bounded(&value) => ArchiveValue {
                value,
                complete: true,
                reason: None,
            },
            Some(_) => ArchiveValue::gap("archive_payload_invalid"),
            None if refs.response_archive_state == crate::model::RequestArchiveState::Bound => {
                ArchiveValue {
                    value: Value::Null,
                    complete: true,
                    reason: None,
                }
            }
            None => ArchiveValue {
                value: Value::Null,
                complete: false,
                reason: refs.response_archive_reason.clone(),
            },
        },
    };
    crate::model::RequestDetail {
        view: refs.view,
        request_body: request.value,
        response_body: response.value,
        archive_complete: request.complete && response.complete,
        archive: crate::model::RequestArchiveCompletenessView {
            request: crate::model::RequestArchiveSideView {
                state: refs.request_archive_state,
                complete: request.complete,
                reason: request.reason.or(refs.request_archive_reason),
            },
            response: crate::model::RequestArchiveSideView {
                state: refs.response_archive_state,
                complete: response.complete,
                reason: response.reason.or(refs.response_archive_reason),
            },
        },
        provenance: refs.provenance,
    }
}

struct ArchiveValue {
    value: Value,
    complete: bool,
    reason: Option<String>,
}

impl ArchiveValue {
    fn gap(reason: &str) -> Self {
        Self {
            value: Value::Null,
            complete: false,
            reason: Some(reason.to_owned()),
        }
    }
}

async fn archive_value(state: &AppState, location: &str) -> ArchiveValue {
    if let Some(value) = location.strip_prefix("inline-json:") {
        return decode_archive_value(value.as_bytes());
    }
    if location.starts_with("gap://") {
        return ArchiveValue::gap("archive_object_unavailable");
    }
    match state
        .archive
        .get_bounded(location, MAX_ARCHIVE_DETAIL_BODY)
        .await
    {
        Ok(bytes) => decode_archive_value(&bytes),
        Err(_) => {
            // Imported locators and object-store errors can contain provider
            // paths, signed query values, or storage identity metadata.
            tracing::warn!(
                error_code = "archive_object_unavailable",
                "archived request object is unavailable"
            );
            ArchiveValue::gap("archive_object_unavailable")
        }
    }
}

fn decode_archive_value(bytes: &[u8]) -> ArchiveValue {
    if bytes.len() > MAX_ARCHIVE_DETAIL_BODY || !json_bytes_structure_is_bounded(bytes) {
        return ArchiveValue::gap("archive_payload_invalid");
    }
    match serde_json::from_slice(bytes) {
        Ok(value) if json_value_structure_is_bounded(&value) => ArchiveValue {
            value,
            complete: true,
            reason: None,
        },
        Ok(_) => ArchiveValue::gap("archive_payload_invalid"),
        Err(_) => ArchiveValue {
            value: Value::String(String::from_utf8_lossy(bytes).into_owned()),
            complete: true,
            reason: None,
        },
    }
}

/// Reject highly expanded JSON before serde allocates a `Value`. This is a
/// conservative structural scan; serde_json remains the source of truth for
/// syntax after the depth/node budget is proven bounded.
fn json_bytes_structure_is_bounded(bytes: &[u8]) -> bool {
    let mut depth = 0_usize;
    let mut nodes = 1_usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut string_bytes = 0_usize;
    let mut containers = Vec::<(u8, usize)>::new();
    for &byte in bytes {
        if in_string {
            string_bytes = string_bytes.saturating_add(1);
            if string_bytes > MAX_ARCHIVE_DETAIL_JSON_STRING_BYTES {
                return false;
            }
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                string_bytes = 0;
            }
            b'[' | b'{' => {
                depth = depth.saturating_add(1);
                nodes = nodes.saturating_add(1);
                containers.push((byte, 0));
                if depth > MAX_ARCHIVE_DETAIL_JSON_DEPTH || nodes > MAX_ARCHIVE_DETAIL_JSON_NODES {
                    return false;
                }
            }
            b']' | b'}' => {
                depth = depth.saturating_sub(1);
                containers.pop();
            }
            b',' => {
                nodes = nodes.saturating_add(1);
                if nodes > MAX_ARCHIVE_DETAIL_JSON_NODES {
                    return false;
                }
                if let Some((kind, commas)) = containers.last_mut() {
                    *commas = commas.saturating_add(1);
                    let limit = if *kind == b'[' {
                        MAX_ARCHIVE_DETAIL_JSON_ARRAY_ITEMS
                    } else {
                        MAX_ARCHIVE_DETAIL_JSON_OBJECT_FIELDS
                    };
                    if *commas >= limit {
                        return false;
                    }
                }
            }
            _ => {}
        }
    }
    true
}

pub(super) fn json_value_structure_is_bounded(root: &Value) -> bool {
    let mut stack = vec![(root, 0_usize)];
    let mut nodes = 0_usize;
    while let Some((value, depth)) = stack.pop() {
        nodes = nodes.saturating_add(1);
        if nodes > MAX_ARCHIVE_DETAIL_JSON_NODES || depth > MAX_ARCHIVE_DETAIL_JSON_DEPTH {
            return false;
        }
        match value {
            Value::Array(values) => {
                if values.len() > MAX_ARCHIVE_DETAIL_JSON_ARRAY_ITEMS {
                    return false;
                }
                stack.extend(values.iter().map(|value| (value, depth.saturating_add(1))));
            }
            Value::Object(values) => {
                if values.len() > MAX_ARCHIVE_DETAIL_JSON_OBJECT_FIELDS
                    || values.keys().any(|key| {
                        key.len() > MAX_ARCHIVE_DETAIL_JSON_STRING_BYTES
                            || key.chars().any(char::is_control)
                    })
                {
                    return false;
                }
                stack.extend(
                    values
                        .values()
                        .map(|value| (value, depth.saturating_add(1))),
                );
            }
            Value::String(value) if value.len() > MAX_ARCHIVE_DETAIL_JSON_STRING_BYTES => {
                return false;
            }
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_detail_json_is_rejected_before_structural_expansion() {
        let flat_array = format!(
            "[{}]",
            std::iter::repeat_n("0", MAX_ARCHIVE_DETAIL_JSON_ARRAY_ITEMS + 1)
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(!json_bytes_structure_is_bounded(flat_array.as_bytes()));
        let rejected = decode_archive_value(flat_array.as_bytes());
        assert_eq!(rejected.value, Value::Null);
        assert!(!rejected.complete);
        assert_eq!(rejected.reason.as_deref(), Some("archive_payload_invalid"));

        let deep = format!(
            "{}0{}",
            "[".repeat(MAX_ARCHIVE_DETAIL_JSON_DEPTH + 1),
            "]".repeat(MAX_ARCHIVE_DETAIL_JSON_DEPTH + 1)
        );
        assert!(!json_bytes_structure_is_bounded(deep.as_bytes()));

        let large_string = format!(
            "\"{}\"",
            "x".repeat(MAX_ARCHIVE_DETAIL_JSON_STRING_BYTES + 1)
        );
        assert!(!json_bytes_structure_is_bounded(large_string.as_bytes()));

        let safe = br#"{"items":[1,2,3],"text":"brackets [inside] a string"}"#;
        assert!(json_bytes_structure_is_bounded(safe));
        assert!(decode_archive_value(safe).complete);
    }
}
