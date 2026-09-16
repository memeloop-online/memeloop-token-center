use serde_json::Value;

const COLLABORATION_TOOL_NAMES: &[&str] = &["spawn_agent", "send_message", "followup_task"];

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
    let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for part in &mut *content {
        if part.get("type").and_then(Value::as_str) != Some("encrypted_content") {
            continue;
        }
        let Some(text) = part
            .get("encrypted_content")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            // Non-string payloads are left untouched and will fail closed in
            // the destination transport instead of being guessed at.
            continue;
        };
        if let Some(part) = part.as_object_mut() {
            part.insert("type".into(), Value::String("input_text".into()));
            part.insert("text".into(), Value::String(text));
            part.remove("encrypted_content");
        }
    }

    // CPA's compatibility rewrite lowers the inter-agent envelope to an
    // ordinary user message.  Only do so once every part is readable; an
    // opaque payload remains agent_message and is rejected by the target
    // transport instead of being silently forwarded as an unknown object.
    let readable = !content.is_empty()
        && content
            .iter()
            .all(|part| match part.get("type").and_then(Value::as_str) {
                Some("input_text" | "output_text" | "text") => {
                    part.get("text").is_some_and(Value::is_string)
                }
                Some("input_image") => part.get("image_url").is_some_and(Value::is_string),
                _ => false,
            });
    if readable {
        if let Some(item) = item.as_object_mut() {
            item.insert("type".into(), Value::String("message".into()));
            item.insert("role".into(), Value::String("user".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
                    {"type":"encrypted_content","encrypted_content":"delegated task","trace":"keep"},
                    {"type":"encrypted_content","encrypted_content":{"ciphertext":"opaque"}}
                ]
            }]
        });
        normalize_codex_multi_agent_v2(&mut request, true);

        assert_eq!(request["input"][0]["type"], "message");
        assert_eq!(request["input"][0]["role"], "user");
        assert_eq!(
            request["input"][0]["internal_chat_message_metadata_passthrough"]["turn_id"],
            "turn"
        );
        assert_eq!(request["input"][0]["content"][1]["type"], "input_text");
        assert_eq!(request["input"][0]["content"][1]["text"], "delegated task");
        assert!(
            request["input"][0]["content"][1]
                .get("encrypted_content")
                .is_none()
        );
        assert_eq!(request["input"][0]["content"][1]["trace"], "keep");
        assert_eq!(
            request["input"][0]["content"][2]["encrypted_content"]["ciphertext"],
            "opaque"
        );
    }
}
