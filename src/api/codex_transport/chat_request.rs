use super::*;

fn invalid(detail: &str) -> AppError {
    AppError::BadRequest(format!("Codex text Chat {detail}"))
}

fn non_null<'a>(object: &'a Map<String, Value>, field: &str) -> Option<&'a Value> {
    object.get(field).filter(|value| !value.is_null())
}

fn required_string<'a>(object: &'a Map<String, Value>, field: &str) -> Result<&'a str, AppError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid("tool identity fields must be non-empty strings"))
}

pub(super) fn translate_controls(object: &mut Map<String, Value>) -> Result<(), AppError> {
    for field in [
        "functions",
        "function_call",
        "audio",
        "prediction",
        "input",
        "instructions",
        "previous_response_id",
        "conversation",
        "reasoning",
        "text",
        "background",
        "truncation",
        "context_management",
        "generate",
    ] {
        if non_null(object, field).is_some() {
            return Err(invalid(&format!(
                "cannot preserve {field} semantics on this route"
            )));
        }
        object.remove(field);
    }
    for (field, neutral) in [
        ("logprobs", json!(false)),
        ("top_logprobs", json!(0)),
        ("logit_bias", json!({})),
        ("modalities", json!(["text"])),
    ] {
        if non_null(object, field).is_some_and(|value| value != &neutral) {
            return Err(invalid(&format!(
                "cannot preserve {field} semantics on this route"
            )));
        }
        object.remove(field);
    }
    if let Some(value) = non_null(object, "parallel_tool_calls") {
        if !value.is_boolean() {
            return Err(invalid("parallel_tool_calls must be a boolean"));
        }
    } else {
        object.remove("parallel_tool_calls");
    }
    if let Some(value) = non_null(object, "tools") {
        let tools = value
            .as_array()
            .ok_or_else(|| invalid("tools must be an array"))?;
        let tools = tools
            .iter()
            .map(translate_tool)
            .collect::<Result<Vec<_>, _>>()?;
        object.insert("tools".into(), Value::Array(tools));
    } else {
        object.remove("tools");
    }
    if let Some(value) = non_null(object, "tool_choice") {
        let choice = match value {
            Value::String(choice) if matches!(choice.as_str(), "auto" | "none" | "required") => {
                value.clone()
            }
            Value::Object(choice)
                if choice.get("type").and_then(Value::as_str) == Some("function") =>
            {
                let function = choice
                    .get("function")
                    .and_then(Value::as_object)
                    .ok_or_else(|| invalid("tool_choice.function must be an object"))?;
                json!({"type": "function", "name": required_string(function, "name")?})
            }
            _ => {
                return Err(invalid(
                    "tool_choice must be auto, none, required, or a named function",
                ));
            }
        };
        object.insert("tool_choice".into(), choice);
    } else {
        object.remove("tool_choice");
    }
    let mut text = Map::new();
    if let Some(format) = non_null(object, "response_format") {
        let format = format
            .as_object()
            .ok_or_else(|| invalid("response_format must be an object"))?;
        let translated = match format.get("type").and_then(Value::as_str) {
            Some("text") => None,
            Some("json_object") => Some(json!({"type": "json_object"})),
            Some("json_schema") => {
                let mut schema = format
                    .get("json_schema")
                    .and_then(Value::as_object)
                    .cloned()
                    .ok_or_else(|| invalid("response_format.json_schema must be an object"))?;
                required_string(&schema, "name")?;
                if !schema.get("schema").is_some_and(Value::is_object) {
                    return Err(invalid(
                        "response_format.json_schema.schema must be an object",
                    ));
                }
                schema.insert("type".into(), json!("json_schema"));
                Some(Value::Object(schema))
            }
            _ => return Err(invalid("response_format type is unsupported")),
        };
        if let Some(format) = translated {
            text.insert("format".into(), format);
        }
    }
    if let Some(verbosity) = non_null(object, "verbosity") {
        if !matches!(verbosity.as_str(), Some("low" | "medium" | "high")) {
            return Err(invalid("verbosity must be low, medium, or high"));
        }
        text.insert("verbosity".into(), verbosity.clone());
    }
    object.remove("verbosity");
    if !text.is_empty() {
        object.insert("text".into(), Value::Object(text));
    }
    if let Some(effort) = non_null(object, "reasoning_effort") {
        if !matches!(
            effort.as_str(),
            Some("none" | "minimal" | "low" | "medium" | "high" | "xhigh")
        ) {
            return Err(invalid("reasoning_effort is unsupported"));
        }
        object.insert("reasoning".into(), json!({"effort": effort}));
    }
    object.remove("reasoning_effort");
    Ok(())
}

fn translate_tool(value: &Value) -> Result<Value, AppError> {
    let tool = value
        .as_object()
        .ok_or_else(|| invalid("tools entries must be objects"))?;
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        return Err(invalid("supports function tools only"));
    }
    let mut function = tool
        .get("function")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| invalid("tools.function must be an object"))?;
    required_string(&function, "name")?;
    if non_null(&function, "parameters").is_some_and(|value| !value.is_object())
        || non_null(&function, "description").is_some_and(|value| !value.is_string())
        || non_null(&function, "strict").is_some_and(|value| !value.is_boolean())
    {
        return Err(invalid(
            "function parameters, description, or strict has an invalid type",
        ));
    }
    if non_null(&function, "strict").is_none() {
        function.insert("strict".into(), json!(false));
    }
    function.insert("type".into(), json!("function"));
    Ok(Value::Object(function))
}

pub(super) fn validate_message(message: &Map<String, Value>, role: &str) -> Result<(), AppError> {
    for field in ["function_call", "audio"] {
        if non_null(message, field).is_some() {
            return Err(invalid(&format!(
                "cannot preserve message.{field} semantics on this route"
            )));
        }
    }
    if non_null(message, "name").is_some() && role != "tool" {
        return Err(invalid(
            "cannot preserve named message participants on this route",
        ));
    }
    if let Some(refusal) = non_null(message, "refusal")
        && (role != "assistant" || !refusal.is_string())
    {
        return Err(invalid("refusal requires assistant text"));
    }
    if role != "assistant" && non_null(message, "tool_calls").is_some() {
        return Err(invalid("tool_calls requires the assistant role"));
    }
    if role != "tool" && non_null(message, "tool_call_id").is_some() {
        return Err(invalid("tool_call_id requires the tool role"));
    }
    Ok(())
}

pub(super) fn assistant_calls(
    message: &Map<String, Value>,
    role: &str,
) -> Result<Vec<Value>, AppError> {
    let Some(calls) = non_null(message, "tool_calls") else {
        return Ok(Vec::new());
    };
    if role != "assistant" {
        return Err(invalid("tool_calls requires the assistant role"));
    }
    let calls = calls
        .as_array()
        .ok_or_else(|| invalid("tool_calls must be an array"))?;
    let mut identities = BTreeSet::new();
    calls
        .iter()
        .map(|call| {
            let call = call
                .as_object()
                .ok_or_else(|| invalid("tool_calls entries must be objects"))?;
            if call.get("type").and_then(Value::as_str) != Some("function") {
                return Err(invalid("supports function tool_calls only"));
            }
            let call_id = required_string(call, "id")?;
            if !identities.insert(call_id) {
                return Err(invalid("tool_calls ids must be unique"));
            }
            let function = call
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| invalid("tool_calls.function must be an object"))?;
            let arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("tool_calls.function.arguments must be a string"))?;
            Ok(json!({"type": "function_call", "call_id": call_id,
            "name": required_string(function, "name")?, "arguments": arguments}))
        })
        .collect()
}

pub(super) fn tool_result(
    message: &Map<String, Value>,
    input: &[Value],
) -> Result<Value, AppError> {
    let call_id = required_string(message, "tool_call_id")?;
    if let Some(name) = non_null(message, "name") {
        let call = input
            .iter()
            .rev()
            .find(|item| item["type"] == "function_call" && item["call_id"] == call_id);
        if !name.is_string() || call.is_none_or(|call| &call["name"] != name) {
            return Err(invalid(
                "tool result name must match its preceding tool call",
            ));
        }
    }
    let content = message
        .get("content")
        .ok_or_else(|| invalid("tool result requires content"))?;
    Ok(
        json!({"type": "function_call_output", "call_id": call_id, "output": chat_text_content(content)?}),
    )
}

pub(super) fn user_content(content: Option<&Value>) -> Result<Value, AppError> {
    let content = content.ok_or_else(|| invalid("user message requires content"))?;
    if let Some(text) = content.as_str() {
        return Ok(json!([{"type": "input_text", "text": text}]));
    }
    let parts = content
        .as_array()
        .filter(|parts| !parts.is_empty())
        .ok_or_else(|| invalid("user content must be text or content parts"))?;
    let parts = parts
        .iter()
        .map(|part| match part.get("type").and_then(Value::as_str) {
            Some("text") => part
                .get("text")
                .and_then(Value::as_str)
                .map(|text| json!({"type": "input_text", "text": text}))
                .ok_or_else(|| invalid("text content part requires text")),
            Some("image_url") => {
                let image = part
                    .get("image_url")
                    .and_then(Value::as_object)
                    .ok_or_else(|| invalid("image_url content part requires an object"))?;
                let url = required_string(image, "url")?;
                let detail = non_null(image, "detail").cloned().unwrap_or(json!("auto"));
                if !matches!(detail.as_str(), Some("auto" | "low" | "high" | "original")) {
                    return Err(invalid("image_url.detail is unsupported"));
                }
                Ok(json!({"type": "input_image", "image_url": url, "detail": detail}))
            }
            _ => Err(invalid(
                "user content part type cannot be represented on this route",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Value::Array(parts))
}
