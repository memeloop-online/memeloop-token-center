use http::{HeaderMap, header};
use serde_json::Value;

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
    let user_agent = user_agent.trim();
    user_agent.starts_with("Codex Desktop/")
        || user_agent.starts_with("codex-tui/")
        || user_agent == "codex_cli_rs"
        || user_agent.starts_with("codex_cli_rs/")
}

/// Normalize the subset of Codex MultiAgentV2 request shapes that a declared
/// third-party upstream can read.  The native Codex route deliberately does
/// not call this function. Readable agent messages are lowered to an ordinary
/// user message, while opaque content is left untouched for the destination
/// transport to reject; inter-agent content never gains a higher privilege
/// role.
pub(super) fn normalize_codex_multi_agent_v2(request: &mut Value, enabled: bool) {
    if !enabled {
        return;
    }

    if let Some(tools) = request.get_mut("tools") {
        rewrite_tool_list(tools);
    }
    if let Some(input) = request.get_mut("input").and_then(Value::as_array_mut) {
        for item in input {
            match item.get("type").and_then(Value::as_str) {
                Some("additional_tools") => {
                    if let Some(tools) = item.get_mut("tools") {
                        rewrite_tool_list(tools);
                    }
                }
                Some("agent_message") => rewrite_agent_message(item),
                _ => {}
            }
        }
    }
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

fn rewrite_agent_message(item: &mut Value) {
    let Some(content) = item.get("content").and_then(Value::as_array).cloned() else {
        return;
    };

    let normalized_content = content
        .into_iter()
        .map(|mut part| {
            if part.get("type").and_then(Value::as_str) == Some("encrypted_content")
                && let Some(text) = part
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                && let Some(part) = part.as_object_mut()
            {
                part.insert("type".into(), Value::String("input_text".into()));
                part.insert("text".into(), Value::String(text));
                part.remove("encrypted_content");
            }
            part
        })
        .collect::<Vec<_>>();

    // CPA's compatibility rewrite lowers the inter-agent envelope to an
    // ordinary user message.  Only do so once every part is readable; an
    // opaque payload remains agent_message and is rejected by the target
    // transport instead of being silently forwarded as an unknown object.
    let Some(sanitized_content) = normalized_content
        .iter()
        .map(|part| match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => part
                .get("text")
                .and_then(Value::as_str)
                .map(|text| serde_json::json!({"type":"input_text", "text":text})),
            Some("input_image") => part
                .get("image_url")
                .and_then(Value::as_str)
                .map(|image_url| {
                    let mut sanitized = serde_json::json!({
                        "type": "input_image",
                        "image_url": image_url,
                    });
                    if let Some(detail) = part.get("detail").filter(Value::is_string) {
                        sanitized["detail"] = detail.clone();
                    }
                    sanitized
                }),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
    else {
        if let Some(item) = item.as_object_mut() {
            item.insert("content".into(), Value::Array(normalized_content));
        }
        return;
    };
    if sanitized_content.is_empty() {
        return;
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
            "codex-tui/0.1.0",
            "codex_cli_rs",
            "codex_cli_rs/0.1.0",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::USER_AGENT, HeaderValue::from_static(user_agent));
            assert!(is_official_codex_user_agent(&headers), "{user_agent}");
        }
        for user_agent in ["Mozilla/5.0", "Codex Desktop", "codex_cli_rs-other"] {
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
        normalize_codex_multi_agent_v2(&mut request, false);
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
        normalize_codex_multi_agent_v2(&mut request, true);

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
        normalize_codex_multi_agent_v2(&mut request, true);

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
    fn opaque_agent_message_stays_agent_message_and_keeps_payload() {
        let mut request = json!({
            "input": [{"type":"agent_message","role":"system", "content":[
                {"type":"input_text","text":"prefix"},
                {"type":"encrypted_content","encrypted_content":{"ciphertext":"opaque"}}
            ]}]
        });
        normalize_codex_multi_agent_v2(&mut request, true);

        assert_eq!(request["input"][0]["type"], "agent_message");
        assert_eq!(request["input"][0]["role"], "system");
        assert_eq!(
            request["input"][0]["content"][1]["encrypted_content"]["ciphertext"],
            "opaque"
        );
    }
}
