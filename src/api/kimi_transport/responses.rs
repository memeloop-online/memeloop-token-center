use super::responses_request::{ToolIdentity, tools};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

const MAX_ACCUMULATED_BYTES: usize = 8 * 1024 * 1024;
const MAX_ITEMS: usize = 512;

pub(in crate::api) use crate::provider::ResponsesViaChatDialect;

#[derive(Clone)]
pub(in crate::api) struct Context {
    model: String,
    tools: BTreeMap<String, ToolIdentity>,
    usage_dialect: ResponsesViaChatDialect,
    /// The source request carried a compaction_trigger control, so the
    /// upstream response must be wrapped as an opaque compaction checkpoint
    /// item instead of ordinary message output.
    compaction: bool,
}

impl Context {
    #[cfg(test)]
    pub(in crate::api) fn new(request: &Value) -> Self {
        Self::with_dialect(request, ResponsesViaChatDialect::OpenAiChatV1)
    }

    #[cfg(test)]
    pub(in crate::api) fn for_kimi(request: &Value) -> Self {
        Self::with_dialect(request, ResponsesViaChatDialect::KimiV1)
    }

    pub(in crate::api) fn with_dialect(
        request: &Value,
        usage_dialect: ResponsesViaChatDialect,
    ) -> Self {
        Self {
            model: request["model"].as_str().unwrap_or("").into(),
            tools: tools(request)
                .into_iter()
                .map(|(name, (identity, _))| (name, identity))
                .collect(),
            usage_dialect,
            compaction: request["input"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item["type"] == "compaction_trigger")
            }),
        }
    }

    pub(in crate::api) fn uses_kimi_dialect(&self) -> bool {
        self.usage_dialect == ResponsesViaChatDialect::KimiV1
    }

    pub(in crate::api) fn is_compaction(&self) -> bool {
        self.compaction
    }

    fn tool_item(&self, call: &Value, id: &str) -> Value {
        let wire_name = call["function"]["name"].as_str().unwrap_or("");
        let identity = self.tools.get(wire_name);
        let custom = identity.is_some_and(|identity| identity.custom);
        let arguments = call["function"]["arguments"].as_str().unwrap_or("");
        let mut item = json!({"type":if custom {"custom_tool_call"} else {"function_call"},
            "id":id,"call_id":call["id"],"status":"completed",
            "name":identity.map_or(wire_name, |identity| identity.name.as_str())});
        if let Some(identity) = identity.filter(|identity| !identity.namespace.is_empty()) {
            item["namespace"] = Value::String(identity.namespace.clone());
        }
        if custom {
            let parsed = serde_json::from_str::<Value>(arguments).ok();
            item["input"] = Value::String(
                parsed
                    .as_ref()
                    .and_then(|value| value["input"].as_str())
                    .unwrap_or(arguments)
                    .into(),
            );
        } else {
            item["arguments"] = Value::String(arguments.into());
            if identity.is_some_and(|identity| {
                identity.namespace == "collaboration"
                    && matches!(
                        identity.name.as_str(),
                        "spawn_agent" | "send_message" | "followup_task"
                    )
            }) {
                // Codex uses this explicit empty marker to distinguish a
                // plaintext collaboration message from encrypted tool
                // arguments. Omitting it makes the child task unreadable to
                // Responses-via-Chat providers.
                item["encrypted_function_args"] = json!([]);
            }
        }
        item
    }

    fn validate_custom_call(&self, call: &Value) -> Result<(), &'static str> {
        let name = call["function"]["name"].as_str().unwrap_or("");
        if self.tools.get(name).is_some_and(|tool| tool.custom) {
            let args = call["function"]["arguments"].as_str().unwrap_or("");
            let parsed = crate::api::sse::parse_unique_json(args.as_bytes())
                .map_err(|_| "tool_arguments_invalid")?;
            if parsed["input"].as_str().is_none() {
                return Err("tool_arguments_invalid");
            }
        }
        Ok(())
    }
}

/// Self-contained opaque checkpoint blob. The client stores it and echoes it
/// back verbatim without inspecting it; the version prefix leaves room for a
/// future readable mapping without invalidating checkpoints already stored.
fn compaction_blob(text: &str) -> String {
    use base64::Engine as _;
    format!(
        "mtc-compact-v1.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text.as_bytes())
    )
}

pub(in crate::api) fn compaction_item(id: &str, text: &str) -> Value {
    json!({"id":id,"type":"compaction","encrypted_content":compaction_blob(text)})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenAiChatUsage {
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiPromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<OpenAiCompletionTokensDetails>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenAiPromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<i64>,
    #[serde(default)]
    cache_write_tokens: Option<i64>,
    #[serde(default)]
    audio_tokens: Option<i64>,
    #[serde(default)]
    image_tokens: Option<i64>,
    #[serde(default)]
    text_tokens: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenAiCompletionTokensDetails {
    #[serde(default)]
    accepted_prediction_tokens: Option<i64>,
    #[serde(default)]
    audio_tokens: Option<i64>,
    #[serde(default)]
    reasoning_tokens: Option<i64>,
    #[serde(default)]
    rejected_prediction_tokens: Option<i64>,
    #[serde(default)]
    text_tokens: Option<i64>,
}

fn normalize_openai_chat_usage(value: &Value) -> Result<Value, &'static str> {
    let usage = serde_json::from_value::<OpenAiChatUsage>(value.clone())
        .map_err(|_| "responses_chat_usage_schema")?;
    let cached = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
        .unwrap_or(0);
    let cache_write = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cache_write_tokens)
        .unwrap_or(0);
    let prompt_details_valid = usage.prompt_tokens_details.as_ref().is_none_or(|details| {
        [
            details.cached_tokens,
            details.cache_write_tokens,
            details.audio_tokens,
            details.image_tokens,
            details.text_tokens,
        ]
        .into_iter()
        .flatten()
        .all(|tokens| (0..=usage.prompt_tokens).contains(&tokens))
    });
    let completion_details_valid = usage
        .completion_tokens_details
        .as_ref()
        .is_none_or(|details| {
            [
                details.accepted_prediction_tokens,
                details.audio_tokens,
                details.reasoning_tokens,
                details.rejected_prediction_tokens,
                details.text_tokens,
            ]
            .into_iter()
            .flatten()
            .all(|tokens| (0..=usage.completion_tokens).contains(&tokens))
        });
    if usage.prompt_tokens < 0
        || usage.completion_tokens < 0
        || usage.total_tokens <= 0
        || usage.total_tokens
            != usage
                .prompt_tokens
                .checked_add(usage.completion_tokens)
                .unwrap_or(-1)
        || cached < 0
        || cache_write < 0
        || cached > usage.prompt_tokens
        || cached
            .checked_add(cache_write)
            .is_none_or(|cached_and_written| cached_and_written > usage.prompt_tokens)
        || !prompt_details_valid
        || !completion_details_valid
        || usage.prompt_tokens > crate::api::limits::MAX_REPORTED_TOKENS
        || usage.completion_tokens > crate::api::limits::MAX_REPORTED_TOKENS
    {
        return Err("responses_chat_usage_invalid");
    }
    Ok(value.clone())
}

fn usage(value: &Value, dialect: ResponsesViaChatDialect) -> Result<Value, &'static str> {
    let value = match dialect {
        ResponsesViaChatDialect::KimiV1 => super::usage::normalize(value)?,
        ResponsesViaChatDialect::OpenAiChatV1 => normalize_openai_chat_usage(value)?,
    };
    let input = value["prompt_tokens"]
        .as_u64()
        .ok_or("usage_input_invalid")?;
    let output = value["completion_tokens"]
        .as_u64()
        .ok_or("usage_output_invalid")?;
    let total = value["total_tokens"]
        .as_u64()
        .ok_or("responses_chat_usage_field_type")?;
    let cached = value
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = value
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if cached > input || reasoning > output {
        return Err("usage_detail_exceeds_total");
    }
    Ok(
        json!({"input_tokens":input,"output_tokens":output,"total_tokens":total,
        "input_tokens_details":{"cached_tokens":cached},"output_tokens_details":{"reasoning_tokens":reasoning}}),
    )
}

fn envelope(context: &Context, id: &str, created: i64, outputs: Vec<Value>, usage: Value) -> Value {
    json!({"id":id,"object":"response","created_at":created,"model":context.model,
        "status":"completed","error":null,"incomplete_details":null,"output":outputs,"usage":usage})
}

fn finish_failure(reason: Option<&str>) -> &'static str {
    match reason {
        None => "finish_reason_missing",
        Some("length") => "finish_reason_length",
        Some("content_filter") => "finish_reason_content_filter",
        Some(_) => "finish_reason_unsupported",
    }
}

fn incomplete_reason(reason: Option<&str>) -> Option<&'static str> {
    match reason {
        Some("length") => Some("max_output_tokens"),
        Some("content_filter") => Some("content_filter"),
        _ => None,
    }
}

fn terminal_envelope(mut value: Value, reason: Option<&str>) -> Value {
    if let Some(reason) = incomplete_reason(reason) {
        value["status"] = "incomplete".into();
        value["incomplete_details"] = json!({"reason":reason});
        if let Some(items) = value["output"].as_array_mut() {
            for item in items {
                if item.get("status").is_some() {
                    item["status"] = "incomplete".into();
                }
            }
        }
    }
    value
}

fn validate_tool_items(items: &[Value]) -> Result<(), &'static str> {
    let mut ids = BTreeSet::new();
    for item in items {
        if !matches!(
            item["type"].as_str(),
            Some("function_call" | "custom_tool_call")
        ) {
            continue;
        }
        let id = item["call_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("tool_call_id_missing")?;
        if !ids.insert(id) {
            return Err("tool_call_id_duplicate");
        }
        if item["name"].as_str().is_none_or(str::is_empty) {
            return Err("tool_name_missing");
        }
        if item["type"] == "function_call" {
            let arguments = item["arguments"].as_str().ok_or("tool_arguments_invalid")?;
            let value = crate::api::sse::parse_unique_json(arguments.as_bytes())
                .map_err(|_| "tool_arguments_invalid")?;
            if !value.is_object() {
                return Err("tool_arguments_invalid");
            }
        }
    }
    Ok(())
}

/// Compaction responses carry exactly one opaque checkpoint item built from
/// the upstream message text. Anything else (tool calls, truncated output,
/// missing text) would corrupt the client-side history, so it fails the
/// whole response instead of producing a partial checkpoint.
fn buffered_compaction(
    context: &Context,
    id: &str,
    value: &Value,
    choice: &Value,
) -> Result<Value, &'static str> {
    let message = &choice["message"];
    if message["tool_calls"]
        .as_array()
        .is_some_and(|calls| !calls.is_empty())
    {
        return Err("compaction_tool_call");
    }
    if choice["finish_reason"] != "stop" {
        return Err("compaction_finish_invalid");
    }
    let text = message["content"]
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or("compaction_text_missing")?;
    let item = compaction_item(&format!("cmp_{id}"), text);
    Ok(terminal_envelope(
        envelope(
            context,
            id,
            value["created"].as_i64().unwrap_or(0),
            vec![item],
            usage(&value["usage"], context.usage_dialect)?,
        ),
        choice["finish_reason"].as_str(),
    ))
}

pub(in crate::api) fn buffered(context: &Context, value: &Value) -> Result<Value, &'static str> {
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err("provider_error");
    }
    let choices = value["choices"].as_array().ok_or("choices_missing")?;
    if choices.len() != 1 {
        return Err("choice_count_invalid");
    }
    let choice = &choices[0];
    if !matches!(
        choice["finish_reason"].as_str(),
        Some("stop" | "tool_calls" | "function_call" | "length" | "content_filter")
    ) {
        return Err(finish_failure(choice["finish_reason"].as_str()));
    }
    let message = &choice["message"];
    let id = format!("resp_{}", Uuid::now_v7().simple());
    if context.is_compaction() {
        return buffered_compaction(context, &id, value, choice);
    }
    let mut outputs = Vec::new();
    if let Some(reasoning) = message["reasoning_content"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        outputs.push(json!({"id":format!("rs_{id}"),"type":"reasoning",
            "summary":[{"type":"summary_text","text":reasoning}]}));
    }
    if let Some(text) = message["content"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        outputs.push(json!({"id":format!("msg_{id}"),"type":"message","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":text,"annotations":[]}]}));
    }
    if let Some(calls) = message["tool_calls"].as_array() {
        if calls.len() > MAX_ITEMS {
            return Err("item_limit");
        }
        for (index, call) in calls.iter().enumerate() {
            if incomplete_reason(choice["finish_reason"].as_str()).is_none() {
                context.validate_custom_call(call)?;
            }
            outputs.push(context.tool_item(call, &format!("fc_{id}_{index}")));
        }
    }
    if incomplete_reason(choice["finish_reason"].as_str()).is_none() {
        validate_tool_items(&outputs)?;
        if matches!(
            choice["finish_reason"].as_str(),
            Some("tool_calls" | "function_call")
        ) && message["tool_calls"].as_array().is_none_or(Vec::is_empty)
        {
            return Err("tool_calls_missing");
        }
    }
    Ok(terminal_envelope(
        envelope(
            context,
            &id,
            value["created"].as_i64().unwrap_or(0),
            outputs,
            usage(&value["usage"], context.usage_dialect)?,
        ),
        choice["finish_reason"].as_str(),
    ))
}

/// Incremental response translation. Memory is bounded independently of wire
/// chunk size and terminal success is emitted only after validated Chat usage.
pub(in crate::api) struct Stream {
    context: Context,
    id: String,
    created: i64,
    sequence: u64,
    started: bool,
    done: bool,
    bytes: usize,
    items: Vec<Value>,
    message_index: Option<usize>,
    reasoning_index: Option<usize>,
    calls: BTreeMap<u64, (usize, Value)>,
    usage: Option<Value>,
    finish: Option<String>,
    /// Assembled checkpoint text in compaction mode. Intermediate item and
    /// delta events are suppressed; the single compaction item is emitted
    /// only at finish, once the checkpoint text is complete.
    compaction_text: String,
}

impl Stream {
    pub(in crate::api) fn new(context: Context) -> Self {
        Self {
            context,
            id: format!("resp_{}", Uuid::now_v7().simple()),
            created: 0,
            sequence: 0,
            started: false,
            done: false,
            bytes: 0,
            items: Vec::new(),
            message_index: None,
            reasoning_index: None,
            calls: BTreeMap::new(),
            usage: None,
            finish: None,
            compaction_text: String::new(),
        }
    }

    fn event(&mut self, name: &str, mut value: Value) -> Result<Vec<u8>, &'static str> {
        value["type"] = name.into();
        value["sequence_number"] = self.sequence.into();
        self.sequence += 1;
        let json = serde_json::to_string(&value).map_err(|_| "event_serialization")?;
        Ok(format!("event: {name}\ndata: {json}\n\n").into_bytes())
    }

    fn added(&mut self, item: Value, events: &mut Vec<Vec<u8>>) -> Result<usize, &'static str> {
        if self.items.len() >= MAX_ITEMS {
            return Err("item_limit");
        }
        let index = self.items.len();
        self.items.push(item.clone());
        events.push(self.event(
            "response.output_item.added",
            json!({"output_index":index,"item":item}),
        )?);
        Ok(index)
    }

    pub(in crate::api) fn observe(&mut self, chunk: &Value) -> Result<Vec<Vec<u8>>, &'static str> {
        if self.done {
            return Err("event_after_completed");
        }
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            self.created = chunk["created"].as_i64().unwrap_or(0);
            let mut response = envelope(
                &self.context,
                &self.id,
                self.created,
                Vec::new(),
                Value::Null,
            );
            response["status"] = "in_progress".into();
            events.push(self.event("response.created", json!({"response":response.clone()}))?);
            events.push(self.event("response.in_progress", json!({"response":response}))?);
        }
        if !chunk["usage"].is_null() {
            self.usage = Some(usage(&chunk["usage"], self.context.usage_dialect)?);
        }
        let choices = chunk["choices"].as_array().ok_or("choices_missing")?;
        if choices.len() > 1 {
            return Err("choice_count_invalid");
        }
        for choice in choices {
            if choice["index"].as_u64() != Some(0) {
                return Err("choice_index_invalid");
            }
            if let Some(finish) = choice["finish_reason"].as_str() {
                self.finish = Some(finish.into());
            }
            let delta = &choice["delta"];
            for (field, reasoning) in [("reasoning_content", true), ("content", false)] {
                let Some(text) = delta[field].as_str().filter(|text| !text.is_empty()) else {
                    continue;
                };
                self.bytes = self
                    .bytes
                    .checked_add(text.len())
                    .ok_or("accumulation_limit")?;
                if self.bytes > MAX_ACCUMULATED_BYTES {
                    return Err("accumulation_limit");
                }
                if self.context.is_compaction() {
                    if !reasoning {
                        self.compaction_text.push_str(text);
                    }
                    // Intermediate item and delta events are suppressed in
                    // compaction mode; the single checkpoint item is emitted
                    // at finish, once the text is complete.
                    continue;
                }
                let existing = if reasoning {
                    self.reasoning_index
                } else {
                    self.message_index
                };
                let index = if let Some(index) = existing {
                    index
                } else {
                    let item = if reasoning {
                        json!({"id":format!("rs_{}",self.id),"type":"reasoning","summary":[]})
                    } else {
                        json!({"id":format!("msg_{}",self.id),"type":"message","role":"assistant","status":"in_progress","content":[]})
                    };
                    let index = self.added(item, &mut events)?;
                    let part = if reasoning {
                        json!({"type":"summary_text","text":""})
                    } else {
                        json!({"type":"output_text","text":"","annotations":[]})
                    };
                    let name = if reasoning {
                        "response.reasoning_summary_part.added"
                    } else {
                        "response.content_part.added"
                    };
                    events.push(self.event(name, json!({"item_id":self.items[index]["id"],
                        "output_index":index,"content_index":0,"summary_index":0,"part":part.clone()}))?);
                    self.items[index][if reasoning { "summary" } else { "content" }] =
                        json!([part]);
                    if reasoning {
                        self.reasoning_index = Some(index);
                    } else {
                        self.message_index = Some(index);
                    }
                    index
                };
                let content_field = if reasoning { "summary" } else { "content" };
                let current = self.items[index][content_field][0]["text"]
                    .as_str()
                    .ok_or("assembled_content_invalid")?;
                self.items[index][content_field][0]["text"] = format!("{current}{text}").into();
                let name = if reasoning {
                    "response.reasoning_summary_text.delta"
                } else {
                    "response.output_text.delta"
                };
                events.push(self.event(
                    name,
                    json!({"item_id":self.items[index]["id"],"output_index":index,
                    "content_index":0,"summary_index":0,"delta":text}),
                )?);
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                if self.context.is_compaction() {
                    // Tool calls can never be part of a checkpoint; fail the
                    // stream instead of assembling a partial compaction.
                    return Err("compaction_tool_call");
                }
                for call in calls {
                    let call_index = call["index"].as_u64().ok_or("tool_index_invalid")?;
                    if !self.calls.contains_key(&call_index) {
                        let item_index = self.items.len();
                        let mut call = call.clone();
                        if call.get("function").is_none() {
                            call["function"] = json!({});
                        }
                        let mut item = self
                            .context
                            .tool_item(&call, &format!("fc_{}_{call_index}", self.id));
                        item["status"] = "in_progress".into();
                        self.added(item, &mut events)?;
                        self.calls.insert(
                            call_index,
                            (
                                item_index,
                                json!({"id":"","function":{"name":"","arguments":""}}),
                            ),
                        );
                    }
                    let (index, assembled) = self
                        .calls
                        .get_mut(&call_index)
                        .ok_or("assembled_tool_missing")?;
                    if let Some(id) = call["id"].as_str() {
                        assembled["id"] = id.into();
                    }
                    for field in ["name", "arguments"] {
                        if let Some(fragment) = call["function"][field].as_str() {
                            self.bytes = self
                                .bytes
                                .checked_add(fragment.len())
                                .ok_or("accumulation_limit")?;
                            if self.bytes > MAX_ACCUMULATED_BYTES {
                                return Err("accumulation_limit");
                            }
                            let old = assembled["function"][field].as_str().unwrap_or("");
                            assembled["function"][field] = format!("{old}{fragment}").into();
                        }
                    }
                    let index = *index;
                    let assembled = assembled.clone();
                    let item_id = format!("fc_{}_{call_index}", self.id);
                    self.items[index] = self.context.tool_item(&assembled, &item_id);
                    // Freeform arguments are JSON-wrapped by Chat. Emit the
                    // unwrapped input only when complete, never partial JSON.
                    if self.items[index]["type"] == "function_call"
                        && let Some(fragment) = call["function"]["arguments"].as_str()
                    {
                        events.push(self.event(
                            "response.function_call_arguments.delta",
                            json!({"item_id":item_id,"output_index":index,"delta":fragment}),
                        )?);
                    }
                }
            }
        }
        Ok(events)
    }

    pub(in crate::api) fn finish(&mut self) -> Result<Vec<Vec<u8>>, &'static str> {
        if self.done {
            return Err("event_after_completed");
        }
        if !self.started {
            return Err("empty_stream");
        }
        if self.context.is_compaction() {
            return self.finish_compaction();
        }
        if !matches!(
            self.finish.as_deref(),
            Some("stop" | "tool_calls" | "function_call" | "length" | "content_filter")
        ) {
            return Err(finish_failure(self.finish.as_deref()));
        }
        let incomplete = incomplete_reason(self.finish.as_deref()).is_some();
        if !incomplete {
            validate_tool_items(&self.items)?;
            if matches!(self.finish.as_deref(), Some("tool_calls" | "function_call"))
                && self.calls.is_empty()
            {
                return Err("tool_calls_missing");
            }
            for (_, call) in self.calls.values() {
                self.context.validate_custom_call(call)?;
            }
        }
        let usage = self.usage.take().ok_or("usage_missing")?;
        self.done = true;
        let mut events = Vec::new();
        for index in 0..self.items.len() {
            let mut item = self.items[index].clone();
            if item.get("status").is_some() {
                item["status"] = if incomplete {
                    "incomplete"
                } else {
                    "completed"
                }
                .into();
            }
            match item["type"].as_str() {
                Some("message" | "reasoning") => {
                    let reasoning = item["type"] == "reasoning";
                    let part = item[if reasoning {"summary"} else {"content"}][0].clone();
                    events.push(self.event(if reasoning {"response.reasoning_summary_text.done"} else {"response.output_text.done"},
                        json!({"item_id":item["id"],"output_index":index,"content_index":0,"summary_index":0,"text":part["text"]}))?);
                    events.push(self.event(if reasoning {"response.reasoning_summary_part.done"} else {"response.content_part.done"},
                        json!({"item_id":item["id"],"output_index":index,"content_index":0,"summary_index":0,"part":part}))?);
                }
                Some("function_call") => events.push(self.event("response.function_call_arguments.done",
                    json!({"item_id":item["id"],"output_index":index,"arguments":item["arguments"]}))?),
                Some("custom_tool_call") => {
                    events.push(self.event("response.custom_tool_call_input.delta",
                        json!({"item_id":item["id"],"output_index":index,"delta":item["input"]}))?);
                    events.push(self.event("response.custom_tool_call_input.done",
                        json!({"item_id":item["id"],"output_index":index,"input":item["input"]}))?);
                }
                _ => return Err("assembled_item_invalid"),
            }
            self.items[index] = item.clone();
            events.push(self.event(
                "response.output_item.done",
                json!({"output_index":index,"item":item}),
            )?);
        }
        let response = terminal_envelope(
            envelope(
                &self.context,
                &self.id,
                self.created,
                self.items.clone(),
                usage,
            ),
            self.finish.as_deref(),
        );
        events.push(self.event(
            if incomplete {
                "response.incomplete"
            } else {
                "response.completed"
            },
            json!({"response":response}),
        )?);
        Ok(events)
    }

    fn finish_compaction(&mut self) -> Result<Vec<Vec<u8>>, &'static str> {
        if self.finish.as_deref() != Some("stop") {
            return Err("compaction_finish_invalid");
        }
        if !self.calls.is_empty() {
            return Err("compaction_tool_call");
        }
        let usage = self.usage.take().ok_or("usage_missing")?;
        let text = std::mem::take(&mut self.compaction_text);
        if text.is_empty() {
            return Err("compaction_text_missing");
        }
        self.done = true;
        let item = compaction_item(&format!("cmp_{}", self.id), &text);
        let response = envelope(
            &self.context,
            &self.id,
            self.created,
            vec![item.clone()],
            usage,
        );
        let mut events = Vec::new();
        events.push(self.event(
            "response.output_item.added",
            json!({"output_index":0,"item":item}),
        )?);
        events.push(self.event(
            "response.output_item.done",
            json!({"output_index":0,"item":item}),
        )?);
        events.push(self.event("response.completed", json!({"response":response}))?);
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compaction_context() -> Context {
        Context::for_kimi(&json!({"model":"kimi-k3",
        "input":[
            {"role":"user","content":"earlier work"},
            {"type":"compaction_trigger"}
        ]}))
    }

    fn compaction_fixture(text: &str) -> Value {
        json!({"created":1,
            "choices":[{"finish_reason":"stop","message":{"content":text}}],
            "usage":{"prompt_tokens":9,"completion_tokens":4,"total_tokens":13}})
    }

    fn decode_checkpoint(blob: &str) -> String {
        use base64::Engine as _;
        let encoded = blob
            .strip_prefix("mtc-compact-v1.")
            .expect("versioned blob");
        String::from_utf8(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded.as_bytes())
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn context_detects_compaction_trigger_from_source_request() {
        assert!(compaction_context().is_compaction());
        let plain = Context::for_kimi(&json!({"model":"kimi-k3",
            "input":[{"role":"user","content":"hi"}]}));
        assert!(!plain.is_compaction());
    }

    #[test]
    fn buffered_compaction_wraps_text_as_opaque_checkpoint() {
        let context = compaction_context();
        let response = buffered(&context, &compaction_fixture("handoff summary")).unwrap();
        assert_eq!(response["status"], "completed");
        let output = &response["output"][0];
        assert_eq!(output["type"], "compaction");
        assert!(output["id"].as_str().unwrap().starts_with("cmp_"));
        let blob = output["encrypted_content"].as_str().unwrap();
        assert_eq!(decode_checkpoint(blob), "handoff summary");
        assert_eq!(response["usage"]["input_tokens"], 9);
    }

    #[test]
    fn buffered_compaction_rejects_tool_calls_and_truncation() {
        let context = compaction_context();
        let with_calls = json!({"choices":[{"finish_reason":"tool_calls",
            "message":{"content":"","tool_calls":[{"id":"c","type":"function",
                "function":{"name":"f","arguments":"{}"}}]}}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
        assert_eq!(buffered(&context, &with_calls), Err("compaction_tool_call"));
        let truncated = json!({"choices":[{"finish_reason":"length",
            "message":{"content":"partial"}}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
        assert_eq!(
            buffered(&context, &truncated),
            Err("compaction_finish_invalid")
        );
        let empty = json!({"choices":[{"finish_reason":"stop","message":{"content":""}}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
        assert_eq!(buffered(&context, &empty), Err("compaction_text_missing"));
    }

    #[test]
    fn stream_compaction_emits_single_opaque_checkpoint() {
        let mut stream = Stream::new(compaction_context());
        let first = stream
            .observe(&json!({"created":1,"choices":[{"index":0,
                "delta":{"content":"handoff "},"finish_reason":null}],"usage":Value::Null}))
            .unwrap();
        // Only the response lifecycle events; no item or delta events leak.
        let wire = String::from_utf8(first.concat()).unwrap();
        assert!(wire.contains("response.created"));
        assert!(wire.contains("response.in_progress"));
        assert!(!wire.contains("output_item.added"));
        assert!(!wire.contains("output_text.delta"));
        let second = stream
            .observe(&json!({"created":1,"choices":[{"index":0,
                "delta":{"reasoning_content":"private","content":"summary"},
                "finish_reason":null}],"usage":Value::Null}))
            .unwrap();
        assert!(second.is_empty());
        stream
            .observe(&json!({"created":1,"choices":[{"index":0,"delta":{},
                "finish_reason":"stop"}],"usage":Value::Null}))
            .unwrap();
        let usage = stream
            .observe(&json!({"created":1,"choices":[],
                "usage":{"prompt_tokens":9,"completion_tokens":4,"total_tokens":13}}))
            .unwrap();
        assert!(usage.is_empty());
        let events = stream.finish().unwrap();
        let wire = String::from_utf8(events.concat()).unwrap();
        let names: Vec<&str> = wire
            .split("event: ")
            .skip(1)
            .map(|frame| frame.lines().next().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "response.output_item.added",
                "response.output_item.done",
                "response.completed"
            ]
        );
        // Twice on the wire (added + done) plus once inside the completed
        // envelope's output array.
        assert_eq!(wire.matches("\"type\":\"compaction\"").count(), 3);
        let done = wire
            .split("event: response.output_item.done\ndata: ")
            .nth(1)
            .unwrap()
            .split("\n\n")
            .next()
            .unwrap();
        let item: Value = serde_json::from_str(done).unwrap();
        let blob = item["item"]["encrypted_content"].as_str().unwrap();
        assert_eq!(decode_checkpoint(blob), "handoff summary");
        assert!(wire.contains("\"status\":\"completed\""));
    }

    #[test]
    fn stream_compaction_rejects_tool_deltas() {
        let mut stream = Stream::new(compaction_context());
        stream
            .observe(&json!({"created":1,"choices":[{"index":0,
                "delta":{"content":"x"},"finish_reason":null}],"usage":Value::Null}))
            .unwrap();
        let error = stream.observe(&json!({"created":1,"choices":[{"index":0,
            "delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"f"}}]},
            "finish_reason":null}],"usage":Value::Null}));
        assert_eq!(error, Err("compaction_tool_call"));
    }

    #[test]
    fn buffered_documented_cache_alias_is_preserved_and_conflicts_fail() {
        let context = Context::for_kimi(&json!({"model":"kimi-k3"}));
        let mut value = json!({"choices":[{"finish_reason":"stop", "message":{"content":"Hello"}}],
            "usage":{"prompt_tokens":19,"completion_tokens":13,"total_tokens":32,"cached_tokens":12}});
        let response = buffered(&context, &value).unwrap();
        assert_eq!(response["usage"]["input_tokens"], 19);
        assert_eq!(
            response["usage"]["input_tokens_details"]["cached_tokens"],
            12
        );
        value["usage"]["prompt_tokens_details"] = json!({"cached_tokens":11});
        assert_eq!(
            buffered(&context, &value),
            Err("kimi_cached_tokens_conflict")
        );
    }

    #[test]
    fn generic_usage_rejects_kimi_top_level_cache_alias() {
        let context = Context::new(&json!({"model":"generic"}));
        let value = json!({
            "choices":[{"finish_reason":"stop", "message":{"content":"Hello"}}],
            "usage":{"prompt_tokens":19,"completion_tokens":13,"total_tokens":32,
                "cached_tokens":12}
        });
        assert_eq!(
            buffered(&context, &value),
            Err("responses_chat_usage_schema")
        );
    }

    #[test]
    fn buffered_rejects_every_malformed_accounting_shape_before_completed() {
        let context = Context::for_kimi(&json!({"model":"kimi-k3"}));
        for usage in super::super::usage::invalid_examples() {
            let value = json!({"choices":[{"finish_reason":"stop", "message":{"content":"Hello"}}],
                "usage": usage});
            assert!(buffered(&context, &value).is_err());
        }
    }

    #[test]
    fn buffered_custom_output_and_cached_reasoning_usage_are_preserved() {
        let context = Context::for_kimi(&json!({"model":"kimi-k3","tools":[
            {"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}]}));
        let response = buffered(&context, &json!({"created":12,"choices":[{
            "finish_reason":"tool_calls","message":{"reasoning_content":"thinking",
            "tool_calls":[{"id":"a","function":{"name":"editor__patch","arguments":"{\"input\":\"diff\"}"}}]}}],
            "usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14,
                "prompt_tokens_details":{"cached_tokens":3},"completion_tokens_details":{"reasoning_tokens":2}}})).unwrap();
        assert_eq!(response["output"][1]["type"], "custom_tool_call");
        assert_eq!(response["output"][1]["namespace"], "editor");
        assert_eq!(response["output"][1]["input"], "diff");
        assert_eq!(
            response["usage"]["input_tokens_details"]["cached_tokens"],
            3
        );
        assert_eq!(
            response["usage"]["output_tokens_details"]["reasoning_tokens"],
            2
        );
    }

    #[test]
    fn buffered_marks_only_plaintext_collaboration_message_calls() {
        let context = Context::new(&json!({
            "model": "third-party-model",
            "tools": [
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","parameters":{"type":"object"}},
                    {"type":"function","name":"send_message","parameters":{"type":"object"}},
                    {"type":"function","name":"followup_task","parameters":{"type":"object"}}
                ]},
                {"type":"namespace","name":"other","tools":[
                    {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
                ]},
                {"type":"function","name":"lookup","parameters":{"type":"object"}}
            ]
        }));
        let response = buffered(&context, &json!({
            "created": 12,
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {"tool_calls": [
                    {"id":"spawn","function":{"name":"collaboration__spawn_agent","arguments":"{}"}},
                    {"id":"send","function":{"name":"collaboration__send_message","arguments":"{}"}},
                    {"id":"follow","function":{"name":"collaboration__followup_task","arguments":"{}"}},
                    {"id":"other","function":{"name":"other__spawn_agent","arguments":"{}"}},
                    {"id":"lookup","function":{"name":"lookup","arguments":"{}"}}
                ]}
            }],
            "usage": {"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}
        }))
        .unwrap();

        let output = response["output"].as_array().unwrap();
        assert_eq!(output.len(), 5);
        for item in &output[..3] {
            assert_eq!(item["namespace"], "collaboration");
            assert_eq!(item["encrypted_function_args"], json!([]));
        }
        for item in &output[3..] {
            assert!(item.get("encrypted_function_args").is_none());
        }
    }

    #[test]
    fn stream_delivers_deltas_but_cannot_complete_without_usage() {
        let mut stream = Stream::new(Context::for_kimi(&json!({"model":"kimi-k3"})));
        let events = stream
            .observe(
                &json!({"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}),
            )
            .unwrap();
        let text = events.concat();
        assert!(
            String::from_utf8(text)
                .unwrap()
                .contains("response.output_text.delta")
        );
        stream
            .observe(&json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}))
            .unwrap();
        assert!(stream.finish().is_err());
        stream.observe(&json!({"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}})).unwrap();
        let events = stream.finish().unwrap();
        let last = String::from_utf8(events.last().unwrap().clone()).unwrap();
        assert!(last.contains("response.completed"));
        assert!(last.contains("\"text\":\"hello\""));
        assert!(stream.finish().is_err());
    }

    #[test]
    fn streamed_collaboration_calls_complete_and_reverse_map_namespaces() {
        let request = json!({
            "model": "kimi-k3-256k",
            "tools": [{"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"spawn_agent","parameters":{"type":"object"}},
                {"type":"function","name":"followup_task","parameters":{"type":"object"}}
            ]}]
        });
        let mut stream = Stream::new(Context::for_kimi(&request));
        stream
            .observe(&json!({
                "created": 7,
                "choices": [{"index":0,"delta":{"tool_calls":[
                    {"index":0,"id":"spawn-call","function":{"name":"collaboration__spawn_agent","arguments":r#"{"message":"spawn"#}}]},
                "finish_reason":null}]
            }))
            .unwrap();
        stream
            .observe(&json!({
                "choices": [{"index":0,"delta":{"tool_calls":[
                    {"index":0,"function":{"arguments":r#" agent"}"#}},
                    {"index":1,"id":"followup-call","function":{"name":"collaboration__followup_task","arguments":r#"{"message":"follow"}"#}}
                ]},"finish_reason":null}]
            }))
            .unwrap();
        stream
            .observe(&json!({
                "choices": [{"index":0,"delta":{},"finish_reason":"tool_calls"}]
            }))
            .unwrap();
        stream
            .observe(&json!({
                "choices": [],
                "usage": {"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}
            }))
            .unwrap();

        let events = stream.finish().unwrap();
        let wire = String::from_utf8(events.concat()).unwrap();
        assert!(wire.contains("response.function_call_arguments.done"));
        assert!(wire.contains("response.completed"));
        assert!(wire.contains("\"name\":\"spawn_agent\""));
        assert!(wire.contains("\"name\":\"followup_task\""));
        assert!(wire.matches("\"namespace\":\"collaboration\"").count() >= 2);
        assert!(wire.matches("\"encrypted_function_args\":[]").count() >= 2);
        assert!(wire.contains("spawn agent"));
        assert!(wire.contains("follow"));
    }
}
