use http::{HeaderMap, header};
use serde_json::Value;

use crate::error::AppError;

const COLLABORATION_TOOL_NAMES: &[&str] = &["spawn_agent", "send_message", "followup_task"];

/// CPA applies MultiAgentV2 compatibility only to the official Codex client
/// envelope.  Keep that boundary strict so a normal Responses caller cannot
/// accidentally trigger the agent-message downgrade on a third-party route.
pub(super) fn is_official_codex_user_agent(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::USER_AGENT).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(user_agent) = value.to_str() else {
        return false;
    };
    crate::api::proxy::codex_transport::is_first_party_codex_user_agent(user_agent)
}

/// Only collaboration-bearing request shapes need the declared third-party
/// MultiAgentV2 conversion. An official Codex user agent alone must not alter
/// routing or payload handling for an otherwise ordinary Responses request.
pub(super) fn has_codex_multi_agent_v2_shape(request: &Value) -> bool {
    request
        .get("tools")
        .is_some_and(contains_collaboration_tool)
        || request
            .get("input")
            .and_then(Value::as_array)
            .is_some_and(|input| {
                input.iter().any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("agent_message")
                        || (item.get("type").and_then(Value::as_str) == Some("additional_tools")
                            && item.get("tools").is_some_and(contains_collaboration_tool))
                })
            })
}

fn contains_collaboration_tool(value: &Value) -> bool {
    value.as_array().is_some_and(|tools| {
        tools.iter().any(|tool| {
            (tool.get("type").and_then(Value::as_str) == Some("namespace")
                && tool.get("name").and_then(Value::as_str) == Some("collaboration"))
                || tool
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| COLLABORATION_TOOL_NAMES.contains(&name))
                || tool
                    .get("function")
                    .and_then(|definition| definition.get("name"))
                    .and_then(Value::as_str)
                    .is_some_and(|name| COLLABORATION_TOOL_NAMES.contains(&name))
                || tool.get("tools").is_some_and(contains_collaboration_tool)
        })
    })
}

/// Normalize the subset of Codex MultiAgentV2 request shapes that a declared
/// third-party upstream can read.  The native Codex route deliberately does
/// not call this function. Readable agent messages are lowered to an ordinary
/// user message; opaque or malformed content fails closed before an upstream
/// request can be dispatched.
pub(super) fn normalize_codex_multi_agent_v2(
    request: &mut Value,
    enabled: bool,
) -> Result<(), AppError> {
    if !enabled {
        return Ok(());
    }

    prepare_codex_multi_agent_v2_tools(request, true)?;
    if let Some(input) = request.get_mut("input").and_then(Value::as_array_mut) {
        for item in input {
            if item.get("type").and_then(Value::as_str) == Some("agent_message") {
                rewrite_agent_message(item)?;
            }
        }
    }
    Ok(())
}

/// Remove the encrypted collaboration message marker that third-party
/// compatibility adapters cannot read. Native Codex requests must bypass this
/// conversion entirely and retain their original collaboration schema.
pub(super) fn prepare_codex_multi_agent_v2_tools(
    request: &mut Value,
    enabled: bool,
) -> Result<(), AppError> {
    if !enabled {
        return Ok(());
    }

    if let Some(tools) = request.get_mut("tools") {
        rewrite_tool_list(tools);
    }
    if let Some(input) = request.get_mut("input").and_then(Value::as_array_mut) {
        for item in input {
            if let Some("additional_tools") = item.get("type").and_then(Value::as_str)
                && let Some(tools) = item.get_mut("tools")
            {
                rewrite_tool_list(tools);
            }
        }
    }
    Ok(())
}

fn rewrite_tool_list(value: &mut Value) {
    let Some(tools) = value.as_array_mut() else {
        return;
    };
    for tool in tools {
        if tool.get("type").and_then(Value::as_str) == Some("namespace") {
            if let Some(nested) = tool.get_mut("tools") {
                rewrite_tool_list(nested);
            }
            continue;
        }

        if tool.get("function").is_some() {
            if let Some(definition) = tool.get_mut("function") {
                rewrite_tool_definition(definition);
            }
        } else if tool.get("type").and_then(Value::as_str) == Some("function") {
            rewrite_tool_definition(tool);
        }
    }
}

fn rewrite_tool_definition(definition: &mut Value) {
    let Some(name) = definition.get("name").and_then(Value::as_str) else {
        return;
    };
    if !COLLABORATION_TOOL_NAMES.contains(&name) {
        return;
    }
    let Some(message) = definition.pointer_mut("/parameters/properties/message") else {
        return;
    };
    if let Some(message) = message.as_object_mut() {
        // Keep the rest of the schema and all tool metadata intact.  The
        // upstream must not be told that this field carries an internal
        // encrypted representation it cannot consume.
        message.remove("encrypted");
    }
}

fn malformed_agent_message() -> AppError {
    AppError::BadRequest(
        "third-party Codex MultiAgentV2 agent_message must contain readable content".into(),
    )
}

fn rewrite_agent_message(item: &mut Value) -> Result<(), AppError> {
    let content = item
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(malformed_agent_message)?;
    if content.is_empty() {
        return Err(malformed_agent_message());
    }

    let mut sanitized_content = Vec::with_capacity(content.len());
    for mut part in content {
        if part.get("type").and_then(Value::as_str) == Some("encrypted_content") {
            let text = part
                .get("encrypted_content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
                .ok_or_else(malformed_agent_message)?;
            let Some(part) = part.as_object_mut() else {
                return Err(malformed_agent_message());
            };
            part.insert("type".into(), Value::String("input_text".into()));
            part.insert("text".into(), Value::String(text));
            part.remove("encrypted_content");
        }

        let sanitized = match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => {
                let text = part
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .ok_or_else(malformed_agent_message)?;
                serde_json::json!({"type":"input_text", "text":text})
            }
            Some("input_image") => {
                let image_url = part
                    .get("image_url")
                    .and_then(Value::as_str)
                    .filter(|image_url| !image_url.is_empty())
                    .ok_or_else(malformed_agent_message)?;
                let mut sanitized = serde_json::json!({
                    "type": "input_image",
                    "image_url": image_url,
                });
                if let Some(detail) = part.get("detail").filter(|detail| detail.is_string()) {
                    sanitized["detail"] = detail.clone();
                }
                sanitized
            }
            _ => return Err(malformed_agent_message()),
        };
        sanitized_content.push(sanitized);
    }

    // An agent envelope is an internal transport shape.  Once it becomes a
    // normal user message, retain only the standard message fields and the
    // allow-listed content fields above; author/recipient and passthrough
    // metadata must never reach a strict third-party upstream.
    if let Some(item) = item.as_object_mut() {
        item.clear();
        item.insert("type".into(), Value::String("message".into()));
        item.insert("role".into(), Value::String("user".into()));
        item.insert("content".into(), Value::Array(sanitized_content));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use serde_json::json;

    #[test]
    fn official_codex_user_agent_boundary_matches_cpa_and_fails_closed() {
        for user_agent in [
            "Codex Desktop/1.2.3",
            "Codex Work/0.154.0 (Linux; x86_64)",
            "Codex Work/0.154.0-dev",
            "codex-tui/0.1.0",
            "codex_vscode/0.154.0",
            "codex_atlas/0.154.0",
            "codex_chatgpt_desktop/0.154.0",
            "codex-chrome-extension-sidepanel/0.154.0",
            "codex-chrome-extension-sidepanel/0.154.0-dev",
            "codex_cli_rs",
            "codex_cli_rs/0.1.0",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::USER_AGENT, HeaderValue::from_static(user_agent));
            assert!(is_official_codex_user_agent(&headers), "{user_agent}");
        }
        for user_agent in [
            "Mozilla/5.0",
            "Codex",
            "Codex /0.154.0",
            "Codex Work",
            "Codex Work/not-a-version",
            "codex-chrome-extension-sidepanel",
            "codex-chrome-extension-sidepanel/not-a-version",
            "codex_vscode/1junk",
            "codex_cli_rs-other",
            "codex_vscode",
            "codex_vscode-not-versioned",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::USER_AGENT, HeaderValue::from_static(user_agent));
            assert!(!is_official_codex_user_agent(&headers), "{user_agent}");
        }
        let mut duplicate = HeaderMap::new();
        duplicate.append(
            header::USER_AGENT,
            HeaderValue::from_static("Codex Desktop/1.2.3"),
        );
        duplicate.append(
            header::USER_AGENT,
            HeaderValue::from_static("Codex Desktop/4.5.6"),
        );
        assert!(!is_official_codex_user_agent(&duplicate));
    }

    #[test]
    fn multi_agent_shape_requires_agent_message_or_collaboration_tool() {
        for request in [
            json!({"input":"ordinary request"}),
            json!({"tools":[{"type":"function","name":"ordinary"}]}),
            json!({"input":[{"type":"additional_tools","tools":[{"type":"function","name":"ordinary"}]}]}),
        ] {
            assert!(!has_codex_multi_agent_v2_shape(&request));
        }
        for request in [
            json!({"input":[{"type":"agent_message","content":[]}]}),
            json!({"tools":[{"type":"function","name":"spawn_agent"}]}),
            json!({"tools":[{"type":"namespace","name":"collaboration","tools":[]}]}),
            json!({"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"collaboration","tools":[]}]}]}),
        ] {
            assert!(has_codex_multi_agent_v2_shape(&request));
        }
    }

    #[test]
    fn disabled_normalization_leaves_native_shape_untouched() {
        let mut request = json!({
            "tools": [{"type":"function","name":"spawn_agent","parameters":{
                "type":"object","properties":{"message":{"type":"string","encrypted":{"type":"boolean"}}}
            }}],
            "input": [{"type":"agent_message","role":"system","content":[
                {"type":"encrypted_content","encrypted_content":"opaque"}
            ]}]
        });
        let original = request.clone();
        normalize_codex_multi_agent_v2(&mut request, false).unwrap();
        assert_eq!(request, original);
    }

    #[test]
    fn collaboration_schema_rewrite_covers_nested_and_additional_tools_only() {
        let mut request = json!({
            "tools": [
                {"type":"function","name":"spawn_agent","x-meta":"keep","parameters":{
                    "type":"object","properties":{"message":{"type":"string","encrypted":{"type":"boolean"},"description":"keep"},"other":{"type":"string"}}
                }},
                {"type":"function","name":"unrelated","parameters":{
                    "type":"object","properties":{"message":{"type":"string","encrypted":{"type":"boolean"}}}
                }},
                {"type":"namespace","name":"delegation","tools":[
                    {"type":"function","name":"send_message","parameters":{
                        "type":"object","properties":{"message":{"type":"string","encrypted":true}}
                    }}
                ]}
            ],
            "input": [{"type":"additional_tools","tools":[
                {"type":"function","name":"followup_task","parameters":{
                    "type":"object","properties":{"message":{"type":"string","encrypted":false}}
                }}
            ]}]
        });
        normalize_codex_multi_agent_v2(&mut request, true).unwrap();

        assert!(
            request["tools"][0]["parameters"]["properties"]["message"]
                .get("encrypted")
                .is_none()
        );
        assert_eq!(request["tools"][0]["x-meta"], "keep");
        assert_eq!(
            request["tools"][0]["parameters"]["properties"]["message"]["description"],
            "keep"
        );
        assert!(
            request["tools"][2]["tools"][0]["parameters"]["properties"]["message"]
                .get("encrypted")
                .is_none()
        );
        assert!(
            request["input"][0]["tools"][0]["parameters"]["properties"]["message"]
                .get("encrypted")
                .is_none()
        );
        assert!(
            request["tools"][1]["parameters"]["properties"]["message"]
                .get("encrypted")
                .is_some()
        );
    }

    #[test]
    fn parent_carrier_preparation_strips_schema_without_downgrading_agent_message() {
        let mut request = json!({
            "tools": [{"type":"function","name":"spawn_agent","parameters":{
                "type":"object","properties":{"message":{"type":"string","encrypted":{"type":"boolean"}}}
            }}],
            "input": [{"type":"agent_message","role":"system",
                "internal_chat_message_metadata_passthrough":{"turn_id":"keep-for-native"},
                "content":[{"type":"encrypted_content","encrypted_content":"delegated task"}]
            }]
        });

        prepare_codex_multi_agent_v2_tools(&mut request, true).unwrap();

        assert!(
            request["tools"][0]["parameters"]["properties"]["message"]
                .get("encrypted")
                .is_none()
        );
        assert_eq!(request["input"][0]["type"], "agent_message");
        assert_eq!(request["input"][0]["role"], "system");
        assert_eq!(
            request["input"][0]["internal_chat_message_metadata_passthrough"]["turn_id"],
            "keep-for-native"
        );
        assert_eq!(
            request["input"][0]["content"][0]["type"],
            "encrypted_content"
        );
    }

    #[test]
    fn readable_agent_message_lowers_to_user_without_promoting_role() {
        let mut request = json!({
            "input": [{"type":"agent_message","role":"system",
                "internal_chat_message_metadata_passthrough":{"turn_id":"turn"},
                "content":[
                    {"type":"input_text","text":"prefix"},
                    {"type":"encrypted_content","encrypted_content":"delegated task","trace":"keep"}
                ]
            }]
        });
        normalize_codex_multi_agent_v2(&mut request, true).unwrap();

        assert_eq!(request["input"][0]["type"], "message");
        assert_eq!(request["input"][0]["role"], "user");
        assert!(
            request["input"][0]
                .get("internal_chat_message_metadata_passthrough")
                .is_none()
        );
        assert!(request["input"][0].get("author").is_none());
        assert!(request["input"][0].get("recipient").is_none());
        assert_eq!(request["input"][0]["content"][1]["type"], "input_text");
        assert_eq!(request["input"][0]["content"][1]["text"], "delegated task");
        assert!(
            request["input"][0]["content"][1]
                .get("encrypted_content")
                .is_none()
        );
        assert!(request["input"][0]["content"][1].get("trace").is_none());
    }

    #[test]
    fn opaque_agent_message_is_rejected_without_mutating_native_shape() {
        let mut request = json!({
            "input": [{"type":"agent_message","role":"system", "content":[
                {"type":"input_text","text":"prefix"},
                {"type":"encrypted_content","encrypted_content":{"ciphertext":"opaque"}}
            ]}]
        });
        assert!(normalize_codex_multi_agent_v2(&mut request, true).is_err());

        assert_eq!(request["input"][0]["type"], "agent_message");
        assert_eq!(request["input"][0]["role"], "system");
        assert_eq!(
            request["input"][0]["content"][1]["encrypted_content"]["ciphertext"],
            "opaque"
        );
    }

    #[test]
    fn empty_or_malformed_agent_messages_fail_closed() {
        for item in [
            json!({"type":"agent_message","content":[]}),
            json!({"type":"agent_message"}),
            json!({"type":"agent_message","content":[{"type":"future_content"}]}),
            json!({"type":"agent_message","content":[{"type":"input_text","text":""}]}),
        ] {
            let mut request = json!({"input":[item]});
            assert!(normalize_codex_multi_agent_v2(&mut request, true).is_err());
        }
    }
}
