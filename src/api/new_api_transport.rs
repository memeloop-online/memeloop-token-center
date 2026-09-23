//! New API / ZeroCat relay: map Codex traffic onto the documented endpoints.
//!
//! Sidebar types on ZeroCat (`openai`, `openai-response`,
//! `openai-response-compact`, `anthropic`, `openai-alpha-search`) correspond to
//! these upstream paths. Compact v2 (`compaction_trigger` on `/v1/responses`)
//! is bridged to `POST /v1/responses/compact` with Responses `input`, then
//! wrapped back into a Responses envelope Codex can consume.
use super::AppError;
use crate::api::kimi_transport::responses::compaction_item;
use bytes::Bytes;
use serde_json::{Value, json};

const COMPACT_PATH: &str = "/v1/responses/compact";
const ALPHA_SEARCH_PATH: &str = "/v1/alpha/search";
const GEMINI_GENERATE_PATH_PREFIX: &str = "/v1beta/models/";

pub(in crate::api) fn is_driver(driver: &str) -> bool {
    driver == "new-api"
}

pub(in crate::api) fn compact_path() -> &'static str {
    COMPACT_PATH
}

pub(in crate::api) fn alpha_search_path() -> &'static str {
    ALPHA_SEARCH_PATH
}

/// Gemini generateContent path used by New API's `gemini` endpoint type.
#[cfg(test)]
pub(in crate::api) fn gemini_generate_path(model: &str) -> String {
    format!("{GEMINI_GENERATE_PATH_PREFIX}{model}:generateContent")
}

pub(in crate::api) fn has_compaction_trigger(request: &Value) -> bool {
    request["input"].as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item["type"] == "compaction_trigger")
    })
}

/// Strip the v2 trigger and fields New API does not forward on compact.
pub(in crate::api) fn prepare_compact_request(
    request: &Value,
    upstream_model: &str,
) -> Result<Value, AppError> {
    let mut forwarded = json!({
        "model": upstream_model,
        "stream": false,
    });
    let input = match &request["input"] {
        Value::String(text) => vec![json!({"role":"user","content":text})],
        Value::Array(items) => items
            .iter()
            .filter(|item| item["type"] != "compaction_trigger")
            .cloned()
            .collect(),
        _ => {
            return Err(AppError::BadRequest(
                "Responses compact input must be a string or array".into(),
            ));
        }
    };
    if input.is_empty() {
        return Err(AppError::BadRequest(
            "Responses compact requires conversation input besides compaction_trigger".into(),
        ));
    }
    forwarded["input"] = Value::Array(input);
    for name in [
        "instructions",
        "previous_response_id",
        "parallel_tool_calls",
        "service_tier",
        "prompt_cache_key",
        "prompt_cache_options",
        "prompt_cache_retention",
    ] {
        if let Some(value) = request.get(name) {
            forwarded[name] = value.clone();
        }
    }
    Ok(forwarded)
}

pub(in crate::api) fn compact_to_responses(compact: &Value) -> Result<Value, &'static str> {
    if compact.get("error").is_some_and(|error| !error.is_null()) {
        return Err("provider_error");
    }
    let output = compact
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut compaction_items: Vec<Value> = output
        .iter()
        .filter(|item| item["type"] == "compaction")
        .cloned()
        .collect();
    if compaction_items.is_empty() {
        let text = output
            .iter()
            .filter_map(|item| {
                item["content"]
                    .as_array()
                    .and_then(|parts| parts.last())
                    .and_then(|part| part["text"].as_str())
                    .or_else(|| item["content"].as_str())
            })
            .find(|text| !text.is_empty());
        let Some(text) = text else {
            return Err("compaction_item_missing");
        };
        let id = compact["id"].as_str().unwrap_or("compact");
        compaction_items.push(compaction_item(&format!("cmp_{id}"), text));
    } else if compaction_items.len() > 1 {
        compaction_items = vec![compaction_items.pop().ok_or("compaction_item_missing")?];
    }
    let id = compact
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp_compact");
    let mut response = json!({
        "id": id,
        "object": "response",
        "status": "completed",
        "error": null,
        "output": compaction_items,
    });
    if let Some(usage) = compact.get("usage") {
        response["usage"] = usage.clone();
    }
    Ok(response)
}

pub(in crate::api) fn responses_to_sse(response: &Value) -> Result<Bytes, &'static str> {
    let item = response
        .get("output")
        .and_then(Value::as_array)
        .and_then(|output| output.first())
        .cloned()
        .ok_or("compaction_item_missing")?;
    let mut created = response.clone();
    created["status"] = json!("in_progress");
    created["output"] = json!([]);
    let mut body = Vec::new();
    push_sse(
        &mut body,
        "response.created",
        json!({"type":"response.created","response":created}),
    )?;
    push_sse(
        &mut body,
        "response.output_item.added",
        json!({"type":"response.output_item.added","output_index":0,"item":item}),
    )?;
    push_sse(
        &mut body,
        "response.output_item.done",
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
    )?;
    push_sse(
        &mut body,
        "response.completed",
        json!({"type":"response.completed","response":response}),
    )?;
    Ok(Bytes::from(body))
}

fn push_sse(body: &mut Vec<u8>, event: &str, data: Value) -> Result<(), &'static str> {
    let payload = serde_json::to_vec(&data).map_err(|_| "event_serialization")?;
    body.extend_from_slice(b"event: ");
    body.extend_from_slice(event.as_bytes());
    body.extend_from_slice(b"\ndata: ");
    body.extend_from_slice(&payload);
    body.extend_from_slice(b"\n\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_request_drops_trigger_and_tools() {
        let request = json!({
            "model": "public",
            "stream": true,
            "tools": [{"type":"function","name":"exec"}],
            "reasoning": {"effort":"high"},
            "text": {"format":{"type":"text"}},
            "instructions": "system",
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"earlier"}]},
                {"type":"compaction_trigger"}
            ]
        });
        let forwarded = prepare_compact_request(&request, "gpt-5.6-terra").unwrap();
        assert_eq!(forwarded["model"], "gpt-5.6-terra");
        assert_eq!(forwarded["stream"], false);
        assert!(forwarded.get("tools").is_none());
        assert!(forwarded.get("max_output_tokens").is_none());
        assert!(forwarded.get("reasoning").is_none());
        let input = forwarded["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
    }

    #[test]
    fn compact_to_responses_keeps_native_checkpoint() {
        let compact = json!({
            "id": "cmp_1",
            "output": [{"id":"c1","type":"compaction","encrypted_content":"opaque"}],
            "usage": {"input_tokens": 9, "output_tokens": 2}
        });
        let response = compact_to_responses(&compact).unwrap();
        assert_eq!(response["status"], "completed");
        assert_eq!(response["output"][0]["encrypted_content"], "opaque");
        assert_eq!(response["usage"]["input_tokens"], 9);
        let sse = String::from_utf8(responses_to_sse(&response).unwrap().to_vec()).unwrap();
        assert!(sse.contains("response.output_item.done"));
        assert!(sse.contains("compaction"));
    }

    #[test]
    fn driver_and_paths_match_new_api_sidebar() {
        assert!(is_driver("new-api"));
        assert_eq!(compact_path(), "/v1/responses/compact");
        assert_eq!(alpha_search_path(), "/v1/alpha/search");
        assert_eq!(
            gemini_generate_path("gemini-2.5-pro"),
            "/v1beta/models/gemini-2.5-pro:generateContent"
        );
    }
}
