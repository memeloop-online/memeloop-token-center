use axum::body::Bytes;
use serde_json::{Map, Value, json};

const ENCRYPTED_FIELD: &str = "encrypted_content";

/// Preserve the exact body when no retention rule applies. This keeps legacy
/// request byte archives stable while ensuring inline media and explicitly
/// encrypted protocol fields never enter the durable archive.
pub(super) fn prepare_request_json(mut retained: Value) -> Option<Value> {
    sanitize_value(&mut retained, true, false).then_some(retained)
}

pub(super) fn encode_json_body(original: &Bytes, retained: &Value) -> Bytes {
    serde_json::to_vec(&retained)
        .map(Bytes::from)
        .unwrap_or_else(|_| metadata_only_body("serialization_failure", original.len()))
}

pub(super) fn prepare_json_body_if_valid(original: &Bytes) -> Option<Value> {
    let mut retained = match crate::api::sse::parse_unique_json(original) {
        Ok(value) => value,
        Err(_) if serde_json::from_slice::<Value>(original).is_ok() => {
            return Some(metadata_only_value("ambiguous_json", original.len()));
        }
        Err(_) => return None,
    };
    if !sanitize_value(&mut retained, true, false) {
        return None;
    }
    Some(retained)
}

pub(super) fn encoded_json_len(retained: &Value) -> usize {
    retained_size(retained)
}

#[cfg(test)]
fn json_body_if_valid(original: &Bytes) -> Bytes {
    prepare_json_body_if_valid(original).as_ref().map_or_else(
        || original.clone(),
        |retained| encode_json_body(original, retained),
    )
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
    if serde_json::from_slice::<Value>(&data).is_err() {
        return original.clone();
    }
    let (mut value, forced_metadata_only) = match crate::api::sse::parse_unique_json(&data) {
        Ok(value) => (value, false),
        Err(_) => (metadata_only_value("ambiguous_json", data.len()), true),
    };
    if !sanitize_value(&mut value, true, false) && !forced_metadata_only {
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

fn sanitize_value(value: &mut Value, infer_protocol: bool, media_context: bool) -> bool {
    match value {
        Value::Array(values) => {
            let mut changed = false;
            for value in values {
                changed |= sanitize_value(value, infer_protocol, media_context);
            }
            changed
        }
        Value::Object(object) => sanitize_object(object, infer_protocol, media_context),
        _ => false,
    }
}

fn sanitize_object(
    object: &mut Map<String, Value>,
    infer_protocol: bool,
    media_context: bool,
) -> bool {
    let object_type = object
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let media = media_context || (infer_protocol && is_media_part(object, object_type.as_deref()));
    let encrypted = infer_protocol && object_type.as_deref().is_some_and(is_encrypted_part);
    let assistant_message =
        infer_protocol && object.get("role").and_then(Value::as_str) == Some("assistant");
    let mut changed = false;
    let keys = object.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        let Some(value) = object.get_mut(&key) else {
            continue;
        };
        if encrypted && key == ENCRYPTED_FIELD && !value.is_null() {
            *value = omitted("encrypted_content", retained_size(value));
            changed = true;
            continue;
        }
        let inline_media = value
            .as_str()
            .and_then(|text| inline_media_kind(text).map(|media_kind| (media_kind, text.len())));
        if let Some((media_kind, original_bytes)) = inline_media {
            *value = omitted(media_kind, original_bytes);
            changed = true;
            continue;
        }
        if media
            && is_media_body_key(object_type.as_deref(), &key, media_context)
            && value.is_string()
        {
            *value = omitted("inline_media", retained_size(value));
            changed = true;
            continue;
        }
        let nested_media = (media && is_media_container_key(object_type.as_deref(), &key))
            || (assistant_message && is_chat_audio_container(&key, value));
        let nested_infer_protocol =
            infer_protocol && !is_opaque_tool_payload(object_type.as_deref(), &key);
        changed |= sanitize_value(value, nested_infer_protocol, nested_media);
    }
    changed
}

fn retained_size(value: &Value) -> usize {
    value.as_str().map(str::len).unwrap_or_else(|| {
        let mut counter = JsonByteCounter::default();
        serde_json::to_writer(&mut counter, value).map_or(0, |()| counter.bytes)
    })
}

#[derive(Default)]
struct JsonByteCounter {
    bytes: usize,
}

impl std::io::Write for JsonByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn omitted(kind: &str, original_bytes: usize) -> Value {
    json!({
        "retained": false,
        "kind": kind,
        "original_bytes": original_bytes,
    })
}

fn metadata_only_value(kind: &str, original_bytes: usize) -> Value {
    json!({
        "archive_retention": omitted(kind, original_bytes),
    })
}

fn metadata_only_body(kind: &str, original_bytes: usize) -> Bytes {
    serde_json::to_vec(&metadata_only_value(kind, original_bytes))
        .map(Bytes::from)
        .unwrap_or_else(|_| Bytes::from_static(br#"{"archive_retention":{"retained":false}}"#))
}

fn inline_media_kind(value: &str) -> Option<&'static str> {
    let prefix = value.get(..5)?;
    if !prefix.eq_ignore_ascii_case("data:") {
        return None;
    }
    let metadata = value.get(5..)?.split_once(',')?.0;
    let media_type = metadata.split(';').next()?;
    let (kind, _) = media_type.split_once('/')?;
    ["image", "audio", "video"]
        .iter()
        .copied()
        .find(|candidate| kind.eq_ignore_ascii_case(candidate))
}

fn is_media_part(object: &Map<String, Value>, kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some(
            "image_generation_call"
                | "input_image"
                | "output_image"
                | "computer_screenshot"
                | "input_audio"
                | "output_audio"
                | "input_video"
                | "output_video"
                | "response.image_generation_call.partial_image"
                | "response.audio.delta"
                | "response.output_audio.delta"
        )
    ) || kind.is_some_and(|kind| has_typed_base64_source(object, kind))
}

fn has_typed_base64_source(object: &Map<String, Value>, kind: &str) -> bool {
    if !matches!(kind, "image" | "audio" | "video") {
        return false;
    }
    let Some(source) = object.get("source").and_then(Value::as_object) else {
        return false;
    };
    if source.get("type").and_then(Value::as_str) != Some("base64") {
        return false;
    }
    source
        .get("media_type")
        .and_then(Value::as_str)
        .and_then(|media_type| media_type.split_once('/'))
        .is_some_and(|(media_kind, _)| media_kind.eq_ignore_ascii_case(kind))
}

fn is_media_container_key(object_type: Option<&str>, key: &str) -> bool {
    matches!(
        (object_type, key),
        (Some("image" | "audio" | "video"), "source") | (Some("input_audio"), "input_audio")
    )
}

fn is_chat_audio_container(key: &str, value: &Value) -> bool {
    if key != "audio" {
        return false;
    }
    let Some(audio) = value.as_object() else {
        return false;
    };
    audio.get("data").is_some_and(Value::is_string)
        && (audio.get("id").is_some_and(Value::is_string)
            || audio.get("transcript").is_some_and(Value::is_string)
            || audio.get("expires_at").is_some_and(Value::is_number))
}

fn is_opaque_tool_payload(object_type: Option<&str>, key: &str) -> bool {
    matches!(
        (object_type, key),
        (Some("tool_use"), "input")
            | (
                Some("function_call" | "custom_tool_call" | "mcp_call"),
                "input" | "arguments"
            )
            | (Some("function"), "arguments")
    )
}

fn is_encrypted_part(kind: &str) -> bool {
    matches!(kind, "reasoning" | "compaction" | "encrypted_content")
}

fn is_media_body_key(object_type: Option<&str>, key: &str, inherited_media: bool) -> bool {
    (inherited_media && matches!(key, "data" | "b64_json"))
        || matches!(
            (object_type, key),
            (Some("image_generation_call"), "result")
                | (Some("output_image"), "data" | "image" | "b64_json")
                | (Some("output_audio"), "data" | "audio")
                | (Some("output_video"), "data" | "video")
                | (
                    Some("response.image_generation_call.partial_image"),
                    "partial_image_b64"
                )
                | (
                    Some("response.audio.delta" | "response.output_audio.delta"),
                    "delta"
                )
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retained_request(body: &Bytes) -> Bytes {
        let parsed = crate::api::sse::parse_unique_json(body).unwrap();
        prepare_request_json(parsed)
            .as_ref()
            .map_or_else(|| body.clone(), |retained| encode_json_body(body, retained))
    }

    #[test]
    fn unchanged_json_preserves_exact_bytes() {
        let body = Bytes::from_static(br#"{ "model": "gpt", "input": "hello" }"#);
        assert_eq!(retained_request(&body), body);
    }

    #[test]
    fn typed_media_and_encrypted_fields_become_metadata_only() {
        let body = Bytes::from_static(
            br#"{"input":[{"type":"input_image","image_url":"data:image/png;base64,AAAA"},{"type":"input_audio","input_audio":{"data":"BBBB","format":"wav"}},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"DDDD"}},{"type":"reasoning","encrypted_content":"opaque"}],"output":[{"type":"image_generation_call","result":"CCCC"}],"tool":{"data":"ordinary"}}"#,
        );
        let retained: Value = serde_json::from_slice(&retained_request(&body)).unwrap();
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
    fn ordinary_tool_fields_are_not_treated_as_protocol_envelopes() {
        let body = Bytes::from_static(
            br#"{"type":"tool_use","input":{"encrypted_content":"application value","b64_json":"application value","data":"application value"}}"#,
        );
        assert_eq!(retained_request(&body), body);

        let domain_object = Bytes::from_static(
            br#"{"type":"image","media_type":"application/json","content":"thumbnail label","data":"business value"}"#,
        );
        assert_eq!(retained_request(&domain_object), domain_object);

        let tool_audio = Bytes::from_static(
            br#"{"type":"tool_use","input":{"nested":{"type":"reasoning","encrypted_content":"business value"},"message":{"role":"assistant","audio":{"id":"record-1","data":"business value","transcript":"label"}}}}"#,
        );
        assert_eq!(retained_request(&tool_audio), tool_audio);

        let typed_image_with_text = Bytes::from_static(
            br#"{"type":"output_image","content":"caption","data":"IMAGE_BASE64"}"#,
        );
        let retained: Value =
            serde_json::from_slice(&retained_request(&typed_image_with_text)).unwrap();
        assert_eq!(retained["content"], "caption");
        assert_eq!(retained["data"]["retained"], false);
    }

    #[test]
    fn structured_encrypted_content_uses_a_bounded_counting_marker() {
        let body = Bytes::from(format!(
            "{{\"type\":\"reasoning\",\"encrypted_content\":{{\"parts\":[{}]}}}}",
            (0..4_096)
                .map(|_| "\"opaque\"")
                .collect::<Vec<_>>()
                .join(",")
        ));
        let retained = retained_request(&body);
        assert!(retained.len() < 160);
        let retained: Value = serde_json::from_slice(&retained).unwrap();
        assert_eq!(retained["encrypted_content"]["original_bytes"], 36_875);
    }

    #[test]
    fn duplicate_sensitive_keys_fail_closed_without_retaining_the_body() {
        let body = Bytes::from_static(
            br#"{"type":"reasoning","encrypted_content":"SECRET","encrypted_content":null}"#,
        );
        let retained = json_body_if_valid(&body);
        assert!(!std::str::from_utf8(&retained).unwrap().contains("SECRET"));
        let retained: Value = serde_json::from_slice(&retained).unwrap();
        assert_eq!(retained["archive_retention"]["kind"], "ambiguous_json");
    }

    #[test]
    fn inline_media_scheme_and_type_are_ascii_case_insensitive() {
        let body = Bytes::from_static(
            br#"{"input":[{"type":"input_image","image_url":"DATA:IMAGE/png;base64,SECRET"}]}"#,
        );
        let retained = retained_request(&body);
        assert!(!std::str::from_utf8(&retained).unwrap().contains("SECRET"));
    }

    #[test]
    fn inline_media_without_parameters_and_long_mime_are_bounded() {
        let long_subtype = "x".repeat(16_384);
        let body = Bytes::from(format!(
            "{{\"image\":\"data:image/svg+xml,<svg>SECRET</svg>\",\"audio\":\"data:AUDIO/{long_subtype},SECRET\"}}"
        ));
        let retained = retained_request(&body);
        let retained_text = std::str::from_utf8(&retained).unwrap();
        assert!(!retained_text.contains("SECRET"));
        assert!(!retained_text.contains(&long_subtype));
        assert!(retained.len() < 256);
        let retained: Value = serde_json::from_slice(&retained).unwrap();
        assert_eq!(retained["image"]["kind"], "image");
        assert_eq!(retained["audio"]["kind"], "audio");
    }

    #[test]
    fn json_escapes_cannot_bypass_retention() {
        let encrypted =
            Bytes::from_static(br#"{"type":"reasoning","encrypted_\u0063ontent":"SECRET"}"#);
        let retained = retained_request(&encrypted);
        assert!(!std::str::from_utf8(&retained).unwrap().contains("SECRET"));

        let image = Bytes::from_static(
            br#"{"type":"input_image","image_url":"d\u0061ta:image/png;base64,SECRET"}"#,
        );
        let retained = retained_request(&image);
        assert!(!std::str::from_utf8(&retained).unwrap().contains("SECRET"));
    }

    #[test]
    fn chat_audio_and_compaction_payloads_become_metadata_only() {
        let body = Bytes::from_static(
            br#"{"choices":[{"message":{"role":"assistant","audio":{"id":"audio-1","expires_at":1,"data":"AUDIO_BASE64","transcript":"hello"}}}],"output":[{"type":"compaction","encrypted_content":"COMPACT_SECRET"}]}"#,
        );
        let retained: Value = serde_json::from_slice(&retained_request(&body)).unwrap();
        assert_eq!(
            retained["choices"][0]["message"]["audio"]["data"]["retained"],
            false
        );
        assert_eq!(
            retained["choices"][0]["message"]["audio"]["transcript"],
            "hello"
        );
        assert_eq!(
            retained["output"][0]["encrypted_content"]["retained"],
            false
        );
        let retained = serde_json::to_string(&retained).unwrap();
        assert!(!retained.contains("AUDIO_BASE64"));
        assert!(!retained.contains("COMPACT_SECRET"));
    }

    #[test]
    fn sse_archive_copy_redacts_without_changing_delivery_copy() {
        let delivered = Bytes::from_static(
            b"event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"secret\",\"summary\":[{\"text\":\"visible\"}]}}\n\n",
        );
        let archived = sse_frame(&delivered);
        assert!(std::str::from_utf8(&delivered).unwrap().contains("secret"));
        let archived = std::str::from_utf8(&archived).unwrap();
        assert!(!archived.contains("secret"));
        assert!(archived.contains("visible"));
        assert!(archived.contains("\"retained\":false"));
    }

    #[test]
    fn sse_archive_copy_omits_streamed_image_and_audio_bodies() {
        let partial_image = Bytes::from_static(
            b"event: response.image_generation_call.partial_image\ndata: {\"type\":\"response.image_generation_call.partial_image\",\"partial_image_b64\":\"IMAGE_BASE64\",\"partial_image_index\":0}\n\n",
        );
        let archived_image = sse_frame(&partial_image);
        let archived_image = std::str::from_utf8(&archived_image).unwrap();
        assert!(!archived_image.contains("IMAGE_BASE64"));
        assert!(archived_image.contains("partial_image_index"));
        assert!(archived_image.contains("\"retained\":false"));

        let audio_delta = Bytes::from_static(
            b"event: response.output_audio.delta\ndata: {\"type\":\"response.output_audio.delta\",\"delta\":\"AUDIO_BASE64\",\"sequence_number\":1}\n\n",
        );
        let archived_audio = sse_frame(&audio_delta);
        let archived_audio = std::str::from_utf8(&archived_audio).unwrap();
        assert!(!archived_audio.contains("AUDIO_BASE64"));
        assert!(archived_audio.contains("sequence_number"));
        assert!(archived_audio.contains("\"retained\":false"));
    }

    #[test]
    fn duplicate_sse_payload_fails_closed() {
        let delivered = Bytes::from_static(
            b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"SECRET\",\"encrypted_content\":null}}\n\n",
        );
        let archived = sse_frame(&delivered);
        let archived = std::str::from_utf8(&archived).unwrap();
        assert!(!archived.contains("SECRET"));
        assert!(archived.contains("ambiguous_json"));
    }
}
