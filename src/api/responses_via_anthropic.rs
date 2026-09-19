//! Explicit, versioned OpenAI Responses to Anthropic Messages compatibility.
//!
//! This adapter owns both wire directions. It never repairs tool history by
//! deleting it and emits a successful Responses terminal only after the full
//! Anthropic lifecycle and accounting evidence have been observed.

use super::AppError;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

const MAX_ITEMS: usize = 512;
const MAX_ACCUMULATED_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
struct ToolIdentity {
    name: String,
    namespace: String,
    custom: bool,
}

#[derive(Clone)]
pub(in crate::api) struct Context {
    model: String,
    tools: BTreeMap<String, ToolIdentity>,
}

impl Context {
    fn tool_placeholder(
        &self,
        wire_name: &str,
        call_id: &str,
        item_id: &str,
    ) -> Result<Value, &'static str> {
        let identity = self.tools.get(wire_name);
        let custom = identity.is_some_and(|tool| tool.custom);
        let mut item = if custom {
            json!({"type":"custom_tool_call","id":item_id,"call_id":call_id,
                "name":identity.map_or(wire_name, |tool| tool.name.as_str()),
                "input":"","status":"in_progress"})
        } else {
            json!({"type":"function_call","id":item_id,"call_id":call_id,
                "name":identity.map_or(wire_name, |tool| tool.name.as_str()),
                "arguments":"","status":"in_progress"})
        };
        if let Some(identity) = identity.filter(|tool| !tool.namespace.is_empty()) {
            item["namespace"] = Value::String(identity.namespace.clone());
        }
        Ok(item)
    }

    fn tool_item(
        &self,
        wire_name: &str,
        call_id: &str,
        item_id: &str,
        input: &Value,
        status: &str,
    ) -> Result<Value, &'static str> {
        let identity = self.tools.get(wire_name);
        let custom = identity.is_some_and(|tool| tool.custom);
        let mut item = if custom {
            let text = input
                .get("input")
                .and_then(Value::as_str)
                .ok_or("anthropic_custom_tool_input_invalid")?;
            json!({"type":"custom_tool_call","id":item_id,"call_id":call_id,
                "name":identity.map_or(wire_name, |tool| tool.name.as_str()),
                "input":text,"status":status})
        } else {
            json!({"type":"function_call","id":item_id,"call_id":call_id,
                "name":identity.map_or(wire_name, |tool| tool.name.as_str()),
                "arguments":serde_json::to_string(input).map_err(|_| "anthropic_tool_input_invalid")?,
                "status":status})
        };
        if let Some(identity) = identity.filter(|tool| !tool.namespace.is_empty()) {
            item["namespace"] = Value::String(identity.namespace.clone());
        }
        Ok(item)
    }
}

pub(in crate::api) fn prepare(model: &str, request: &mut Value) -> Result<Context, AppError> {
    validate_top_level(request)?;
    let public_model = request
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| AppError::BadRequest("model is required".into()))?
        .to_owned();
    let (tools, identities) = collect_tools(request)?;
    let context = Context {
        model: public_model,
        tools: identities,
    };
    let mut system = Vec::new();
    if let Some(instructions) = request.get("instructions").and_then(Value::as_str)
        && !instructions.trim().is_empty()
    {
        system.push(json!({"type":"text","text":instructions}));
    }
    let mut messages = Vec::new();
    convert_input(request, &mut system, &mut messages)?;
    if messages.is_empty() {
        return Err(AppError::BadRequest(
            "Responses-via-Anthropic requires at least one message".into(),
        ));
    }
    let mut output = json!({
        "model":model,
        "messages":messages,
        "stream":request.get("stream").and_then(Value::as_bool).unwrap_or(false)
    });
    if !system.is_empty() {
        output["system"] = Value::Array(system);
    }
    if !tools.is_empty() {
        output["tools"] = Value::Array(tools);
    }
    if let Some(limit) = request.get("max_output_tokens") {
        output["max_tokens"] = limit.clone();
    }
    for field in ["temperature", "top_p", "service_tier"] {
        if let Some(value) = request.get(field) {
            output[field] = value.clone();
        }
    }
    apply_tool_choice(&mut output, request, &context.tools)?;
    *request = output;
    Ok(context)
}

fn validate_top_level(request: &Value) -> Result<(), AppError> {
    if !request.is_object() {
        return Err(AppError::BadRequest(
            "request body must be an object".into(),
        ));
    }
    if request
        .get("previous_response_id")
        .is_some_and(|value| !value.is_null())
    {
        return Err(AppError::BadRequest(
            "Responses-via-Anthropic continuation requires the complete input history".into(),
        ));
    }
    for field in ["conversation", "prompt"] {
        if request.get(field).is_some_and(|value| !value.is_null()) {
            return Err(AppError::BadRequest(format!(
                "Responses-via-Anthropic cannot preserve {field}"
            )));
        }
    }
    for field in ["background", "store"] {
        if request.get(field).and_then(Value::as_bool) == Some(true) {
            return Err(AppError::BadRequest(format!(
                "Responses-via-Anthropic requires {field} = false"
            )));
        }
    }
    if request.get("reasoning").is_some_and(|value| {
        !value.is_null() && value.as_object().is_none_or(|value| !value.is_empty())
    }) {
        return Err(AppError::BadRequest(
            "Responses-via-Anthropic reasoning configuration requires signed thinking replay"
                .into(),
        ));
    }
    if request
        .get("include")
        .and_then(Value::as_array)
        .is_some_and(|values| !values.is_empty())
    {
        return Err(AppError::BadRequest(
            "Responses-via-Anthropic cannot preserve requested response expansions".into(),
        ));
    }
    if let Some(format) = request.pointer("/text/format")
        && format.get("type").and_then(Value::as_str) != Some("text")
    {
        return Err(AppError::BadRequest(
            "Responses-via-Anthropic supports text output format".into(),
        ));
    }
    validate_tool_history(request)?;
    Ok(())
}

fn validate_tool_history(request: &Value) -> Result<(), AppError> {
    let Some(items) = request.get("input").and_then(Value::as_array) else {
        return Ok(());
    };
    let mut awaiting = BTreeSet::<String>::new();
    let mut answered = BTreeSet::<String>::new();
    let mut in_tool_exchange = false;
    for item in items {
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        match kind {
            "function_call" | "custom_tool_call" => {
                if !answered.is_empty() {
                    answered.clear();
                }
                let id = required_string(item, "call_id", "tool call_id")?;
                if !awaiting.insert(id.to_owned()) {
                    return Err(AppError::BadRequest(
                        "Responses-via-Anthropic tool call_id must be unique".into(),
                    ));
                }
                in_tool_exchange = true;
            }
            "function_call_output" | "custom_tool_call_output" => {
                let id = required_string(item, "call_id", "tool result call_id")?;
                if !awaiting.remove(id) || !answered.insert(id.to_owned()) {
                    return Err(AppError::BadRequest(
                        "Responses-via-Anthropic tool results must follow their tool calls exactly once"
                            .into(),
                    ));
                }
                in_tool_exchange = !awaiting.is_empty();
            }
            "additional_tools" => {}
            "reasoning" => {
                return Err(AppError::BadRequest(
                    "Responses-via-Anthropic cannot preserve reasoning state".into(),
                ));
            }
            "compaction" => {
                return Err(AppError::BadRequest(
                    "Responses-via-Anthropic cannot preserve compaction state".into(),
                ));
            }
            "message" => {
                if in_tool_exchange || !awaiting.is_empty() {
                    return Err(AppError::BadRequest(
                        "Responses-via-Anthropic tool calls and results must be adjacent".into(),
                    ));
                }
                answered.clear();
            }
            "agent_message" => {
                return Err(AppError::BadRequest(
                    "Responses-via-Anthropic requires normalized agent messages".into(),
                ));
            }
            other => {
                return Err(AppError::BadRequest(format!(
                    "unsupported Responses input item for Responses-via-Anthropic: {other}"
                )));
            }
        }
    }
    if !awaiting.is_empty() {
        return Err(AppError::BadRequest(
            "Responses-via-Anthropic tool history has unanswered calls".into(),
        ));
    }
    Ok(())
}

fn collect_tools(
    request: &Value,
) -> Result<(Vec<Value>, BTreeMap<String, ToolIdentity>), AppError> {
    let mut output = Vec::new();
    let mut identities = BTreeMap::new();
    collect_tool_array(request.get("tools"), "", &mut output, &mut identities)?;
    if let Some(items) = request.get("input").and_then(Value::as_array) {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                collect_tool_array(item.get("tools"), "", &mut output, &mut identities)?;
            }
        }
    }
    Ok((output, identities))
}

fn collect_tool_array(
    tools: Option<&Value>,
    namespace: &str,
    output: &mut Vec<Value>,
    identities: &mut BTreeMap<String, ToolIdentity>,
) -> Result<(), AppError> {
    let Some(tools) = tools else {
        return Ok(());
    };
    let tools = tools.as_array().ok_or_else(|| {
        AppError::BadRequest("Responses-via-Anthropic tools must be an array".into())
    })?;
    for tool in tools {
        let kind = tool
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("function");
        if kind == "namespace" {
            let nested_namespace = required_string(tool, "name", "tool namespace")?;
            let nested = qualify(namespace, nested_namespace);
            collect_tool_array(tool.get("tools"), &nested, output, identities)?;
            continue;
        }
        if !matches!(kind, "function" | "custom") {
            return Err(AppError::BadRequest(format!(
                "unsupported Responses host tool for Responses-via-Anthropic: {kind}"
            )));
        }
        let definition = tool.get("function").unwrap_or(tool);
        let name = required_string(definition, "name", "tool name")?;
        let wire_name = qualify(namespace, name);
        if identities.contains_key(&wire_name) {
            return Err(AppError::BadRequest(format!(
                "duplicate Responses-via-Anthropic tool name: {wire_name}"
            )));
        }
        let schema = if kind == "custom" {
            json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"]})
        } else {
            definition
                .get("parameters")
                .or_else(|| definition.get("parametersJsonSchema"))
                .or_else(|| definition.get("input_schema"))
                .cloned()
                .unwrap_or_else(|| json!({"type":"object","properties":{}}))
        };
        validate_object_schema(&schema)?;
        output.push(json!({
            "name":wire_name,
            "description":definition.get("description").and_then(Value::as_str).unwrap_or_default(),
            "input_schema":schema
        }));
        identities.insert(
            wire_name,
            ToolIdentity {
                name: name.to_owned(),
                namespace: namespace.to_owned(),
                custom: kind == "custom",
            },
        );
    }
    Ok(())
}

fn validate_object_schema(schema: &Value) -> Result<(), AppError> {
    let object = schema.as_object().ok_or_else(|| {
        AppError::BadRequest("Anthropic tool input schema must be an object".into())
    })?;
    if object
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "object")
        || object.contains_key("oneOf")
        || object.contains_key("anyOf")
        || object.contains_key("allOf")
    {
        return Err(AppError::BadRequest(
            "Anthropic tool input schema requires one object root".into(),
        ));
    }
    Ok(())
}

fn qualify(namespace: &str, name: &str) -> String {
    if namespace.is_empty() || name.starts_with("mcp__") {
        name.to_owned()
    } else if namespace.ends_with("__") {
        format!("{namespace}{name}")
    } else {
        format!("{namespace}__{name}")
    }
}

fn convert_input(
    request: &Value,
    system: &mut Vec<Value>,
    messages: &mut Vec<Value>,
) -> Result<(), AppError> {
    let input = match request.get("input") {
        Some(Value::String(text)) => {
            push_message(messages, "user", vec![json!({"type":"text","text":text})]);
            return Ok(());
        }
        Some(Value::Array(items)) => items,
        _ => {
            return Err(AppError::BadRequest(
                "Responses input must be a string or array".into(),
            ));
        }
    };
    let mut index = 0;
    while index < input.len() {
        let item = &input[index];
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        match kind {
            "message" => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                let blocks =
                    responses_content(item.get("content").unwrap_or(&Value::Null), role == "user")?;
                if matches!(role, "system" | "developer") {
                    if !messages.is_empty() {
                        return Err(AppError::BadRequest(
                            "Responses-via-Anthropic system messages must precede conversation messages"
                                .into(),
                        ));
                    }
                    system.extend(blocks);
                } else if matches!(role, "user" | "assistant") {
                    push_message(messages, role, blocks);
                } else {
                    return Err(AppError::BadRequest(format!(
                        "unsupported Responses-via-Anthropic message role: {role}"
                    )));
                }
                index += 1;
            }
            "additional_tools" => index += 1,
            "reasoning" => {
                return Err(AppError::BadRequest(
                    "Responses-via-Anthropic cannot preserve reasoning state".into(),
                ));
            }
            "agent_message" => {
                return Err(AppError::BadRequest(
                    "Responses-via-Anthropic requires normalized agent messages".into(),
                ));
            }
            "function_call" | "custom_tool_call" => {
                let mut calls = Vec::new();
                let mut call_ids = Vec::new();
                while index < input.len()
                    && matches!(
                        input[index].get("type").and_then(Value::as_str),
                        Some("function_call" | "custom_tool_call")
                    )
                {
                    let call = &input[index];
                    let call_kind = call.get("type").and_then(Value::as_str).unwrap_or_default();
                    let call_id = required_string(call, "call_id", "tool call_id")?;
                    let name = required_string(call, "name", "tool name")?;
                    let wire_name = qualify(
                        call.get("namespace")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        name,
                    );
                    let arguments = if call_kind == "custom_tool_call" {
                        json!({"input":required_string(call, "input", "custom tool input")?})
                    } else {
                        let raw = required_string(call, "arguments", "tool arguments")?;
                        let parsed =
                            crate::api::sse::parse_unique_json(raw.as_bytes()).map_err(|_| {
                                AppError::BadRequest(
                                    "Responses-via-Anthropic tool arguments must be JSON".into(),
                                )
                            })?;
                        if !parsed.is_object() {
                            return Err(AppError::BadRequest(
                                "Responses-via-Anthropic tool arguments must be an object".into(),
                            ));
                        }
                        parsed
                    };
                    calls.push(
                        json!({"type":"tool_use","id":call_id,"name":wire_name,"input":arguments}),
                    );
                    call_ids.push(call_id.to_owned());
                    index += 1;
                }
                push_message(messages, "assistant", calls);
                let mut results = Vec::new();
                for call_id in call_ids {
                    let result = input.get(index).ok_or_else(|| {
                        AppError::BadRequest(
                            "Responses-via-Anthropic tool history has unanswered calls".into(),
                        )
                    })?;
                    if !matches!(
                        result.get("type").and_then(Value::as_str),
                        Some("function_call_output" | "custom_tool_call_output")
                    ) || result.get("call_id").and_then(Value::as_str) != Some(call_id.as_str())
                    {
                        return Err(AppError::BadRequest(
                            "Responses-via-Anthropic parallel tool results must preserve call order"
                                .into(),
                        ));
                    }
                    results.push(json!({
                        "type":"tool_result",
                        "tool_use_id":call_id,
                        "content":tool_result_content(result.get("output").unwrap_or(&Value::Null))?
                    }));
                    index += 1;
                }
                push_message(messages, "user", results);
            }
            "function_call_output" | "custom_tool_call_output" => {
                return Err(AppError::BadRequest(
                    "Responses-via-Anthropic tool result has no adjacent call".into(),
                ));
            }
            other => {
                return Err(AppError::BadRequest(format!(
                    "unsupported Responses input item for Responses-via-Anthropic: {other}"
                )));
            }
        }
    }
    Ok(())
}

fn responses_content(content: &Value, allow_images: bool) -> Result<Vec<Value>, AppError> {
    if let Some(text) = content.as_str() {
        if text.trim().is_empty() {
            return Err(AppError::BadRequest(
                "Responses-via-Anthropic message content is empty".into(),
            ));
        }
        return Ok(vec![json!({"type":"text","text":text})]);
    }
    let parts = content.as_array().ok_or_else(|| {
        AppError::BadRequest("Responses-via-Anthropic message content is invalid".into())
    })?;
    let mut blocks = Vec::with_capacity(parts.len());
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => {
                let text = required_string(part, "text", "message text")?;
                blocks.push(json!({"type":"text","text":text}));
            }
            Some("input_image") if allow_images => {
                if part.get("detail").and_then(Value::as_str) == Some("original") {
                    return Err(AppError::BadRequest(
                        "Responses-via-Anthropic does not support original image detail".into(),
                    ));
                }
                let url = required_string(part, "image_url", "image URL")?;
                blocks.push(json!({"type":"image","source":anthropic_image_source(url)?}));
            }
            Some("encrypted_content") => {
                return Err(AppError::BadRequest(
                    "Responses-via-Anthropic agent content must be readable".into(),
                ));
            }
            Some(kind) => {
                return Err(AppError::BadRequest(format!(
                    "unsupported Responses-via-Anthropic content block: {kind}"
                )));
            }
            None => {
                return Err(AppError::BadRequest(
                    "Responses-via-Anthropic content block type is required".into(),
                ));
            }
        }
    }
    if blocks.is_empty() {
        return Err(AppError::BadRequest(
            "Responses-via-Anthropic message content is empty".into(),
        ));
    }
    Ok(blocks)
}

fn tool_result_content(output: &Value) -> Result<Value, AppError> {
    if let Some(text) = output.as_str() {
        return Ok(Value::String(text.to_owned()));
    }
    if output.is_array() {
        return Ok(Value::Array(responses_content(output, true)?));
    }
    Err(AppError::BadRequest(
        "Responses-via-Anthropic tool output must be text or content blocks".into(),
    ))
}

fn push_message(messages: &mut Vec<Value>, role: &str, mut blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.append(&mut blocks);
    } else {
        messages.push(json!({"role":role,"content":blocks}));
    }
}

fn anthropic_image_source(url: &str) -> Result<Value, AppError> {
    if let Some(data) = url.strip_prefix("data:") {
        let (header, encoded) = data.split_once(',').ok_or_else(|| {
            AppError::BadRequest("Responses-via-Anthropic data image is invalid".into())
        })?;
        let media_type = header.strip_suffix(";base64").ok_or_else(|| {
            AppError::BadRequest("Responses-via-Anthropic data image must be base64".into())
        })?;
        if !matches!(
            media_type,
            "image/jpeg" | "image/png" | "image/gif" | "image/webp"
        ) || encoded.is_empty()
        {
            return Err(AppError::BadRequest(
                "Responses-via-Anthropic image media type is unsupported".into(),
            ));
        }
        return Ok(json!({"type":"base64","media_type":media_type,"data":encoded}));
    }
    if url.starts_with("https://") {
        return Ok(json!({"type":"url","url":url}));
    }
    Err(AppError::BadRequest(
        "Responses-via-Anthropic images require an HTTPS or data URL".into(),
    ))
}

fn apply_tool_choice(
    output: &mut Value,
    request: &Value,
    tools: &BTreeMap<String, ToolIdentity>,
) -> Result<(), AppError> {
    let choice = request.get("tool_choice");
    if choice.and_then(Value::as_str) == Some("none") {
        output
            .as_object_mut()
            .ok_or(AppError::Internal)?
            .remove("tools");
        return Ok(());
    }
    let mut mapped = match choice {
        None => None,
        Some(Value::String(value)) if value == "auto" => Some(json!({"type":"auto"})),
        Some(Value::String(value)) if value == "required" => Some(json!({"type":"any"})),
        Some(Value::Object(value))
            if matches!(
                value.get("type").and_then(Value::as_str),
                Some("function" | "custom")
            ) =>
        {
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| AppError::BadRequest("tool_choice name is required".into()))?;
            let wire_name = qualify(
                value
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                name,
            );
            if !tools.contains_key(&wire_name) {
                return Err(AppError::BadRequest(
                    "tool_choice references an undeclared tool".into(),
                ));
            }
            Some(json!({"type":"tool","name":wire_name}))
        }
        Some(_) => {
            return Err(AppError::BadRequest(
                "unsupported tool_choice for Responses-via-Anthropic".into(),
            ));
        }
    };
    if let Some(parallel) = request.get("parallel_tool_calls").and_then(Value::as_bool) {
        let choice = mapped.get_or_insert_with(|| json!({"type":"auto"}));
        choice["disable_parallel_tool_use"] = Value::Bool(!parallel);
    }
    if let Some(choice) = mapped {
        output["tool_choice"] = choice;
    }
    Ok(())
}

fn required_string<'a>(value: &'a Value, field: &str, label: &str) -> Result<&'a str, AppError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::BadRequest(format!("{label} is required")))
}

fn anthropic_usage(value: &Value) -> Result<Value, &'static str> {
    let required_integer = |name: &str| -> Result<i64, &'static str> {
        value
            .get(name)
            .and_then(Value::as_i64)
            .ok_or("anthropic_usage_invalid")
    };
    let optional_integer = |name: &str| -> Result<i64, &'static str> {
        match value.get(name) {
            None => Ok(0),
            Some(value) => value.as_i64().ok_or("anthropic_usage_invalid"),
        }
    };
    let input = required_integer("input_tokens")?;
    let output = required_integer("output_tokens")?;
    let cached = optional_integer("cache_read_input_tokens")?;
    let cache_write = optional_integer("cache_creation_input_tokens")?;
    if [input, output, cached, cache_write]
        .into_iter()
        .any(|tokens| !(0..=crate::api::limits::MAX_REPORTED_TOKENS).contains(&tokens))
    {
        return Err("anthropic_usage_invalid");
    }
    let total_input = input
        .checked_add(cached)
        .and_then(|tokens| tokens.checked_add(cache_write))
        .ok_or("anthropic_usage_invalid")?;
    let total = total_input
        .checked_add(output)
        .ok_or("anthropic_usage_invalid")?;
    Ok(
        json!({"input_tokens":total_input,"output_tokens":output,"total_tokens":total,
        "input_tokens_details":{"cached_tokens":cached},
        "cache_creation_input_tokens":cache_write}),
    )
}

#[derive(Clone, Copy)]
enum Terminal {
    Completed,
    Incomplete(&'static str),
}

fn terminal(reason: &str) -> Result<Terminal, &'static str> {
    match reason {
        "end_turn" | "stop_sequence" | "tool_use" => Ok(Terminal::Completed),
        "max_tokens" | "model_context_window_exceeded" => {
            Ok(Terminal::Incomplete("max_output_tokens"))
        }
        "refusal" => Ok(Terminal::Incomplete("content_filter")),
        _ => Err("anthropic_stop_reason_unsupported"),
    }
}

fn envelope(context: &Context, id: &str, created: i64, output: Vec<Value>, usage: Value) -> Value {
    json!({"id":id,"object":"response","created_at":created,"model":context.model,
        "status":"completed","error":null,"incomplete_details":null,"output":output,"usage":usage})
}

pub(in crate::api) fn buffered(context: &Context, value: &Value) -> Result<Value, &'static str> {
    if value.get("type").and_then(Value::as_str) != Some("message")
        || value.get("role").and_then(Value::as_str) != Some("assistant")
    {
        return Err("anthropic_message_invalid");
    }
    let terminal = terminal(
        value
            .get("stop_reason")
            .and_then(Value::as_str)
            .ok_or("anthropic_stop_reason_missing")?,
    )?;
    let blocks = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or("anthropic_content_invalid")?;
    let id = format!("resp_{}", Uuid::now_v7().simple());
    let mut output = Vec::new();
    let mut text_parts = Vec::new();
    let flush_text = |output: &mut Vec<Value>, text_parts: &mut Vec<Value>| {
        if !text_parts.is_empty() {
            output.push(
                json!({"id":format!("msg_{}_{}", id, output.len()),"type":"message",
                "role":"assistant","status":"completed","content":std::mem::take(text_parts)}),
            );
        }
    };
    let mut call_ids = BTreeSet::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text_parts.push(json!({"type":"output_text",
                "text":block.get("text").and_then(Value::as_str).ok_or("anthropic_text_invalid")?,
                "annotations":[]})),
            Some("tool_use") => {
                flush_text(&mut output, &mut text_parts);
                let call_id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("anthropic_tool_id_invalid")?;
                if !call_ids.insert(call_id) {
                    return Err("anthropic_tool_id_duplicate");
                }
                let input = block.get("input").ok_or("anthropic_tool_input_invalid")?;
                if !input.is_object() {
                    return Err("anthropic_tool_input_invalid");
                }
                output.push(
                    context.tool_item(
                        block
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or("anthropic_tool_name_invalid")?,
                        call_id,
                        &format!("fc_{}_{}", id, output.len()),
                        input,
                        "completed",
                    )?,
                );
            }
            Some("thinking" | "redacted_thinking") => {
                return Err("anthropic_signed_thinking_unsupported");
            }
            Some(_) | None => return Err("anthropic_content_type_unsupported"),
        }
    }
    flush_text(&mut output, &mut text_parts);
    let mut response = envelope(
        context,
        &id,
        0,
        output,
        anthropic_usage(value.get("usage").ok_or("anthropic_usage_missing")?)?,
    );
    if let Terminal::Incomplete(reason) = terminal {
        response["status"] = Value::String("incomplete".into());
        response["incomplete_details"] = json!({"reason":reason});
        if let Some(items) = response.get_mut("output").and_then(Value::as_array_mut) {
            for item in items {
                if item.get("status").is_some() {
                    item["status"] = Value::String("incomplete".into());
                }
            }
        }
    }
    Ok(response)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BlockKind {
    Text,
    Tool,
}

struct BlockState {
    kind: BlockKind,
    output_index: usize,
    wire_name: Option<String>,
    open: bool,
    accumulated: String,
}

pub(in crate::api) struct Stream {
    context: Context,
    id: String,
    created: i64,
    sequence: u64,
    started: bool,
    stopped: bool,
    bytes: usize,
    blocks: BTreeMap<u64, BlockState>,
    call_ids: BTreeSet<String>,
    output: Vec<Value>,
    usage: Map<String, Value>,
    stop_reason: Option<String>,
}

impl Stream {
    pub(in crate::api) fn new(context: Context) -> Self {
        Self {
            context,
            id: format!("resp_{}", Uuid::now_v7().simple()),
            created: 0,
            sequence: 0,
            started: false,
            stopped: false,
            bytes: 0,
            blocks: BTreeMap::new(),
            call_ids: BTreeSet::new(),
            output: Vec::new(),
            usage: Map::new(),
            stop_reason: None,
        }
    }

    fn event(&mut self, name: &str, mut value: Value) -> Result<Vec<u8>, &'static str> {
        value["type"] = Value::String(name.to_owned());
        value["sequence_number"] = self.sequence.into();
        self.sequence = self.sequence.saturating_add(1);
        let body = serde_json::to_string(&value).map_err(|_| "event_serialization")?;
        Ok(format!("event: {name}\ndata: {body}\n\n").into_bytes())
    }

    pub(in crate::api) fn observe(
        &mut self,
        event_name: Option<&str>,
        value: &Value,
    ) -> Result<Vec<Vec<u8>>, &'static str> {
        if self.stopped {
            return Err("anthropic_event_after_stop");
        }
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .or(event_name)
            .ok_or("anthropic_event_type_missing")?;
        if event_name.is_some_and(|name| name != kind) {
            return Err("anthropic_event_type_mismatch");
        }
        match kind {
            "ping" => Ok(Vec::new()),
            "message_start" => self.message_start(value),
            "content_block_start" => self.block_start(value),
            "content_block_delta" => self.block_delta(value),
            "content_block_stop" => self.block_stop(value),
            "message_delta" => self.message_delta(value),
            "message_stop" => self.message_stop(),
            "error" => Err("anthropic_stream_error"),
            _ => Err("anthropic_event_type_unsupported"),
        }
    }

    fn message_start(&mut self, value: &Value) -> Result<Vec<Vec<u8>>, &'static str> {
        if self.started {
            return Err("anthropic_message_start_duplicate");
        }
        let message = value.get("message").ok_or("anthropic_message_missing")?;
        if message.get("type").and_then(Value::as_str) != Some("message")
            || message.get("role").and_then(Value::as_str) != Some("assistant")
        {
            return Err("anthropic_message_invalid");
        }
        self.merge_usage(message.get("usage"))?;
        self.started = true;
        let mut response = envelope(
            &self.context,
            &self.id,
            self.created,
            Vec::new(),
            Value::Null,
        );
        response["status"] = Value::String("in_progress".into());
        Ok(vec![
            self.event("response.created", json!({"response":response.clone()}))?,
            self.event("response.in_progress", json!({"response":response}))?,
        ])
    }

    fn block_start(&mut self, value: &Value) -> Result<Vec<Vec<u8>>, &'static str> {
        self.require_started()?;
        if self.blocks.len() >= MAX_ITEMS || self.output.len() >= MAX_ITEMS {
            return Err("anthropic_item_limit");
        }
        let index = value
            .get("index")
            .and_then(Value::as_u64)
            .ok_or("anthropic_block_index_invalid")?;
        if self.blocks.contains_key(&index) {
            return Err("anthropic_block_index_duplicate");
        }
        let block = value
            .get("content_block")
            .ok_or("anthropic_content_block_missing")?;
        let output_index = self.output.len();
        let (kind, wire_name, item) = match block.get("type").and_then(Value::as_str) {
            Some("text") => (
                {
                    if block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        != ""
                    {
                        return Err("anthropic_text_start_invalid");
                    }
                    BlockKind::Text
                },
                None,
                json!({"id":format!("msg_{}_{}",self.id,output_index),"type":"message",
                    "role":"assistant","status":"in_progress",
                    "content":[{"type":"output_text","text":"","annotations":[]}]}),
            ),
            Some("tool_use") => {
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                if input.as_object().is_none_or(|input| !input.is_empty()) {
                    return Err("anthropic_tool_start_input_invalid");
                }
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("anthropic_tool_name_invalid")?;
                let call_id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("anthropic_tool_id_invalid")?;
                if !self.call_ids.insert(call_id.to_owned()) {
                    return Err("anthropic_tool_id_duplicate");
                }
                (
                    BlockKind::Tool,
                    Some(name.to_owned()),
                    self.context.tool_placeholder(
                        name,
                        call_id,
                        &format!("fc_{}_{}", self.id, output_index),
                    )?,
                )
            }
            Some("thinking" | "redacted_thinking") => {
                return Err("anthropic_signed_thinking_unsupported");
            }
            _ => return Err("anthropic_content_type_unsupported"),
        };
        self.output.push(item.clone());
        self.blocks.insert(
            index,
            BlockState {
                kind,
                output_index,
                wire_name,
                open: true,
                accumulated: String::new(),
            },
        );
        let mut events = vec![self.event(
            "response.output_item.added",
            json!({"output_index":output_index,"item":item.clone()}),
        )?];
        if kind == BlockKind::Text {
            events.push(self.event(
                "response.content_part.added",
                json!({"item_id":item["id"],"output_index":output_index,"content_index":0,
                    "part":{"type":"output_text","text":"","annotations":[]}}),
            )?);
        }
        Ok(events)
    }

    fn block_delta(&mut self, value: &Value) -> Result<Vec<Vec<u8>>, &'static str> {
        self.require_started()?;
        let index = value
            .get("index")
            .and_then(Value::as_u64)
            .ok_or("anthropic_block_index_invalid")?;
        let delta = value.get("delta").ok_or("anthropic_delta_missing")?;
        let state = self
            .blocks
            .get_mut(&index)
            .ok_or("anthropic_block_missing")?;
        if !state.open {
            return Err("anthropic_block_closed");
        }
        let fragment = match (state.kind, delta.get("type").and_then(Value::as_str)) {
            (BlockKind::Text, Some("text_delta")) => delta
                .get("text")
                .and_then(Value::as_str)
                .ok_or("anthropic_text_invalid")?,
            (BlockKind::Tool, Some("input_json_delta")) => delta
                .get("partial_json")
                .and_then(Value::as_str)
                .ok_or("anthropic_tool_input_invalid")?,
            _ => return Err("anthropic_delta_type_mismatch"),
        };
        self.bytes = self
            .bytes
            .checked_add(fragment.len())
            .ok_or("anthropic_accumulation_limit")?;
        if self.bytes > MAX_ACCUMULATED_BYTES {
            return Err("anthropic_accumulation_limit");
        }
        state.accumulated.push_str(fragment);
        let output_index = state.output_index;
        let item_id = self.output[output_index]["id"].clone();
        match state.kind {
            BlockKind::Text => {
                self.output[output_index]["content"][0]["text"] =
                    Value::String(state.accumulated.clone());
                Ok(vec![self.event(
                    "response.output_text.delta",
                    json!({"item_id":item_id,"output_index":output_index,
                        "content_index":0,"delta":fragment}),
                )?])
            }
            BlockKind::Tool => {
                if self.output[output_index]["type"] == "function_call" {
                    self.output[output_index]["arguments"] =
                        Value::String(state.accumulated.clone());
                    Ok(vec![self.event(
                        "response.function_call_arguments.delta",
                        json!({"item_id":item_id,"output_index":output_index,"delta":fragment}),
                    )?])
                } else {
                    Ok(Vec::new())
                }
            }
        }
    }

    fn block_stop(&mut self, value: &Value) -> Result<Vec<Vec<u8>>, &'static str> {
        let index = value
            .get("index")
            .and_then(Value::as_u64)
            .ok_or("anthropic_block_index_invalid")?;
        let state = self
            .blocks
            .get_mut(&index)
            .ok_or("anthropic_block_missing")?;
        if !state.open {
            return Err("anthropic_block_closed");
        }
        state.open = false;
        if state.kind == BlockKind::Tool {
            let parsed = crate::api::sse::parse_unique_json(state.accumulated.as_bytes())
                .map_err(|_| "anthropic_tool_input_invalid")?;
            if !parsed.is_object() {
                return Err("anthropic_tool_input_invalid");
            }
            let output_index = state.output_index;
            let wire_name = state
                .wire_name
                .as_deref()
                .ok_or("anthropic_tool_name_invalid")?
                .to_owned();
            let call_id = self.output[output_index]["call_id"]
                .as_str()
                .ok_or("anthropic_tool_id_invalid")?
                .to_owned();
            let item_id = self.output[output_index]["id"]
                .as_str()
                .ok_or("anthropic_tool_id_invalid")?
                .to_owned();
            self.output[output_index] =
                self.context
                    .tool_item(&wire_name, &call_id, &item_id, &parsed, "in_progress")?;
        }
        Ok(Vec::new())
    }

    fn message_delta(&mut self, value: &Value) -> Result<Vec<Vec<u8>>, &'static str> {
        self.require_started()?;
        if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
            terminal(reason)?;
            if self.stop_reason.is_some() {
                return Err("anthropic_stop_reason_duplicate");
            }
            self.stop_reason = Some(reason.to_owned());
        }
        self.merge_usage(value.get("usage"))?;
        Ok(Vec::new())
    }

    fn message_stop(&mut self) -> Result<Vec<Vec<u8>>, &'static str> {
        self.require_started()?;
        if self.blocks.values().any(|state| state.open) {
            return Err("anthropic_block_incomplete");
        }
        let terminal = terminal(
            self.stop_reason
                .as_deref()
                .ok_or("anthropic_stop_reason_missing")?,
        )?;
        let usage = anthropic_usage(&Value::Object(self.usage.clone()))?;
        let incomplete = matches!(terminal, Terminal::Incomplete(_));
        let mut events = Vec::new();
        for output_index in 0..self.output.len() {
            let mut item = self.output[output_index].clone();
            item["status"] = Value::String(
                if incomplete {
                    "incomplete"
                } else {
                    "completed"
                }
                .into(),
            );
            let item_id = item["id"].clone();
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    let part = item["content"][0].clone();
                    events.push(self.event(
                        "response.output_text.done",
                        json!({"item_id":item_id,"output_index":output_index,"content_index":0,
                            "text":part["text"]}),
                    )?);
                    events.push(self.event(
                        "response.content_part.done",
                        json!({"item_id":item_id,"output_index":output_index,"content_index":0,
                            "part":part}),
                    )?);
                }
                Some("function_call") => events.push(self.event(
                    "response.function_call_arguments.done",
                    json!({"item_id":item_id,"output_index":output_index,
                        "arguments":item["arguments"]}),
                )?),
                Some("custom_tool_call") => {
                    events.push(self.event(
                        "response.custom_tool_call_input.delta",
                        json!({"item_id":item_id,"output_index":output_index,"delta":item["input"]}),
                    )?);
                    events.push(self.event(
                        "response.custom_tool_call_input.done",
                        json!({"item_id":item_id,"output_index":output_index,"input":item["input"]}),
                    )?);
                }
                _ => return Err("anthropic_output_item_invalid"),
            }
            self.output[output_index] = item.clone();
            events.push(self.event(
                "response.output_item.done",
                json!({"output_index":output_index,"item":item}),
            )?);
        }
        let mut response = envelope(
            &self.context,
            &self.id,
            self.created,
            self.output.clone(),
            usage,
        );
        let event = match terminal {
            Terminal::Completed => "response.completed",
            Terminal::Incomplete(reason) => {
                response["status"] = Value::String("incomplete".into());
                response["incomplete_details"] = json!({"reason":reason});
                "response.incomplete"
            }
        };
        events.push(self.event(event, json!({"response":response}))?);
        self.stopped = true;
        Ok(events)
    }

    pub(in crate::api) fn finish(&self) -> Result<(), &'static str> {
        self.stopped
            .then_some(())
            .ok_or("anthropic_message_stop_missing")
    }

    fn require_started(&self) -> Result<(), &'static str> {
        self.started
            .then_some(())
            .ok_or("anthropic_message_start_missing")
    }

    fn merge_usage(&mut self, usage: Option<&Value>) -> Result<(), &'static str> {
        let Some(usage) = usage else {
            return Ok(());
        };
        let usage = usage.as_object().ok_or("anthropic_usage_invalid")?;
        for field in [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ] {
            if let Some(value) = usage.get(field) {
                let next = value.as_i64().ok_or("anthropic_usage_invalid")?;
                if next < 0
                    || self
                        .usage
                        .get(field)
                        .and_then(Value::as_i64)
                        .is_some_and(|current| next < current)
                {
                    return Err("anthropic_usage_invalid");
                }
                self.usage.insert(field.to_owned(), value.clone());
            }
        }
        Ok(())
    }
}

pub(in crate::api) fn error_body(value: &Value) -> Result<Value, &'static str> {
    let error = value
        .get("error")
        .and_then(Value::as_object)
        .ok_or("anthropic_error_invalid")?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .ok_or("anthropic_error_invalid")?;
    let kind = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("upstream_error");
    Ok(json!({"error":{"message":message,"type":kind,"code":kind}}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_preserves_namespaced_custom_tools_images_and_pairing() {
        let mut request = json!({
            "model":"claude-route","stream":true,"instructions":"system",
            "tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}],
            "input":[
                {"role":"developer","content":"policy"},
                {"role":"user","content":[{"type":"input_text","text":"look"},{"type":"input_image","image_url":"data:image/png;base64,fixture"}]},
                {"type":"custom_tool_call","namespace":"editor","name":"patch","call_id":"call-a","input":"diff"},
                {"type":"custom_tool_call_output","call_id":"call-a","output":"ok"}
            ],
            "tool_choice":{"type":"function","namespace":"editor","name":"patch"},
            "parallel_tool_calls":false,
            "max_output_tokens":4096
        });
        let context = prepare("claude-upstream", &mut request).unwrap();
        assert_eq!(request["model"], "claude-upstream");
        assert_eq!(request["system"][0]["text"], "system");
        assert_eq!(request["system"][1]["text"], "policy");
        assert_eq!(
            request["messages"][0]["content"][1]["source"]["type"],
            "base64"
        );
        assert_eq!(
            request["messages"][1]["content"][0]["name"],
            "editor__patch"
        );
        assert_eq!(request["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(request["tool_choice"]["name"], "editor__patch");
        assert_eq!(request["tool_choice"]["disable_parallel_tool_use"], true);
        assert_eq!(context.model, "claude-route");
    }

    #[test]
    fn unrepresentable_features_and_broken_tool_history_fail_before_dispatch() {
        for mut request in [
            json!({"model":"claude","input":"hello","previous_response_id":"resp_previous"}),
            json!({"model":"claude","input":[{"type":"compaction","encrypted_content":"opaque"}]}),
            json!({"model":"claude","input":"hello","tools":[{"type":"web_search"}]}),
            json!({"model":"claude","input":"hello","store":true}),
            json!({"model":"claude","input":"hello","reasoning":{"effort":"high"}}),
            json!({"model":"claude","input":"hello","reasoning":{"summary":"auto"}}),
            json!({"model":"claude","input":"hello","include":["reasoning.encrypted_content"]}),
            json!({"model":"claude","input":[
                {"role":"user","content":"hello"},{"role":"developer","content":"late policy"}
            ]}),
            json!({"model":"claude","input":[{"type":"function_call","call_id":"a","name":"x","arguments":"{}"}]}),
            json!({"model":"claude","input":[{"type":"function_call_output","call_id":"a","output":"x"}]}),
        ] {
            assert!(prepare("claude", &mut request).is_err());
        }
    }

    #[test]
    fn buffered_response_restores_custom_tool_identity_and_cache_usage() {
        let mut request = json!({"model":"claude","input":"hello","tools":[
            {"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}
        ]});
        let context = prepare("claude", &mut request).unwrap();
        let response = buffered(&context, &json!({
            "id":"msg_1","type":"message","role":"assistant","model":"claude",
            "content":[{"type":"tool_use","id":"toolu_1","name":"editor__patch","input":{"input":"diff"}}],
            "stop_reason":"tool_use","usage":{"input_tokens":5,"output_tokens":3,
                "cache_read_input_tokens":7,"cache_creation_input_tokens":2}
        })).unwrap();
        assert_eq!(response["model"], "claude");
        assert_eq!(response["output"][0]["type"], "custom_tool_call");
        assert_eq!(response["output"][0]["namespace"], "editor");
        assert_eq!(response["output"][0]["input"], "diff");
        assert_eq!(response["usage"]["input_tokens"], 14);
        assert_eq!(
            response["usage"]["input_tokens_details"]["cached_tokens"],
            7
        );
        assert_eq!(response["usage"]["cache_creation_input_tokens"], 2);
    }

    #[test]
    fn streaming_ping_is_silent_and_terminal_usage_completes_once() {
        let mut request = json!({"model":"claude","input":"hello","stream":true});
        let context = prepare("claude-upstream", &mut request).unwrap();
        let mut stream = Stream::new(context);
        let mut output = Vec::new();
        for (event, value) in [
            (
                "message_start",
                json!({"type":"message_start","message":{"type":"message","role":"assistant","usage":{"input_tokens":5,"cache_read_input_tokens":2}}}),
            ),
            ("ping", json!({"type":"ping"})),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ] {
            let events = stream.observe(Some(event), &value).unwrap();
            if event == "ping" {
                assert!(events.is_empty());
            }
            output.extend(events);
        }
        stream.finish().unwrap();
        let wire = String::from_utf8(output.concat()).unwrap();
        assert_eq!(wire.matches("event: response.completed").count(), 1);
        assert!(wire.contains("\"input_tokens\":7"));
        assert!(wire.contains("\"output_tokens\":2"));
    }

    #[test]
    fn signed_thinking_and_eof_without_message_stop_fail_closed() {
        let mut request = json!({"model":"claude","input":"hello","stream":true});
        let context = prepare("claude", &mut request).unwrap();
        let mut stream = Stream::new(context);
        stream
            .observe(
                Some("message_start"),
                &json!({"type":"message_start","message":{
            "type":"message","role":"assistant","usage":{"input_tokens":1}}}),
            )
            .unwrap();
        assert!(
            stream
                .observe(
                    Some("content_block_start"),
                    &json!({"type":"content_block_start",
            "index":0,"content_block":{"type":"thinking","thinking":"","signature":""}})
                )
                .is_err()
        );
        assert!(stream.finish().is_err());
    }

    #[test]
    fn captured_codex_multi_agent_fixture_normalizes_to_anthropic_messages() {
        let mut request: Value = serde_json::from_str(include_str!(
            "kimi_transport/fixtures/codex-multi-agent-v2.json"
        ))
        .unwrap();
        crate::api::request_normalization::normalize_codex_multi_agent_v2(&mut request, true)
            .unwrap();
        prepare("claude-upstream", &mut request).unwrap();
        assert_eq!(request["model"], "claude-upstream");
        assert_eq!(request["messages"][0]["role"], "user");
        assert_eq!(request["messages"][1]["role"], "assistant");
        assert_eq!(
            request["messages"][1]["content"][0]["name"],
            "collaboration__spawn_agent"
        );
        assert_eq!(
            request["messages"][3]["content"][0]["name"],
            "followup_task"
        );
        assert!(
            request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "collaboration__spawn_agent")
        );
        assert!(!request.to_string().contains("encrypted_content"));
    }

    #[test]
    fn streaming_custom_tool_restores_identity_after_json_delta() {
        let mut request = json!({"model":"claude","input":"hello","stream":true,"tools":[
            {"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}
        ]});
        let context = prepare("claude-upstream", &mut request).unwrap();
        let mut stream = Stream::new(context);
        let mut output = Vec::new();
        for (event, value) in [
            (
                "message_start",
                json!({"type":"message_start","message":{"type":"message","role":"assistant","usage":{"input_tokens":5}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"editor__patch","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"diff\"}"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ] {
            output.extend(stream.observe(Some(event), &value).unwrap());
        }
        stream.finish().unwrap();
        let wire = String::from_utf8(output.concat()).unwrap();
        assert!(wire.contains("\"type\":\"custom_tool_call\""));
        assert!(wire.contains("\"namespace\":\"editor\""));
        assert!(wire.contains("\"name\":\"patch\""));
        assert!(wire.contains("\"input\":\"diff\""));
    }
}
