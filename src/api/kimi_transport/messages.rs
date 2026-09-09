use serde_json::{Value, json};

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("").trim()
}

fn empty_content(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(value) => value.trim().is_empty(),
        Value::Array(parts) => parts.iter().all(empty_part),
        _ => false,
    }
}

fn empty_part(value: &Value) -> bool {
    match value {
        Value::Null | Value::String(_) => empty_content(value),
        Value::Object(value) => {
            if let Some(part_text) = value.get("text") {
                return text(part_text).is_empty();
            }
            value.is_empty() || value.get("type").and_then(Value::as_str) == Some("text")
        }
        _ => false,
    }
}

/// Source-compatible repair within this request only. A missing tool result ID
/// is inferred only when there is exactly one pending call; ambiguity is never
/// resolved by selecting an arbitrary call.
pub(super) fn repair(request: &mut Value) {
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let mut pending = Vec::<String>::new();
    let mut latest_reasoning = String::new();
    messages.retain_mut(|message| {
        match text(&message["role"]) {
            "assistant" => {
                let calls = message["tool_calls"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let reasoning = text(&message["reasoning_content"]);
                let has_legacy_call =
                    !message["function_call"].is_null() && message["function_call"] != json!({});
                if calls.is_empty()
                    && reasoning.is_empty()
                    && !has_legacy_call
                    && empty_content(&message["content"])
                {
                    return false;
                }
                if !reasoning.is_empty() {
                    latest_reasoning = reasoning.to_owned();
                } else if !calls.is_empty() {
                    let content = match &message["content"] {
                        Value::String(value) => value.trim().to_owned(),
                        Value::Array(parts) => parts
                            .iter()
                            .map(|part| text(&part["text"]))
                            .filter(|text| !text.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => String::new(),
                    };
                    let fallback = if !latest_reasoning.is_empty() {
                        latest_reasoning.clone()
                    } else if !content.is_empty() {
                        content
                    } else {
                        "[reasoning unavailable]".into()
                    };
                    message["reasoning_content"] = Value::String(fallback);
                }
                pending.extend(
                    calls
                        .iter()
                        .map(|call| text(&call["id"]))
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned),
                );
            }
            "tool" => {
                let mut id = text(&message["tool_call_id"]).to_owned();
                if id.is_empty() {
                    id = text(&message["call_id"]).to_owned();
                    if id.is_empty() && pending.len() == 1 {
                        id.clone_from(&pending[0]);
                    }
                    if !id.is_empty() {
                        message["tool_call_id"] = Value::String(id.clone());
                    }
                }
                if let Some(index) = pending.iter().position(|candidate| candidate == &id) {
                    pending.remove(index);
                }
            }
            _ => {}
        }
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repairs_unique_links_and_preserves_ambiguous_results() {
        let mut request = json!({"messages":[
            {"role":"assistant","content":""},
            {"role":"assistant","reasoning_content":"remember","content":""},
            {"role":"assistant","tool_calls":[{"id":"a"}]},
            {"role":"tool","content":"result"},
            {"role":"assistant","tool_calls":[{"id":"b"},{"id":"c"}]},
            {"role":"tool","content":"ambiguous"},
            {"role":"tool","call_id":"c","content":"explicit"}
        ]});
        repair(&mut request);
        assert_eq!(request["messages"].as_array().unwrap().len(), 6);
        assert_eq!(request["messages"][1]["reasoning_content"], "remember");
        assert_eq!(request["messages"][2]["tool_call_id"], "a");
        assert!(request["messages"][4]["tool_call_id"].is_null());
        assert_eq!(request["messages"][5]["tool_call_id"], "c");
    }
}
