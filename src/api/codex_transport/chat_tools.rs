use super::*;

pub(super) struct StreamingTool {
    index: usize,
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    done: bool,
}

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str, &'static str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or("upstream_invalid_response")
}

fn identity<'a>(value: &'a Value, field: &str) -> Result<&'a str, &'static str> {
    let result = string(value, field)?;
    if result.is_empty() {
        return Err("upstream_invalid_response");
    }
    Ok(result)
}

fn output_index(event: &Value) -> Result<usize, &'static str> {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .filter(|index| *index < MAX_OUTPUT_ITEMS)
        .ok_or("upstream_invalid_response")
}

pub(super) fn chat_call(item: &Value) -> Result<Value, &'static str> {
    Ok(
        json!({"id": identity(item, "call_id")?, "type": "function", "function": {
            "name": identity(item, "name")?, "arguments": string(item, "arguments")?
        }}),
    )
}

impl CodexChatStreamTranslator {
    pub(super) fn translate_tool_item(
        &mut self,
        event: &Value,
        done: bool,
    ) -> Result<Option<Bytes>, &'static str> {
        if self.output_kind == Some(ChatOutputKind::Refusal) {
            return Err("upstream_unsupported_chat_output");
        }
        let position = output_index(event)?;
        let item = event.get("item").ok_or("upstream_invalid_response")?;
        let call = chat_call(item)?;
        let item_id = identity(item, "id")?;
        let arguments = string(item, "arguments")?;
        if let Some(tool) = self.tools.get_mut(&position) {
            if tool.item_id != item_id
                || tool.call_id != call["id"]
                || tool.name != call["function"]["name"]
                || !done
            {
                return Err("upstream_invalid_response");
            }
            let remaining = arguments
                .strip_prefix(&tool.arguments)
                .ok_or("upstream_invalid_response")?
                .to_owned();
            if tool.done && !remaining.is_empty() {
                return Err("upstream_invalid_response");
            }
            tool.arguments = arguments.to_owned();
            tool.done = true;
            let index = tool.index;
            return if remaining.is_empty() {
                Ok(None)
            } else {
                self.tool_delta_chunk(json!({"index": index, "function": {"arguments": remaining}}))
                    .map(Some)
            };
        }
        if self.tools.len() >= MAX_OUTPUT_ITEMS
            || self
                .tools
                .values()
                .any(|tool| tool.call_id == call["id"] || tool.item_id == item_id)
        {
            return Err("upstream_invalid_response");
        }
        let index = self.tools.len();
        self.tools.insert(
            position,
            StreamingTool {
                index,
                item_id: item_id.into(),
                call_id: identity(item, "call_id")?.into(),
                name: identity(item, "name")?.into(),
                arguments: arguments.into(),
                done,
            },
        );
        self.tool_delta_chunk(json!({"index": index, "id": call["id"], "type": "function", "function": call["function"]})).map(Some)
    }

    pub(super) fn translate_tool_arguments(
        &mut self,
        event: &Value,
        done: bool,
    ) -> Result<Option<Bytes>, &'static str> {
        let position = output_index(event)?;
        let tool = self
            .tools
            .get_mut(&position)
            .ok_or("upstream_invalid_response")?;
        if identity(event, "item_id")? != tool.item_id {
            return Err("upstream_invalid_response");
        }
        let arguments = string(event, if done { "arguments" } else { "delta" })?;
        let delta = if done {
            arguments
                .strip_prefix(&tool.arguments)
                .ok_or("upstream_invalid_response")?
        } else {
            if tool.done {
                return Err("upstream_invalid_response");
            }
            arguments
        };
        if tool.done && !delta.is_empty() {
            return Err("upstream_invalid_response");
        }
        if tool.arguments.len().saturating_add(delta.len()) > MAX_PROXY_RESPONSE_BODY {
            return Err("upstream_response_too_large");
        }
        let delta = delta.to_owned();
        tool.arguments.push_str(&delta);
        tool.done = done;
        let index = tool.index;
        if delta.is_empty() {
            return Ok(None);
        }
        self.tool_delta_chunk(json!({"index": index, "function": {"arguments": delta}}))
            .map(Some)
    }

    pub(super) fn complete_tools(
        &mut self,
        response: &Value,
        output: &mut Vec<u8>,
    ) -> Result<(), &'static str> {
        let items = response
            .get("output")
            .and_then(Value::as_array)
            .ok_or("upstream_invalid_response")?;
        for (position, tool) in &self.tools {
            let item = items.get(*position).ok_or("upstream_invalid_response")?;
            if item.get("type").and_then(Value::as_str) != Some("function_call")
                || identity(item, "call_id")? != tool.call_id
            {
                return Err("upstream_invalid_response");
            }
        }
        for (position, item) in items.iter().enumerate() {
            if item.get("type").and_then(Value::as_str) == Some("function_call")
                && let Some(chunk) = self
                    .translate_tool_item(&json!({"output_index": position, "item": item}), true)?
            {
                output.extend_from_slice(&chunk);
            }
        }
        Ok(())
    }

    fn tool_delta_chunk(&mut self, call: Value) -> Result<Bytes, &'static str> {
        let mut delta = json!({"tool_calls": [call]});
        if !std::mem::replace(&mut self.started, true) {
            delta["role"] = json!("assistant");
        }
        chat_sse_chunk(
            &self.id,
            &self.model,
            self.created,
            json!([{"index": 0, "delta": delta, "finish_reason": null}]),
            None,
        )
    }
}
