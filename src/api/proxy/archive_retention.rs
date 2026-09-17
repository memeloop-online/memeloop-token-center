use axum::body::Bytes;
use serde_json::{Map, Value, json};

use crate::error::AppError;

const ENCRYPTED_FIELD: &str = "encrypted_content";

/// Preserve the exact body when no retention rule applies. This keeps legacy
/// request byte archives stable while ensuring inline media and explicitly
/// encrypted protocol fields never enter the durable archive.
pub(super) fn json_body(original: &Bytes, parsed: &Value) -> Result<Bytes, AppError> {
    let mut retained = parsed.clone();
    if !sanitize_value(&mut retained, false) {
        return Ok(original.clone());
    }
    serde_json::to_vec(&retained)
        .map(Bytes::from)
        .map_err(|_| AppError::Internal)
}

pub(super) fn json_body_if_valid(original: &Bytes) -> Bytes {
    let Ok(parsed) = serde_json::from_slice::<Value>(original) else {
        return original.clone();
    };
    json_body(original, &parsed).unwrap_or_else(|_| {
        Bytes::from_static(
            br#"{"archive_retention":{"retained":false,"kind":"serialization_failure"}}"#,
        )
    })
}

pub(super) fn sse_frame(original: &Bytes) -> Bytes {
    let mut framer = crate::api::sse::BoundedSseFramer::default();
    let batch = framer.push(original);
    if batch.rejection.is_some() || batch.events.len() != 1 || !framer.is_complete() {
        return original.clone();
    }
    let event = &batch.events[0];
    let Ok((_, Some(data))) = crate::api::sse::parse_sse_event(event) else {
        return original.clone();
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&data) else {
        return original.clone();
    };
    if !sanitize_value(&mut value, false) {
        return original.clone();
    }
    let Ok(data) = serde_json::to_vec(&value) else {
        return original.clone();
    };

    let mut output = Vec::with_capacity(original.len().min(data.len().saturating_add(128)));
    let mut wrote_data = false;
    for line in &event.lines {
        if crate::api::sse::is_sse_field_line(&line.value, b"data") {
            if !wrote_data {
                output.extend_from_slice(b"data: ");
                output.extend_from_slice(&data);
                output.extend_from_slice(&line.ending);
                wrote_data = true;
            }
        } else {
            output.extend_from_slice(&line.value);
            output.extend_from_slice(&line.ending);
        }
    }
    if !wrote_data {
        return original.clone();
    }
    output.extend_from_slice(&event.terminator);
    Bytes::from(output)
}

fn sanitize_value(value: &mut Value, media_context: bool) -> bool {
    match value {
        Value::Array(values) => {
            let mut changed = false;
            for value in values {
                changed |= sanitize_value(value, media_context);
            }
            changed
        }
        Value::Object(object) => sanitize_object(object, media_context),
        _ => false,
    }
}

fn sanitize_object(object: &mut Map<String, Value>, media_context: bool) -> bool {
    let media = media_context
        || object
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(is_media_part);
    let mut changed = false;
    let keys = object.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        let Some(value) = object.get_mut(&key) else {
            continue;
        };
        if key == ENCRYPTED_FIELD && !value.is_null() {
            *value = omitted("encrypted_content", retained_size(value));
            changed = true;
            continue;
        }
        if key == "b64_json" && value.is_string() {
            *value = omitted("inline_media", retained_size(value));
            changed = true;
            continue;
        }
        let inline_media = value.as_str().and_then(|text| {
            inline_media_type(text).map(|media_type| (media_type.to_owned(), text.len()))
        });
        if let Some((media_type, original_bytes)) = inline_media {
            *value = omitted(&media_type, original_bytes);
            changed = true;
            continue;
        }
        if media && is_media_body_key(&key) && value.is_string() {
            *value = omitted("inline_media", retained_size(value));
            changed = true;
            continue;
        }
        changed |= sanitize_value(value, media);
    }
    changed
}

fn retained_size(value: &Value) -> usize {
    value
        .as_str()
        .map(str::len)
        .unwrap_or_else(|| serde_json::to_vec(value).map_or(0, |bytes| bytes.len()))
}

fn omitted(kind: &str, original_bytes: usize) -> Value {
    json!({
        "retained": false,
        "kind": kind,
        "original_bytes": original_bytes,
    })
}

fn inline_media_type(value: &str) -> Option<&str> {
    let media_type = value.strip_prefix("data:")?.split_once(';')?.0;
    matches!(
        media_type.split_once('/').map(|(kind, _)| kind),
        Some("image" | "audio" | "video")
    )
    .then_some(media_type)
}

fn is_media_part(kind: &str) -> bool {
    matches!(
        kind,
        "image"
            | "image_generation_call"
            | "input_image"
            | "image_url"
            | "audio"
            | "input_audio"
            | "video"
            | "input_video"
    )
}

fn is_media_body_key(key: &str) -> bool {
    matches!(
        key,
        "data" | "result" | "image" | "audio" | "video" | "content"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_json_preserves_exact_bytes() {
        let body = Bytes::from_static(br#"{ "model": "gpt", "input": "hello" }"#);
        let parsed = serde_json::from_slice(&body).unwrap();
        assert_eq!(json_body(&body, &parsed).unwrap(), body);
    }

    #[test]
    fn typed_media_and_encrypted_fields_become_metadata_only() {
        let body = Bytes::from_static(
            br#"{"input":[{"type":"input_image","image_url":"data:image/png;base64,AAAA"},{"type":"input_audio","input_audio":{"data":"BBBB","format":"wav"}},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"DDDD"}},{"type":"reasoning","encrypted_content":"opaque"}],"output":[{"type":"image_generation_call","result":"CCCC"}],"tool":{"data":"ordinary"}}"#,
        );
        let parsed = serde_json::from_slice(&body).unwrap();
        let retained: Value = serde_json::from_slice(&json_body(&body, &parsed).unwrap()).unwrap();
        assert_eq!(retained["input"][0]["image_url"]["retained"], false);
        assert_eq!(
            retained["input"][1]["input_audio"]["data"]["retained"],
            false
        );
        assert_eq!(retained["input"][1]["input_audio"]["format"], "wav");
        assert_eq!(retained["input"][2]["source"]["data"]["retained"], false);
        assert_eq!(retained["input"][2]["source"]["media_type"], "image/png");
        assert_eq!(
            retained["input"][3]["encrypted_content"]["kind"],
            "encrypted_content"
        );
        assert_eq!(retained["tool"]["data"], "ordinary");
        assert_eq!(retained["output"][0]["result"]["retained"], false);
        assert!(!serde_json::to_string(&retained).unwrap().contains("AAAA"));
        assert!(!serde_json::to_string(&retained).unwrap().contains("BBBB"));
        assert!(!serde_json::to_string(&retained).unwrap().contains("opaque"));
        assert!(!serde_json::to_string(&retained).unwrap().contains("CCCC"));
        assert!(!serde_json::to_string(&retained).unwrap().contains("DDDD"));
    }

    #[test]
    fn sse_archive_copy_redacts_without_changing_delivery_copy() {
        let delivered = Bytes::from_static(
            b"event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"encrypted_content\":\"secret\",\"delta\":\"visible\"}\n\n",
        );
        let archived = sse_frame(&delivered);
        assert!(std::str::from_utf8(&delivered).unwrap().contains("secret"));
        let archived = std::str::from_utf8(&archived).unwrap();
        assert!(!archived.contains("secret"));
        assert!(archived.contains("visible"));
        assert!(archived.contains("\"retained\":false"));
    }
}
