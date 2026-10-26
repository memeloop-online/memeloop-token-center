use axum::body::Bytes;
use serde_json::{Value, json};

pub(crate) fn encode_json_body(original: &Bytes, retained: &Value) -> Bytes {
    serde_json::to_vec(&retained)
        .map(Bytes::from)
        .unwrap_or_else(|_| metadata_only_body("serialization_failure", original.len()))
}

/// Return a substitute only when the bytes are JSON with duplicate keys.
///
/// Unique JSON yields `None`, and the caller stores the original request or
/// response bytes. Image, audio, video, and file bytes stay in that archive.
/// Codex and other provider fields stay in that archive; the conversation
/// record written later in CNPG handles those fields. Objects already stored
/// with `retained: false` stay unchanged.
pub(crate) fn prepare_json_body_if_valid(original: &Bytes) -> Option<Value> {
    is_ambiguous_json(original).then(|| metadata_only_value("ambiguous_json", original.len()))
}

#[cfg(test)]
fn json_body_if_valid(original: &Bytes) -> Bytes {
    prepare_json_body_if_valid(original).as_ref().map_or_else(
        || original.clone(),
        |retained| encode_json_body(original, retained),
    )
}

#[cfg(test)]
pub(super) fn sse_frame(original: &Bytes) -> Bytes {
    sse_frame_with_limits(original, crate::provider::SseFramingLimits::default())
}

/// Copy one complete SSE event into the archive.
///
/// Unique JSON stays byte for byte, including image, audio, video, file, and
/// provider fields. Duplicate keys become a metadata marker that stays within
/// the original frame size.
pub(super) fn sse_frame_with_limits(
    original: &Bytes,
    limits: crate::provider::SseFramingLimits,
) -> Bytes {
    let mut framer = crate::api::sse::BoundedSseFramer::with_limits(limits);
    let batch = framer.push(original);
    if batch.rejection.is_some() || batch.events.len() != 1 || !framer.is_complete() {
        return original.clone();
    }
    let event = &batch.events[0];
    let Ok((_, Some(data))) = crate::api::sse::parse_sse_event(event) else {
        return original.clone();
    };
    if !is_ambiguous_json(&data) {
        return original.clone();
    }
    let value = metadata_only_value("ambiguous_json", data.len());
    let Ok(mut retained_data) = serde_json::to_vec(&value) else {
        return original.clone();
    };
    if retained_data.len() > data.len() {
        retained_data =
            serde_json::to_vec(&metadata_only_value("retention_projection", data.len()))
                .unwrap_or_else(|_| br#"{"retained":false}"#.to_vec());
        if retained_data.len() > data.len() {
            retained_data = b"null".to_vec();
        }
    }

    let mut output = Vec::with_capacity(original.len());
    let mut wrote_data = false;
    for line in &event.lines {
        if crate::api::sse::is_sse_field_line(&line.value, b"data") {
            if !wrote_data {
                let prefix = if line.value.starts_with(b"data: ") {
                    b"data: ".as_slice()
                } else {
                    b"data:".as_slice()
                };
                output.extend_from_slice(prefix);
                output.extend_from_slice(&retained_data);
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
    if output.len() > original.len() {
        return Bytes::from_static(b"data:null\n\n");
    }
    Bytes::from(output)
}

/// True when serde accepts the bytes and a single document cannot be chosen.
fn is_ambiguous_json(original: &[u8]) -> bool {
    crate::api::sse::parse_unique_json(original).is_err()
        && serde_json::from_slice::<Value>(original).is_ok()
}

/// Metadata marker for JSON that does not identify one document.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_json_preserves_exact_bytes() {
        let body = Bytes::from_static(br#"{ "model": "gpt", "input": "hello" }"#);
        assert_eq!(json_body_if_valid(&body), body);
    }

    #[test]
    fn image_audio_video_file_and_provider_fields_keep_original_bytes() {
        let body = Bytes::from_static(
            br#"{"input":[{"type":"input_image","image_url":"data:image/png;base64,AAAA"},{"type":"input_audio","input_audio":{"data":"BBBB","format":"wav"}},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"DDDD"}},{"type":"input_video","video_url":"data:video/mp4;base64,VIDEO"},{"type":"input_file","file_data":"FILE_BYTES","filename":"notes.pdf"},{"type":"reasoning","encrypted_content":"opaque"}],"output":[{"type":"image_generation_call","result":"CCCC"},{"type":"output_video","data":"VIDEO_BYTES"}],"attachment":"data:application/pdf;base64,FILEPDF","tool":{"data":"ordinary"}}"#,
        );
        assert_eq!(json_body_if_valid(&body), body);
    }

    #[test]
    fn tool_domain_and_typed_media_payloads_keep_original_bytes() {
        let bodies = [
            Bytes::from_static(
                br#"{"type":"tool_use","input":{"encrypted_content":"application value","b64_json":"application value","data":"application value"}}"#,
            ),
            Bytes::from_static(
                br#"{"type":"image","media_type":"application/json","content":"thumbnail label","data":"business value"}"#,
            ),
            Bytes::from_static(
                br#"{"type":"tool_use","input":{"nested":{"type":"reasoning","encrypted_content":"business value"},"message":{"role":"assistant","audio":{"id":"record-1","data":"business value","transcript":"label"}}}}"#,
            ),
            Bytes::from_static(
                br#"{"type":"output_image","content":"caption","data":"IMAGE_BASE64"}"#,
            ),
            Bytes::from_static(
                br#"{"tools":[{"type":"function","name":"flat","parameters":{"properties":{"value":{"default":{"type":"reasoning","encrypted_content":"business"}}}}},{"type":"function","function":{"name":"nested","parameters":{"examples":[{"role":"assistant","audio":{"id":"x","data":"business","transcript":"label"}}]}}},{"name":"anthropic","input_schema":{"examples":[{"type":"reasoning","encrypted_content":"business"}]}}]}"#,
            ),
        ];
        for body in bodies {
            assert_eq!(json_body_if_valid(&body), body);
        }
    }

    #[test]
    fn structured_provider_payload_keeps_original_bytes() {
        let body = Bytes::from(format!(
            "{{\"type\":\"reasoning\",\"encrypted_content\":{{\"parts\":[{}]}}}}",
            (0..4_096)
                .map(|_| "\"opaque\"")
                .collect::<Vec<_>>()
                .join(",")
        ));
        assert_eq!(json_body_if_valid(&body), body);
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
    fn inline_media_urls_keep_original_bytes() {
        let body = Bytes::from_static(
            br#"{"input":[{"type":"input_image","image_url":"DATA:IMAGE/png;base64,SECRET"}]}"#,
        );
        assert_eq!(json_body_if_valid(&body), body);
    }

    #[test]
    fn long_inline_media_keeps_original_bytes() {
        let long_subtype = "x".repeat(16_384);
        let body = Bytes::from(format!(
            "{{\"image\":\"data:image/svg+xml,<svg>SECRET</svg>\",\"audio\":\"data:AUDIO/{long_subtype},SECRET\"}}"
        ));
        assert_eq!(json_body_if_valid(&body), body);
    }

    #[test]
    fn inline_media_in_arrays_and_tool_payloads_keeps_original_bytes() {
        let body = Bytes::from_static(
            br#"{"items":["data:image/png,SECRET"],"tool":{"type":"tool_use","input":["data:audio/wav,OPAQUE"]}}"#,
        );
        assert_eq!(json_body_if_valid(&body), body);
    }

    #[test]
    fn json_escapes_keep_original_bytes() {
        let encrypted =
            Bytes::from_static(br#"{"type":"reasoning","encrypted_\u0063ontent":"SECRET"}"#);
        assert_eq!(json_body_if_valid(&encrypted), encrypted);

        let image = Bytes::from_static(
            br#"{"type":"input_image","image_url":"d\u0061ta:image/png;base64,SECRET"}"#,
        );
        assert_eq!(json_body_if_valid(&image), image);
    }

    #[test]
    fn chat_audio_and_provider_payloads_keep_original_bytes() {
        let body = Bytes::from_static(
            br#"{"choices":[{"message":{"role":"assistant","audio":{"id":"audio-1","expires_at":1,"data":"AUDIO_BASE64","transcript":"hello"}}}],"output":[{"type":"compaction","encrypted_content":"COMPACT_SECRET"}]}"#,
        );
        assert_eq!(json_body_if_valid(&body), body);
    }

    #[test]
    fn sse_archive_keeps_original_provider_frame() {
        let secret = "s".repeat(512);
        let delivered = Bytes::from(format!(
            "event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"compaction\",\"encrypted_content\":\"{secret}\",\"summary\":[{{\"text\":\"visible\"}}]}}}}\n\n"
        ));
        assert_eq!(sse_frame(&delivered), delivered);
    }

    #[test]
    fn sse_archive_keeps_streamed_image_and_audio_bytes() {
        let image_body = "I".repeat(512);
        let partial_image = Bytes::from(format!(
            "event: response.image_generation_call.partial_image\ndata: {{\"type\":\"response.image_generation_call.partial_image\",\"partial_image_b64\":\"{image_body}\",\"partial_image_index\":0}}\n\n"
        ));
        assert_eq!(sse_frame(&partial_image), partial_image);

        let audio_body = "A".repeat(512);
        let audio_delta = Bytes::from(format!(
            "event: response.output_audio.delta\ndata: {{\"type\":\"response.output_audio.delta\",\"delta\":\"{audio_body}\",\"sequence_number\":1}}\n\n"
        ));
        assert_eq!(sse_frame(&audio_delta), audio_delta);
    }

    #[test]
    fn sse_archive_keeps_short_media_payloads() {
        let values = (0..2_048)
            .map(|_| "\"data:image/x,\"")
            .collect::<Vec<_>>()
            .join(",");
        let delivered = Bytes::from(format!("data: {{\"items\":[{values}]}}\n\n"));
        assert_eq!(sse_frame(&delivered), delivered);
    }

    #[test]
    fn reassembled_sse_frame_keeps_short_media_bytes() {
        let delivered = Bytes::from_static(
            b"data: {\"items\":[\"data:image/x,SECRET\"],\"label\":\"visible\"}\n\n",
        );
        let split = delivered
            .windows(b"image".len())
            .position(|window| window == b"image")
            .unwrap()
            + 2;
        let mut framer = crate::api::sse::BoundedSseFramer::default();
        let first = framer.push(&delivered[..split]);
        assert!(first.rejection.is_none());
        assert!(first.events.is_empty());
        let second = framer.push(&delivered[split..]);
        assert!(second.rejection.is_none());
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].bytes, delivered);

        let archived = sse_frame(&second.events[0].bytes);
        assert_eq!(archived, delivered);
    }

    #[test]
    fn sse_archive_keeps_large_provider_frame() {
        let visible = "v".repeat(192 * 1024);
        let delivered = Bytes::from(format!(
            "data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"compaction\",\"encrypted_content\":\"s\",\"summary\":[{{\"text\":\"{visible}\"}}]}}}}\n\n"
        ));
        assert_eq!(sse_frame(&delivered), delivered);
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

    #[test]
    fn short_duplicate_sse_without_space_never_expands() {
        let delivered = Bytes::from_static(b"data:{\"a\":0,\"a\":0}\n\n");
        let archived = sse_frame(&delivered);
        assert_eq!(archived, Bytes::from_static(b"data:null\n\n"));
        assert!(archived.len() <= delivered.len());
    }
}
