use super::responses_request::{ToolIdentity, tools};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

const MAX_ACCUMULATED_BYTES: usize = 8 * 1024 * 1024;
const MAX_ITEMS: usize = 512;

#[derive(Clone)]
pub(in crate::api) struct Context {
    model: String,
    tools: BTreeMap<String, ToolIdentity>,
}

impl Context {
    pub(in crate::api) fn new(request: &Value) -> Self {
        Self {
            model: request["model"].as_str().unwrap_or("").into(),
            tools: tools(request)
                .into_iter()
                .map(|(name, (identity, _))| (name, identity))
                .collect(),
        }
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
        }
        item
    }
}

fn usage(value: &Value) -> Result<Value, ()> {
    let input = value["prompt_tokens"].as_u64().ok_or(())?;
    let output = value["completion_tokens"].as_u64().ok_or(())?;
    let total = input.checked_add(output).ok_or(())?;
    if value["total_tokens"]
        .as_u64()
        .is_some_and(|claimed| claimed != total)
    {
        return Err(());
    }
    let cached = value
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = value
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if cached > input || reasoning > output {
        return Err(());
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

pub(in crate::api) fn buffered(context: &Context, value: &Value) -> Result<Value, ()> {
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err(());
    }
    let choices = value["choices"].as_array().ok_or(())?;
    if choices.len() != 1 {
        return Err(());
    }
    let choice = &choices[0];
    if !matches!(
        choice["finish_reason"].as_str(),
        Some("stop" | "tool_calls" | "function_call")
    ) {
        return Err(());
    }
    let message = &choice["message"];
    let id = format!("resp_{}", Uuid::now_v7().simple());
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
            return Err(());
        }
        for (index, call) in calls.iter().enumerate() {
            outputs.push(context.tool_item(call, &format!("fc_{id}_{index}")));
        }
    }
    Ok(envelope(
        context,
        &id,
        value["created"].as_i64().unwrap_or(0),
        outputs,
        usage(&value["usage"])?,
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
        }
    }

    fn event(&mut self, name: &str, mut value: Value) -> Result<Vec<u8>, ()> {
        value["type"] = name.into();
        value["sequence_number"] = self.sequence.into();
        self.sequence += 1;
        let json = serde_json::to_string(&value).map_err(|_| ())?;
        Ok(format!("event: {name}\ndata: {json}\n\n").into_bytes())
    }

    fn added(&mut self, item: Value, events: &mut Vec<Vec<u8>>) -> Result<usize, ()> {
        if self.items.len() >= MAX_ITEMS {
            return Err(());
        }
        let index = self.items.len();
        self.items.push(item.clone());
        events.push(self.event(
            "response.output_item.added",
            json!({"output_index":index,"item":item}),
        )?);
        Ok(index)
    }

    pub(in crate::api) fn observe(&mut self, chunk: &Value) -> Result<Vec<Vec<u8>>, ()> {
        if self.done {
            return Err(());
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
            self.usage = Some(usage(&chunk["usage"])?);
        }
        let choices = chunk["choices"].as_array().ok_or(())?;
        if choices.len() > 1 {
            return Err(());
        }
        for choice in choices {
            if choice["index"].as_u64() != Some(0) {
                return Err(());
            }
            if let Some(finish) = choice["finish_reason"].as_str() {
                self.finish = Some(finish.into());
            }
            let delta = &choice["delta"];
            for (field, reasoning) in [("reasoning_content", true), ("content", false)] {
                let Some(text) = delta[field].as_str().filter(|text| !text.is_empty()) else {
                    continue;
                };
                self.bytes = self.bytes.checked_add(text.len()).ok_or(())?;
                if self.bytes > MAX_ACCUMULATED_BYTES {
                    return Err(());
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
                    .ok_or(())?;
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
                for call in calls {
                    let call_index = call["index"].as_u64().ok_or(())?;
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
                    let (index, assembled) = self.calls.get_mut(&call_index).ok_or(())?;
                    if let Some(id) = call["id"].as_str() {
                        assembled["id"] = id.into();
                    }
                    for field in ["name", "arguments"] {
                        if let Some(fragment) = call["function"][field].as_str() {
                            self.bytes = self.bytes.checked_add(fragment.len()).ok_or(())?;
                            if self.bytes > MAX_ACCUMULATED_BYTES {
                                return Err(());
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

    pub(in crate::api) fn finish(&mut self) -> Result<Vec<Vec<u8>>, ()> {
        if self.done
            || !self.started
            || !matches!(
                self.finish.as_deref(),
                Some("stop" | "tool_calls" | "function_call")
            )
        {
            return Err(());
        }
        let usage = self.usage.take().ok_or(())?;
        self.done = true;
        let mut events = Vec::new();
        for index in 0..self.items.len() {
            let mut item = self.items[index].clone();
            if item["type"] == "message" {
                item["status"] = "completed".into();
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
                _ => return Err(()),
            }
            self.items[index] = item.clone();
            events.push(self.event(
                "response.output_item.done",
                json!({"output_index":index,"item":item}),
            )?);
        }
        let response = envelope(
            &self.context,
            &self.id,
            self.created,
            self.items.clone(),
            usage,
        );
        events.push(self.event("response.completed", json!({"response":response}))?);
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffered_custom_output_and_cached_reasoning_usage_are_preserved() {
        let context = Context::new(&json!({"model":"kimi-k3","tools":[
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
    fn stream_delivers_deltas_but_cannot_complete_without_usage() {
        let mut stream = Stream::new(Context::new(&json!({"model":"kimi-k3"})));
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
}
