use http::{HeaderMap, header};
use serde_json::Value;

use crate::error::AppError;

const COLLABORATION_TOOL_NAMES: &[&str] = &["spawn_agent", "send_message", "followup_task"];

/// MultiAgent compatibility applies only to the official Codex client
/// envelope. Keep that boundary strict so a normal Responses caller cannot
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

    prepare_plaintext_collaboration_tools(request, true)?;
    if let Some(input) = request.get_mut("input").and_then(Value::as_array_mut) {
        let mut normalized = Vec::with_capacity(input.len());
        let mut last_opaque_agent = None;
        let mut last_readable_user_or_agent = None;
        for (index, mut item) in input.clone().into_iter().enumerate() {
            if item.get("type").and_then(Value::as_str) == Some("agent_message") {
                if rewrite_agent_message(&mut item)? {
                    last_readable_user_or_agent = Some(index);
                    normalized.push(item);
                } else {
                    last_opaque_agent = Some(index);
                }
            } else {
                if has_readable_user_message(&item) {
                    last_readable_user_or_agent = Some(index);
                }
                normalized.push(item);
            }
        }
        if last_opaque_agent.is_some_and(|opaque| {
            last_readable_user_or_agent.is_none_or(|readable| readable < opaque)
        }) {
            return Err(malformed_agent_message());
        }
        *input = normalized;
    }
    Ok(())
}

/// Ask the parent model to emit readable collaboration task arguments. The
/// rewrite is intentionally limited to message-bearing functions inside the
/// collaboration namespace. Other tool schemas and opaque child messages are
/// left intact.
pub(super) fn prepare_plaintext_collaboration_tools(
    request: &mut Value,
    enabled: bool,
) -> Result<(), AppError> {
    if !enabled {
        return Ok(());
    }

    if let Some(tools) = request.get_mut("tools") {
        rewrite_tool_list(tools, false);
    }
    if let Some(input) = request.get_mut("input").and_then(Value::as_array_mut) {
        for item in input {
            if let Some("additional_tools") = item.get("type").and_then(Value::as_str)
                && let Some(tools) = item.get_mut("tools")
            {
                rewrite_tool_list(tools, false);
            }
        }
    }
    Ok(())
}

fn rewrite_tool_list(value: &mut Value, in_collaboration_namespace: bool) {
    let Some(tools) = value.as_array_mut() else {
        return;
    };
    for tool in tools {
        if tool.get("type").and_then(Value::as_str) == Some("namespace") {
            let nested_is_collaboration =
                tool.get("name").and_then(Value::as_str) == Some("collaboration");
            if let Some(nested) = tool.get_mut("tools") {
                rewrite_tool_list(
                    nested,
                    in_collaboration_namespace || nested_is_collaboration,
                );
            }
            continue;
        }

        if !in_collaboration_namespace {
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

fn has_readable_user_message(item: &Value) -> bool {
    if !matches!(
        item.get("type").and_then(Value::as_str),
        None | Some("message")
    ) || item.get("role").and_then(Value::as_str) != Some("user")
    {
        return false;
    }
    match item.get("content") {
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(parts)) => {
            parts
                .iter()
                .any(|part| match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => part
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.trim().is_empty()),
                    Some("input_image") => part
                        .get("image_url")
                        .and_then(Value::as_str)
                        .is_some_and(|url| !url.trim().is_empty()),
                    _ => false,
                })
        }
        _ => false,
    }
}

fn agent_envelope_has_empty_payload(text: &str) -> bool {
    text.starts_with("Message Type:")
        && text
            .split_once("Payload:")
            .is_some_and(|(_, payload)| payload.trim().is_empty())
}

fn rewrite_agent_message(item: &mut Value) -> Result<bool, AppError> {
    let content = item
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(malformed_agent_message)?;
    if content.is_empty() {
        return Err(malformed_agent_message());
    }

    let carries_encrypted_content = content
        .iter()
        .any(|part| part.get("type").and_then(Value::as_str) == Some("encrypted_content"));
    let mut sanitized_content = Vec::with_capacity(content.len());
    for part in content {
        if part.get("type").and_then(Value::as_str) == Some("encrypted_content") {
            continue;
        }

        let sanitized = match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => {
                let text = part
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                    .ok_or_else(malformed_agent_message)?;
                if carries_encrypted_content && agent_envelope_has_empty_payload(text) {
                    continue;
                }
                serde_json::json!({"type":"input_text", "text":text})
            }
            Some("input_image") => {
                let image_url = part
                    .get("image_url")
                    .and_then(Value::as_str)
                    .filter(|image_url| !image_url.trim().is_empty())
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

    if sanitized_content.is_empty() {
        return if carries_encrypted_content {
            Ok(false)
        } else {
            Err(malformed_agent_message())
        };
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
    Ok(true)
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
            "codex_exec/0.154.0",
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
            "codex_exec",
            "codex_exec/not-a-version",
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
            "tools": [{"type":"namespace","name":"collaboration","tools":[{
                "type":"function","name":"spawn_agent","parameters":{
                    "type":"object","properties":{"message":{"type":"string","encrypted":{"type":"boolean"}}}
                }
            }]}],
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
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"namespace","name":"nested","tools":[
                        {"type":"function","name":"spawn_agent","x-meta":"keep","parameters":{
                            "type":"object","properties":{"message":{"type":"string","encrypted":{"type":"boolean"},"description":"keep"},"other":{"type":"string"}}
                        }}
                    ]},
                    {"type":"function","name":"send_message","parameters":{
                        "type":"object","properties":{"message":{"type":"string","encrypted":true}}
                    }}
                ]},
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
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"namespace","name":"nested","tools":[
                        {"type":"function","name":"followup_task","parameters":{
                            "type":"object","properties":{"message":{"type":"string","encrypted":false}}
                        }}
                    ]}
                ]}
            ]}]
        });
        normalize_codex_multi_agent_v2(&mut request, true).unwrap();

        assert!(
            request["tools"][0]["tools"][0]["tools"][0]["parameters"]["properties"]["message"]
                .get("encrypted")
                .is_none()
        );
        assert_eq!(request["tools"][0]["tools"][0]["name"], "nested");
        assert_eq!(
            request["tools"][0]["tools"][0]["tools"][0]["x-meta"],
            "keep"
        );
        assert_eq!(
            request["tools"][0]["tools"][0]["tools"][0]["parameters"]["properties"]["message"]["description"],
            "keep"
        );
        assert!(
            request["tools"][0]["tools"][1]["parameters"]["properties"]["message"]
                .get("encrypted")
                .is_none()
        );
        assert!(
            request["input"][0]["tools"][0]["tools"][0]["tools"][0]["parameters"]["properties"]["message"]
                .get("encrypted")
                .is_none()
        );
        assert!(
            request["tools"][1]["parameters"]["properties"]["message"]
                .get("encrypted")
                .is_some()
        );
        assert!(
            request["tools"][2]["tools"][0]["parameters"]["properties"]["message"]
                .get("encrypted")
                .is_some()
        );
    }

    #[test]
    fn parent_carrier_preparation_strips_schema_without_downgrading_agent_message() {
        let mut request = json!({
            "tools": [{"type":"namespace","name":"collaboration","x-namespace":"keep","tools":[
                {"type":"function","name":"spawn_agent","description":"keep","parameters":{
                    "type":"object","properties":{"message":{"type":"string","encrypted":{"type":"boolean"},"description":"task"}}
                }}
            ]}],
            "input": [{"type":"agent_message","role":"system",
                "internal_chat_message_metadata_passthrough":{"turn_id":"keep-for-native"},
                "content":[{"type":"encrypted_content","encrypted_content":"opaque-ciphertext"}]
            }]
        });
        let original_input = request["input"].clone();

        prepare_plaintext_collaboration_tools(&mut request, true).unwrap();

        let message = request
            .pointer("/tools/0/tools/0/parameters/properties/message")
            .expect("collaboration spawn_agent message schema exists");
        assert_eq!(message["type"], "string");
        assert_eq!(message["description"], "task");
        assert!(message.get("encrypted").is_none());
        assert_eq!(request["tools"][0]["name"], "collaboration");
        assert_eq!(request["tools"][0]["x-namespace"], "keep");
        assert_eq!(request["tools"][0]["tools"][0]["description"], "keep");
        assert_eq!(request["input"], original_input);
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
                    {"type":"input_text","text":"delegated task"},
                    {"type":"encrypted_content","encrypted_content":"opaque-ciphertext","trace":"keep"}
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
        assert_eq!(request["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(request["input"][0]["content"][0]["text"], "delegated task");
        assert!(
            request["input"][0]["content"][0]
                .get("encrypted_content")
                .is_none()
        );
        assert!(request["input"][0]["content"][0].get("trace").is_none());
        assert!(!request.to_string().contains("opaque-ciphertext"));
    }

    #[test]
    fn opaque_agent_part_is_omitted_when_readable_agent_content_remains() {
        let mut request = json!({
            "input": [{"type":"agent_message","role":"system", "content":[
                {"type":"input_text","text":"prefix"},
                {"type":"encrypted_content","encrypted_content":{"ciphertext":"opaque"}}
            ]}]
        });
        normalize_codex_multi_agent_v2(&mut request, true).unwrap();

        assert_eq!(request["input"][0]["type"], "message");
        assert_eq!(request["input"][0]["role"], "user");
        assert_eq!(
            request["input"][0]["content"],
            json!([{"type":"input_text","text":"prefix"}])
        );
    }

    #[test]
    fn empty_or_malformed_agent_messages_fail_closed() {
        for item in [
            json!({"type":"agent_message","content":[]}),
            json!({"type":"agent_message"}),
            json!({"type":"agent_message","content":[{"type":"future_content"}]}),
            json!({"type":"agent_message","content":[{"type":"input_text","text":""}]}),
            json!({"type":"agent_message","content":[
                {"type":"encrypted_content","encrypted_content":{"ciphertext":"opaque"}}
            ]}),
        ] {
            let mut request = json!({"input":[item]});
            assert!(normalize_codex_multi_agent_v2(&mut request, true).is_err());
        }
    }

    #[test]
    fn opaque_agent_history_is_omitted_only_when_followed_by_readable_intent() {
        let opaque = json!({"type":"agent_message","role":"system","content":[
            {"type":"input_text","text":"Message Type: NEW_TASK\nPayload:\n"},
            {"type":"encrypted_content","encrypted_content":"opaque-ciphertext"}
        ]});

        let mut historical = json!({"input":[
            opaque.clone(),
            {"type":"message","role":"user","content":"continue with the visible task"}
        ]});
        normalize_codex_multi_agent_v2(&mut historical, true).unwrap();
        assert_eq!(historical["input"].as_array().unwrap().len(), 1);
        assert_eq!(
            historical["input"][0]["content"],
            "continue with the visible task"
        );
        assert!(!historical.to_string().contains("opaque-ciphertext"));

        let mut current = json!({"input":[
            {"type":"message","role":"user","content":"earlier visible request"},
            opaque
        ]});
        assert!(normalize_codex_multi_agent_v2(&mut current, true).is_err());

        let mut whitespace_history = json!({"input":[
            {"type":"agent_message","content":[
                {"type":"encrypted_content","encrypted_content":"opaque-ciphertext"}
            ]},
            {"type":"message","role":"user","content":"   \n"}
        ]});
        assert!(normalize_codex_multi_agent_v2(&mut whitespace_history, true).is_err());

        let mut whitespace_agent = json!({"input":[
            {"type":"agent_message","content":[
                {"type":"input_text","text":"\n"},
                {"type":"encrypted_content","encrypted_content":"opaque-ciphertext"}
            ]}
        ]});
        assert!(normalize_codex_multi_agent_v2(&mut whitespace_agent, true).is_err());
    }
}
