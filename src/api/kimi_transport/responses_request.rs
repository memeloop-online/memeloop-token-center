use super::AppError;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
pub(in crate::api) struct ToolIdentity {
    pub name: String,
    pub namespace: String,
    pub custom: bool,
}

pub(super) fn qualified(namespace: &str, name: &str) -> String {
    if namespace.is_empty() || name.starts_with("mcp__") || name.starts_with(namespace) {
        name.into()
    } else if namespace.ends_with("__") {
        format!("{namespace}{name}")
    } else {
        format!("{namespace}__{name}")
    }
}

pub(in crate::api) fn tools(request: &Value) -> BTreeMap<String, (ToolIdentity, Value)> {
    fn insert(
        result: &mut BTreeMap<String, (ToolIdentity, Value)>,
        tools: &Value,
        namespace: &str,
    ) {
        let Some(tools) = tools.as_array() else {
            return;
        };
        for tool in tools {
            let kind = tool["type"].as_str().unwrap_or("function");
            if kind == "namespace" {
                insert(result, &tool["tools"], tool["name"].as_str().unwrap_or(""));
                continue;
            }
            if !matches!(kind, "function" | "custom") {
                continue;
            }
            let definition = tool.get("function").unwrap_or(tool);
            let name = definition["name"].as_str().unwrap_or("").trim();
            if name.is_empty() {
                continue;
            }
            let wire_name = qualified(namespace, name);
            let mut function = json!({"name":wire_name,"description":definition["description"].as_str().unwrap_or("")});
            function["parameters"] = if kind == "custom" {
                json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"]})
            } else {
                definition
                    .get("parameters")
                    .or_else(|| definition.get("parametersJsonSchema"))
                    .or_else(|| definition.get("input_schema"))
                    .cloned()
                    .unwrap_or_else(|| json!({}))
            };
            result.entry(wire_name).or_insert((
                ToolIdentity {
                    name: name.into(),
                    namespace: namespace.into(),
                    custom: kind == "custom",
                },
                json!({"type":"function","function":function}),
            ));
        }
    }
    let mut result = BTreeMap::new();
    insert(&mut result, &request["tools"], "");
    if let Some(input) = request["input"].as_array() {
        for item in input {
            if item["type"] == "additional_tools" {
                insert(&mut result, &item["tools"], "");
            }
        }
    }
    result
}

fn content(value: &Value) -> Value {
    let Some(parts) = value.as_array() else {
        return value.clone();
    };
    Value::Array(
        parts
            .iter()
            .map(|part| match part["type"].as_str().unwrap_or("input_text") {
                "input_text" | "output_text" | "text" => json!({"type":"text","text":part["text"]}),
                "input_image" => {
                    let mut image =
                        json!({"type":"image_url","image_url":{"url":part["image_url"]}});
                    if let Some(detail) = part["detail"].as_str() {
                        image["image_url"]["detail"] = Value::String(
                            if detail == "original" { "high" } else { detail }.into(),
                        );
                    }
                    image
                }
                _ => part.clone(),
            })
            .collect(),
    )
}

fn combine(existing: &mut String, incoming: &str) {
    if incoming.trim().is_empty() || existing == incoming {
        return;
    }
    if existing.is_empty() || existing == "[reasoning unavailable]" {
        *existing = incoming.into();
    } else if incoming != "[reasoning unavailable]" {
        existing.push_str("\n\n");
        existing.push_str(incoming);
    }
}

/// Convert the source's Responses message/tool forms without a service bridge.
/// Tool outputs remain adjacent to their calls even when interleaved user
/// messages occur in the input timeline.
pub(super) fn convert(request: &Value) -> Result<Value, AppError> {
    if request
        .get("previous_response_id")
        .is_some_and(|id| !id.is_null())
    {
        return Err(AppError::BadRequest(
            "Kimi Responses continuation requires the complete input history".into(),
        ));
    }
    let mut output =
        json!({"model":request["model"], "stream":request["stream"].as_bool().unwrap_or(false)});
    let mut messages = Vec::<Value>::new();
    if let Some(instructions) = request["instructions"].as_str() {
        messages.push(json!({"role":"system","content":instructions}));
    }
    let input = match &request["input"] {
        Value::String(text) => vec![json!({"role":"user","content":text})],
        Value::Array(input) => input.clone(),
        _ => {
            return Err(AppError::BadRequest(
                "Responses input must be a string or array".into(),
            ));
        }
    };
    let output_ids = input
        .iter()
        .filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call_output" | "custom_tool_call_output")
            )
        })
        .filter_map(|item| item["call_id"].as_str())
        .collect::<BTreeSet<_>>();
    let mut awaiting = BTreeSet::<String>::new();
    let mut deferred = Vec::new();
    let mut reasoning = String::new();
    for item in &input {
        match item["type"].as_str().unwrap_or("message") {
            "reasoning" => {
                let summary = item["summary"]
                    .as_array()
                    .map(|parts| {
                        parts
                            .iter()
                            .filter(|part| part["type"] == "summary_text")
                            .filter_map(|part| part["text"].as_str())
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                combine(
                    &mut reasoning,
                    if summary.is_empty() {
                        "[reasoning unavailable]"
                    } else {
                        &summary
                    },
                );
            }
            "function_call" | "custom_tool_call" => {
                combine(
                    &mut reasoning,
                    item["reasoning_content"].as_str().unwrap_or(""),
                );
                let id = item["call_id"].as_str().unwrap_or("");
                let arguments = if item["type"] == "custom_tool_call" {
                    serde_json::to_string(&json!({"input":item["input"]}))
                        .map_err(|_| AppError::Internal)?
                } else {
                    item["arguments"].as_str().unwrap_or("").into()
                };
                let call = json!({"id":id,"type":"function","function":{
                    "name":qualified(item["namespace"].as_str().unwrap_or(""),item["name"].as_str().unwrap_or("")),
                    "arguments":arguments}});
                let merge = messages
                    .last()
                    .is_some_and(|message| message["role"] == "assistant");
                if !merge {
                    messages.push(json!({"role":"assistant","tool_calls":[]}));
                }
                let message = messages.last_mut().ok_or(AppError::Internal)?;
                if message.get("tool_calls").is_none() {
                    message["tool_calls"] = json!([]);
                }
                message["tool_calls"]
                    .as_array_mut()
                    .ok_or(AppError::Internal)?
                    .push(call);
                if !reasoning.is_empty() {
                    message["reasoning_content"] = Value::String(std::mem::take(&mut reasoning));
                }
                if output_ids.contains(id) {
                    awaiting.insert(id.into());
                }
            }
            "function_call_output" | "custom_tool_call_output" => {
                let id = item["call_id"].as_str().unwrap_or("");
                let body = if item["output"].is_array() {
                    content(&item["output"])
                } else if item["output"].is_string() {
                    item["output"].clone()
                } else {
                    Value::String(item["output"].to_string())
                };
                messages.push(json!({"role":"tool","tool_call_id":id,"content":body}));
                awaiting.remove(id);
                if awaiting.is_empty() {
                    messages.append(&mut deferred);
                }
            }
            "additional_tools" => {}
            "message" => {
                let role = item["role"].as_str().unwrap_or("user");
                let mut message = json!({"role":if role == "developer" {"user"} else {role},
                    "content":content(&item["content"])});
                if role == "assistant" {
                    combine(
                        &mut reasoning,
                        item["reasoning_content"].as_str().unwrap_or(""),
                    );
                    if !reasoning.is_empty() {
                        message["reasoning_content"] =
                            Value::String(std::mem::take(&mut reasoning));
                    }
                } else if !reasoning.is_empty() {
                    messages.push(json!({"role":"assistant","content":"",
                        "reasoning_content":std::mem::take(&mut reasoning)}));
                }
                if awaiting.is_empty() {
                    messages.push(message);
                } else {
                    deferred.push(message);
                }
            }
            _ => {
                return Err(AppError::BadRequest(
                    "unsupported Responses input item for Kimi".into(),
                ));
            }
        }
    }
    messages.append(&mut deferred);
    if !reasoning.is_empty() {
        messages.push(json!({"role":"assistant","content":"","reasoning_content":reasoning}));
    }
    output["messages"] = Value::Array(messages);
    if let Some(limit) = request.get("max_output_tokens") {
        output["max_tokens"] = limit.clone();
    }
    for name in [
        "temperature",
        "top_p",
        "parallel_tool_calls",
        "service_tier",
    ] {
        if let Some(value) = request.get(name) {
            output[name] = value.clone();
        }
    }
    if let Some(effort) = request.pointer("/reasoning/effort") {
        output["reasoning_effort"] = effort.clone();
    }
    if let Some(format) = request.pointer("/text/format") {
        output["response_format"] = if format["type"] == "json_schema" {
            let mut schema = format.clone();
            schema
                .as_object_mut()
                .ok_or(AppError::Internal)?
                .remove("type");
            json!({"type":"json_schema","json_schema":schema})
        } else {
            format.clone()
        };
    }
    let declarations = tools(request);
    if !declarations.is_empty() {
        output["tools"] = Value::Array(declarations.into_values().map(|(_, tool)| tool).collect());
        if let Some(choice) = request.get("tool_choice") {
            output["tool_choice"] = if choice.is_object() && choice.get("name").is_some() {
                json!({"type":"function","function":{"name":qualified(
                    choice["namespace"].as_str().unwrap_or(""), choice["name"].as_str().unwrap_or(""))}})
            } else {
                choice.clone()
            };
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lite_tools_namespace_custom_and_ordered_results_survive_conversion() {
        let request = json!({"model":"kimi-k3","stream":true,"instructions":"system",
            "tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}],
            "input":[
                {"type":"reasoning","summary":[{"type":"summary_text","text":"reason"}]},
                {"type":"custom_tool_call","name":"patch","namespace":"editor","call_id":"a","input":"diff"},
                {"role":"user","content":"continue"},
                {"type":"custom_tool_call_output","call_id":"a","output":"ok"},
                {"type":"additional_tools","tools":[{"type":"function","name":"other","parameters":{"type":"object"}}]}
            ],"max_output_tokens":400,"reasoning":{"effort":"high"},
            "text":{"format":{"type":"json_schema","name":"result","schema":{"type":"object"},"strict":true}}});
        let converted = convert(&request).unwrap();
        assert_eq!(converted["messages"][0]["role"], "system");
        assert_eq!(
            converted["messages"][1]["tool_calls"][0]["function"]["name"],
            "editor__patch"
        );
        assert_eq!(converted["messages"][1]["reasoning_content"], "reason");
        assert_eq!(converted["messages"][2]["role"], "tool");
        assert_eq!(converted["messages"][3]["content"], "continue");
        assert_eq!(converted["tools"].as_array().unwrap().len(), 2);
        assert_eq!(converted["max_tokens"], 400);
        assert_eq!(converted["reasoning_effort"], "high");
        assert_eq!(converted["response_format"]["json_schema"]["strict"], true);
    }

    #[test]
    fn first_tool_declaration_controls_reverse_mapping_and_image_detail() {
        let request = json!({"input":[{"role":"user","content":[
            {"type":"input_image","image_url":"data:image/png;base64,fixture","detail":"original"}]},
            {"type":"additional_tools","tools":[{"type":"custom","name":"same"}]}],
            "tools":[{"type":"function","name":"same","parameters":{}}]});
        assert!(!tools(&request)["same"].0.custom);
        let output = convert(&request).unwrap();
        assert_eq!(
            output["messages"][0]["content"][0]["image_url"]["detail"],
            "high"
        );
    }
}
