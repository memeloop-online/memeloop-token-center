//! New API and opted-in HTTP JSON relays: map Codex traffic onto compact endpoints.
//!
//! Compact v2 (`compaction_trigger` on `/v1/responses`) is bridged to
//! `POST /v1/responses/compact` with Responses `input`, then wrapped back into
//! a Responses envelope. Generic HTTP JSON accounts need an explicit
//! `responses_compact_v2_bridge` opt-in; New API keeps its built-in contract.
use super::AppError;
use crate::api::kimi_transport::responses::compaction_item;
use bytes::Bytes;
use serde_json::{Value, json};

const COMPACT_PATH: &str = "/v1/responses/compact";
const ALPHA_SEARCH_PATH: &str = "/v1/alpha/search";
#[cfg(test)]
const GEMINI_GENERATE_PATH_PREFIX: &str = "/v1beta/models/";

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
    require_completed_status(compact)?;
    let output = compact
        .get("output")
        .and_then(Value::as_array)
        .filter(|output| !output.is_empty())
        .ok_or("compaction_item_missing")?;
    let native = output.iter().any(|item| item["type"] == "compaction");
    let mut translated = Vec::with_capacity(output.len());
    for (index, item) in output.iter().enumerate() {
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())
            .ok_or("output_item_malformed")?;
        require_completed_status(item)?;
        if kind == "compaction"
            && item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Err("compaction_item_malformed");
        }
        if !native
            && kind == "message"
            && let Some(text) = message_text(item)
        {
            let id = compact["id"].as_str().unwrap_or("compact");
            let checkpoint_id = if output.len() == 1 {
                format!("cmp_{id}")
            } else {
                format!("cmp_{id}_{index}")
            };
            translated.push(compaction_item(&checkpoint_id, &text));
        } else {
            translated.push(item.clone());
        }
    }
    let id = compact
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp_compact");
    let mut response = compact.clone();
    response["id"] = json!(id);
    response["object"] = json!("response");
    response["status"] = json!("completed");
    response["error"] = Value::Null;
    response["output"] = Value::Array(translated);
    Ok(response)
}

fn require_completed_status(value: &Value) -> Result<(), &'static str> {
    if value
        .get("incomplete_details")
        .is_some_and(|details| !details.is_null())
    {
        return Err("provider_not_completed");
    }
    match value.get("status") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(status)) if status == "completed" => Ok(()),
        _ => Err("provider_not_completed"),
    }
}

fn message_text(item: &Value) -> Option<String> {
    let content = item.get("content")?;
    if let Some(text) = content.as_str() {
        return (!text.is_empty()).then(|| text.to_owned());
    }
    let mut text = String::new();
    for part in content.as_array()? {
        if !matches!(
            part.get("type").and_then(Value::as_str),
            None | Some("output_text" | "input_text" | "text")
        ) {
            return None;
        }
        text.push_str(part.get("text")?.as_str()?);
    }
    (!text.is_empty()).then_some(text)
}

pub(in crate::api) fn responses_to_sse(response: &Value) -> Result<Bytes, &'static str> {
    require_completed_status(response)?;
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .filter(|output| !output.is_empty())
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
    for (index, item) in output.iter().enumerate() {
        push_sse(
            &mut body,
            "response.output_item.added",
            json!({"type":"response.output_item.added","output_index":index,"item":item}),
        )?;
        push_sse(
            &mut body,
            "response.output_item.done",
            json!({"type":"response.output_item.done","output_index":index,"item":item}),
        )?;
    }
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
    use base64::Engine as _;

    fn assert_sse_preserves_output(response: &Value) {
        let sse = String::from_utf8(responses_to_sse(response).unwrap().to_vec()).unwrap();
        let events: Vec<Value> = sse
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
        let output = response["output"].as_array().unwrap();
        assert_eq!(events.len(), 2 + output.len() * 2);
        assert_eq!(events[0]["type"], "response.created");
        assert_eq!(events[0]["response"]["output"], json!([]));
        for (index, item) in output.iter().enumerate() {
            let added = &events[1 + index * 2];
            let done = &events[2 + index * 2];
            assert_eq!(added["type"], "response.output_item.added");
            assert_eq!(done["type"], "response.output_item.done");
            for event in [added, done] {
                assert_eq!(event["output_index"], json!(index));
                assert_eq!(&event["item"], item);
            }
        }
        let completed = events.last().unwrap();
        assert_eq!(completed["type"], "response.completed");
        assert_eq!(&completed["response"], response);
    }

    fn checkpoint_text(item: &Value) -> String {
        let encoded = item["encrypted_content"]
            .as_str()
            .unwrap()
            .strip_prefix("mtc-compact-v1.")
            .unwrap();
        String::from_utf8(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn compact_preserves_native_mixed_output_and_forward_metadata_in_order() {
        let compact = json!({
            "id": "compact-mixed",
            "object": "response.compaction",
            "future_metadata": {"revision": 9},
            "output": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"before"}]},
                {"id":"c1","type":"compaction","encrypted_content":"first","future_metadata":true},
                {"id":"m1","type":"message","role":"assistant","status":"completed","content":[
                    {"type":"output_text","text":"middle ","annotations":[]},
                    {"type":"output_text","text":"完整文本","annotations":[]}
                ]},
                {"id":"c2","type":"compaction","encrypted_content":"second"},
                {"type":"future_output","opaque":{"text":"keep me"}}
            ],
            "usage": {"input_tokens": 9, "output_tokens": 2}
        });
        let response = compact_to_responses(&compact).unwrap();
        assert_eq!(response["output"], compact["output"]);
        assert_eq!(response["future_metadata"], compact["future_metadata"]);
        assert_eq!(response["usage"], compact["usage"]);
        assert_sse_preserves_output(&response);
    }

    #[test]
    fn compact_preserves_multiple_native_checkpoints() {
        let compact = json!({"output": [
            {"type":"compaction","encrypted_content":"first"},
            {"type":"compaction","encrypted_content":"second"}
        ]});
        let response = compact_to_responses(&compact).unwrap();
        assert_eq!(response["output"], compact["output"]);
        assert_sse_preserves_output(&response);
    }

    #[test]
    fn compact_translates_every_legacy_message_and_text_part_in_order() {
        let compact = json!({"id":"legacy", "output": [
            {"type":"message","content":[
                {"type":"output_text","text":"first ","future_metadata":true},
                {"type":"output_text","text":"完整文本"}
            ]},
            {"type":"reasoning","summary":[{"type":"summary_text","text":"keep reasoning"}]},
            {"type":"message","content":"second"}
        ]});
        let response = compact_to_responses(&compact).unwrap();
        assert_eq!(response["output"].as_array().unwrap().len(), 3);
        assert_eq!(checkpoint_text(&response["output"][0]), "first 完整文本");
        assert_eq!(response["output"][1], compact["output"][1]);
        assert_eq!(checkpoint_text(&response["output"][2]), "second");
        assert_ne!(response["output"][0]["id"], response["output"][2]["id"]);
        assert_sse_preserves_output(&response);
    }

    #[test]
    fn compact_preserves_message_parts_that_cannot_be_encoded_as_plain_text() {
        for part in [
            json!({"type":"refusal","refusal":"cannot answer"}),
            json!({"type":"future_part","text":"visible","opaque":"keep me"}),
        ] {
            let compact = json!({"output": [{"type":"message","content":[
                {"type":"output_text","text":"prefix"}, part
            ]}]});
            let response = compact_to_responses(&compact).unwrap();
            assert_eq!(response["output"], compact["output"]);
            assert_sse_preserves_output(&response);
        }
    }

    #[test]
    fn compact_preserves_empty_messages_without_inventing_checkpoints() {
        for content in [
            json!(""),
            json!([]),
            json!([{"type":"output_text","text":""}]),
        ] {
            let compact = json!({"output":[{"type":"message","content":content}]});
            let response = compact_to_responses(&compact).unwrap();
            assert_eq!(response["output"], compact["output"]);
            assert_sse_preserves_output(&response);
        }
    }

    #[test]
    fn compact_never_completes_failed_incomplete_or_nonterminal_results() {
        let output = json!([{"type":"compaction","encrypted_content":"opaque"}]);
        for status in ["failed", "incomplete", "in_progress", "queued", "cancelled"] {
            let compact = json!({"status":status,"output":output});
            assert_eq!(
                compact_to_responses(&compact),
                Err("provider_not_completed")
            );
            assert_eq!(responses_to_sse(&compact), Err("provider_not_completed"));
            for kind in ["message", "compaction", "function_call"] {
                let compact = json!({"output":[{
                    "type":kind,"status":status,"content":"partial","encrypted_content":"opaque"
                }]});
                assert_eq!(
                    compact_to_responses(&compact),
                    Err("provider_not_completed")
                );
            }
        }
        assert_eq!(
            compact_to_responses(&json!({
                "output":output,"incomplete_details":{"reason":"max_output_tokens"}
            })),
            Err("provider_not_completed")
        );
        assert_eq!(
            compact_to_responses(&json!({
                "output":output,"error":{"code":"provider_error"}
            })),
            Err("provider_error")
        );
    }

    #[test]
    fn compact_rejects_unrepresentable_core_fields_without_restricting_metadata() {
        for output in [json!([]), Value::Null, json!("not an array")] {
            assert_eq!(
                compact_to_responses(&json!({"output":output})),
                Err("compaction_item_missing")
            );
        }
        for encrypted in [Value::Null, json!(""), json!(7)] {
            assert_eq!(
                compact_to_responses(&json!({"output":[{
                    "type":"compaction","encrypted_content":encrypted
                }]})),
                Err("compaction_item_malformed")
            );
        }
        for item in [Value::Null, json!({}), json!({"type":7})] {
            assert_eq!(
                compact_to_responses(&json!({"output":[item]})),
                Err("output_item_malformed")
            );
        }
    }

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
        assert_eq!(compact_path(), "/v1/responses/compact");
        assert_eq!(alpha_search_path(), "/v1/alpha/search");
        assert_eq!(
            gemini_generate_path("gemini-2.5-pro"),
            "/v1beta/models/gemini-2.5-pro:generateContent"
        );
    }
}
