use super::*;

fn prepare(body: &mut Value, policy: &str) -> Result<PreparedCodexRequest, AppError> {
    prepare_request_with_id(
        body,
        "gpt-6-sol",
        &json!({
            "base_url": BASE_URL, "network_scope": "public",
            "reservation_token_bounds": {"gpt-6-sol": 64},
            "transport_policy": {"chat_controls": policy}
        }),
        Uuid::nil(),
        Protocol::OpenAiChat,
    )
}

fn request() -> Value {
    json!({"model": "gpt-6-sol", "messages": [{"role": "user", "content": "synthetic task"}]})
}

fn call(index: usize, arguments: &str) -> Value {
    json!({"type": "function_call", "id": format!("fc_{index}"),
        "call_id": format!("call_{index}"), "name": "lookup", "arguments": arguments})
}

fn terminal(output: Value) -> Value {
    json!({"type": "response.completed", "response": {"id": "resp_synthetic", "output": output,
        "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}}})
}

fn frame(value: &Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

fn chunks(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

#[test]
fn workbuddy_chat_request_preserves_agent_history_tools_and_controls() {
    let parameters = json!({"type": "object", "properties": {"key": {"type": "string"}}});
    let mut body = json!({
        "model": "gpt-6-sol", "stream": true,
        "stream_options": {"include_usage": true, "include_obfuscation": false},
        "tools": [{"type": "function", "function": {"name": "lookup", "description": "Synthetic lookup", "parameters": parameters}}],
        "tool_choice": {"type": "function", "function": {"name": "lookup"}},
        "parallel_tool_calls": false, "reasoning_effort": "high", "verbosity": "low",
        "metadata": {"fixture": "workbuddy"}, "client_extension": {"preserve": true},
        "messages": [
            {"role": "system", "content": "Use tools."},
            {"role": "user", "content": "Look up A"},
            {"role": "assistant", "content": "Checking", "tool_calls": [
                {"id": "call_history", "type": "function", "function": {"name": "lookup", "arguments": "{\"key\":\"A\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_history", "content": [{"type": "text", "text": "{\"value\":7}"}]},
            {"role": "developer", "content": "Continue carefully."},
            {"role": "user", "content": [{"type": "text", "text": "Inspect"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,synthetic", "detail": "high"}}]}
        ]
    });
    let plan = prepare(&mut body, "strict").unwrap();
    assert!(plan.downstream_stream);
    assert_eq!(plan.output_token_ceiling, 64);
    assert_eq!(
        body["tools"][0],
        json!({"type": "function", "name": "lookup", "description": "Synthetic lookup", "parameters": parameters, "strict": false})
    );
    assert_eq!(
        body["tool_choice"],
        json!({"type": "function", "name": "lookup"})
    );
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["reasoning"], json!({"effort": "high"}));
    assert_eq!(body["text"], json!({"verbosity": "low"}));
    assert_eq!(body["client_extension"], json!({"preserve": true}));
    assert!(body.get("metadata").is_none());
    assert_eq!(
        body["input"][2],
        json!({"type": "function_call", "call_id": "call_history", "name": "lookup", "arguments": "{\"key\":\"A\"}"})
    );
    assert_eq!(
        body["input"][3],
        json!({"type": "function_call_output", "call_id": "call_history", "output": "{\"value\":7}"})
    );
    assert_eq!(body["input"][4]["role"], "developer");
    assert_eq!(
        body["input"][5]["content"][1],
        json!({"type": "input_image", "image_url": "data:image/png;base64,synthetic", "detail": "high"})
    );
}

#[test]
fn native_metadata_is_removed_without_changing_chat_or_responses_semantics() {
    for protocol in [Protocol::OpenAiChat, Protocol::OpenAiResponses] {
        let mut body = match protocol {
            Protocol::OpenAiChat => json!({
                "model": "gpt-6-sol", "stream": true,
                "messages": [{"role": "system", "content": "Use the supplied tools."}, {"role": "user", "content": "Look up A."}],
                "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object", "properties": {}}}}],
                "tool_choice": "none", "parallel_tool_calls": false,
                "reasoning_effort": "high", "verbosity": "low"
            }),
            Protocol::OpenAiResponses => json!({
                "model": "gpt-6-sol", "stream": true, "input": "Look up A.",
                "instructions": "Use the supplied tools.",
                "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object", "properties": {}}}],
                "tool_choice": "none", "reasoning": {"effort": "high"}, "text": {"verbosity": "low"}
            }),
            _ => unreachable!(),
        };
        let mut without_metadata = body.clone();
        body["metadata"] = json!({"fixture": "workbuddy", "synthetic_tag": "not-an-instruction"});
        let config = json!({"base_url": BASE_URL, "network_scope": "public", "reservation_token_bounds": {"gpt-6-sol": 64}});
        for prepared in [&mut body, &mut without_metadata] {
            prepare_request_with_id(prepared, "gpt-6-sol", &config, Uuid::nil(), protocol).unwrap();
        }
        assert!(body.get("metadata").is_none());
        assert_eq!(body, without_metadata);
        assert_eq!(body["instructions"], "Use the supplied tools.");
        assert_eq!(body["reasoning"], json!({"effort": "high"}));
        assert_eq!(body["text"], json!({"verbosity": "low"}));
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["tool_choice"], "none");
    }
}

#[test]
fn workbuddy_chat_optional_fields_and_tool_modes_are_compatible() {
    for choice in [json!("auto"), json!("none"), json!("required"), Value::Null] {
        for parallel in [json!(true), json!(false), Value::Null] {
            let mut body = request();
            body["tools"] =
                json!([{"type": "function", "function": {"name": "lookup", "strict": true}}]);
            body["tool_choice"] = choice.clone();
            body["parallel_tool_calls"] = parallel.clone();
            for field in [
                "n",
                "stream_options",
                "response_format",
                "reasoning_effort",
                "verbosity",
                "temperature",
                "seed",
                "stop",
            ] {
                body[field] = Value::Null;
            }
            body["logprobs"] = json!(false);
            body["top_logprobs"] = json!(0);
            body["logit_bias"] = json!({});
            body["modalities"] = json!(["text"]);
            prepare(&mut body, "strict").unwrap();
            assert_eq!(body["tool_choice"], choice);
            assert_eq!(body["parallel_tool_calls"], parallel);
            assert_eq!(body["tools"][0]["strict"], true);
        }
    }
    for content in [None, Some(Value::Null)] {
        let mut body = request();
        let mut assistant = json!({"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}]});
        if let Some(content) = content {
            assistant["content"] = content;
        }
        body["messages"].as_array_mut().unwrap().push(assistant);
        prepare(&mut body, "strict").unwrap();
        assert_eq!(body["input"].as_array().unwrap().len(), 2);
        assert_eq!(body["input"][1]["type"], "function_call");
    }
}

#[test]
fn workbuddy_chat_structured_output_maps_without_schema_mutation() {
    for format in [
        json!({"type": "json_object"}),
        json!({"type": "json_schema", "json_schema": {
            "name": "answer", "strict": true, "schema": {"type": "object", "properties": {"value": {"type": "integer"}}, "required": ["value"], "additionalProperties": false}
        }}),
    ] {
        let mut body = request();
        body["response_format"] = format.clone();
        prepare(&mut body, "strict").unwrap();
        assert_eq!(body["text"]["format"]["type"], format["type"]);
        if format["type"] == "json_schema" {
            assert_eq!(
                body["text"]["format"]["schema"],
                format["json_schema"]["schema"]
            );
            assert_eq!(body["text"]["format"]["strict"], true);
        }
        assert!(body.get("response_format").is_none());
    }
}

#[test]
fn workbuddy_chat_rejects_unrepresentable_semantics_without_private_values() {
    for extra in [
        json!({"tools": [{"type": "custom", "custom": {"name": "private-canary"}}]}),
        json!({"parallel_tool_calls": "private-canary"}),
        json!({"tool_choice": {"type": "function", "function": {"name": null}}}),
        json!({"functions": [{"name": "private-canary"}]}),
        json!({"logprobs": true}),
        json!({"logit_bias": {"123": 4}}),
        json!({"modalities": ["text", "audio"]}),
        json!({"prediction": {"type": "content", "content": "private-canary"}}),
        json!({"input": "private-canary"}),
        json!({"instructions": "private-canary"}),
        json!({"max_output_tokens": 1}),
        json!({"reservation_token_bounds": {"gpt-6-sol": 1}}),
        json!({"messages": [{"role": "assistant", "content": null, "tool_calls": [{"id": "private-canary", "type": "function", "function": {"name": "lookup", "arguments": {}}}]}]}),
        json!({"messages": [{"role": "tool", "content": "private-canary"}]}),
    ] {
        let mut body = request();
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let error = prepare(&mut body, "strict").err().unwrap();
        assert!(!error.to_string().contains("private-canary"));
    }
}

#[test]
fn workbuddy_chat_sampling_limits_remain_operator_controlled() {
    for extra in [
        json!({"temperature": 0.7}),
        json!({"top_p": 0.9}),
        json!({"stop": ["END"]}),
        json!({"seed": 42}),
        json!({"max_tokens": 16}),
        json!({"max_output_tokens": 16}),
    ] {
        let mut body = request();
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(prepare(&mut body.clone(), "strict").is_err());
        prepare(&mut body, "provider_default").unwrap();
        for field in extra.as_object().unwrap().keys() {
            assert!(body.get(field).is_none());
        }
    }
}

#[test]
fn chat_output_limit_hint_rejects_invalid_and_conflicting_values() {
    for bad in [
        Value::Null,
        json!(0),
        json!(-1),
        json!(1.5),
        json!("16"),
        json!(MAX_REPORTED_TOKENS + 1),
    ] {
        let mut body = request();
        body["max_output_tokens"] = bad;
        assert!(prepare(&mut body, "provider_default").is_err());
    }
    let mut body = request();
    body["max_output_tokens"] = json!(16);
    body["max_completion_tokens"] = json!(16);
    assert!(prepare(&mut body, "provider_default").is_err());
}

#[test]
fn workbuddy_chat_refusal_history_named_tool_results_and_empty_stop_are_preserved() {
    let mut body = request();
    body["stop"] = json!([]);
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role": "assistant", "content": null, "refusal": "Cannot do that"}),
        json!({"role": "assistant", "content": null, "tool_calls": [{"type": "function", "id": "call_1", "function": {"name": "lookup", "arguments": "{}"}}]}),
        json!({"role": "tool", "tool_call_id": "call_1", "name": "lookup", "content": "ok"}),
    ]);
    let mut mismatched = body.clone();
    mismatched["messages"][3]["name"] = json!("different");
    assert!(prepare(&mut mismatched, "strict").is_err());
    prepare(&mut body, "strict").unwrap();
    assert_eq!(
        body["input"][1]["content"][0],
        json!({"type": "refusal", "refusal": "Cannot do that"})
    );
    assert_eq!(
        body["input"][3],
        json!({"type": "function_call_output", "call_id": "call_1", "output": "ok"})
    );
    let mut body = request();
    body["stop"] = json!(["\n\n"]);
    assert!(prepare(&mut body.clone(), "strict").is_err());
    prepare(&mut body, "provider_default").unwrap();
}

#[test]
fn workbuddy_chat_stream_bounds_total_retained_tool_arguments() {
    let mut translator = CodexChatStreamTranslator::new(Uuid::nil(), "public".into(), false);
    translator
        .translate_frame(&frame(
            &json!({"type": "response.output_item.added", "output_index": 0, "item": call(0, "")}),
        ))
        .unwrap();
    translator.retained_bytes = MAX_PROXY_RESPONSE_BODY;
    let event = json!({"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "fc_0", "delta": "{"});
    assert_eq!(
        translator.translate_frame(&frame(&event)),
        Err("upstream_response_too_large")
    );
}

#[test]
fn workbuddy_chat_tool_stream_matches_buffered_with_interleaved_arguments() {
    let first = call(0, "{\"key\":\"A\"}");
    let second = call(1, "{\"key\":\"B\"}");
    let output = json!([
        {"type": "reasoning", "id": "rs_1", "summary": []}, first, second,
        {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "Checking"}]}
    ]);
    let terminal = terminal(output);
    let mut translator = CodexChatStreamTranslator::new(Uuid::nil(), "gpt-6-sol".into(), true);
    let mut rendered = Vec::new();
    for event in [
        json!({"type": "response.output_item.added", "output_index": 1, "item": call(0, "")}),
        json!({"type": "response.output_item.added", "output_index": 2, "item": call(1, "")}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 2, "item_id": "fc_1", "delta": "{\"key\":"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "item_id": "fc_0", "delta": "{\"key\":\"A\"}"}),
        json!({"type": "response.function_call_arguments.done", "output_index": 2, "item_id": "fc_1", "arguments": "{\"key\":\"B\"}"}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": first}),
        terminal.clone(),
    ] {
        if let Some(bytes) = translator.translate_frame(&frame(&event)).unwrap() {
            rendered.extend_from_slice(&bytes);
        }
    }
    let chunks = chunks(&rendered);
    let mut calls = BTreeMap::<usize, Value>::new();
    let mut text = String::new();
    for chunk in &chunks {
        let delta = &chunk["choices"][0]["delta"];
        if let Some(content) = delta["content"].as_str() {
            text.push_str(content);
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"].as_u64().unwrap() as usize;
            let entry = calls
                .entry(index)
                .or_insert(json!({"function": {"arguments": ""}}));
            if !call["id"].is_null() {
                entry["id"] = call["id"].clone();
                entry["type"] = call["type"].clone();
                entry["function"]["name"] = call["function"]["name"].clone();
            }
            let arguments = entry["function"]["arguments"].as_str().unwrap().to_owned()
                + call["function"]["arguments"].as_str().unwrap();
            entry["function"]["arguments"] = json!(arguments);
        }
    }
    let buffered = translate_buffered_chat_response(
        BufferedCodexResponse {
            body: Bytes::from(serde_json::to_vec(&terminal["response"]).unwrap()),
            usage: canonical_responses_usage(&terminal["response"]).unwrap(),
            terminal: BufferedCodexTerminal::Completed,
        },
        Uuid::nil(),
        "gpt-6-sol",
    )
    .unwrap();
    let buffered: Value = serde_json::from_slice(&buffered.body).unwrap();
    assert_eq!(
        buffered["choices"][0]["message"]["tool_calls"],
        json!(calls.into_values().collect::<Vec<_>>())
    );
    assert_eq!(buffered["choices"][0]["message"]["content"], text);
    assert_eq!(text, "Checking");
    assert_eq!(
        chunks[chunks.len() - 2]["choices"][0]["finish_reason"],
        "tool_calls"
    );
    assert_eq!(chunks.last().unwrap()["usage"], buffered["usage"]);
    assert!(rendered.ends_with(b"data: [DONE]\n\n"));
}

#[test]
fn workbuddy_chat_terminal_only_tool_output_and_incomplete_finish_reasons() {
    for incomplete in [false, true] {
        let mut event = terminal(json!([call(0, "{}")]));
        if incomplete {
            event["type"] = json!("response.incomplete");
            event["response"]["status"] = json!("incomplete");
            event["response"]["error"] = Value::Null;
            event["response"]["incomplete_details"] = json!({"reason": "max_output_tokens"});
        }
        let mut translator = CodexChatStreamTranslator::new(Uuid::nil(), "public".into(), false);
        let rendered = translator.translate_frame(&frame(&event)).unwrap().unwrap();
        let chunks = chunks(&rendered);
        assert_eq!(
            chunks[0]["choices"][0]["delta"]["tool_calls"][0]["id"],
            "call_0"
        );
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            if incomplete { "length" } else { "tool_calls" }
        );
        assert!(translator.translate_frame(&frame(&event)).is_err());
    }
}

#[test]
fn workbuddy_chat_stream_rejects_conflicting_or_missing_tool_identity_and_arguments() {
    for event in [
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "wrong", "delta": "{}"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "item_id": "fc_0", "delta": "{}"}),
        json!({"type": "response.function_call_arguments.done", "output_index": 0, "item_id": "fc_0", "arguments": "conflict"}),
        terminal(json!([])),
        terminal(json!([call(0, "conflict")])),
        terminal(json!([call(1, "{}")])),
    ] {
        let mut translator = CodexChatStreamTranslator::new(Uuid::nil(), "public".into(), false);
        translator.translate_frame(&frame(&json!({"type": "response.output_item.added", "output_index": 0, "item": call(0, "{")}))).unwrap();
        assert!(translator.translate_frame(&frame(&event)).is_err());
    }
    for item in [
        json!({"type": "custom_tool_call"}),
        json!({"type": "function_call", "name": "lookup", "arguments": "{}"}),
    ] {
        assert!(response_chat_content(&json!({"output": [item]}), false).is_err());
    }
}

fn native_text_item() -> Value {
    json!({"type": "message", "id": "msg_synthetic", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": "OK", "annotations": []}]})
}

fn assert_completed_chat_stream(rendered: &[u8], finish_reason: &str) -> Vec<Value> {
    let text = std::str::from_utf8(rendered).unwrap();
    assert!(text.ends_with("data: [DONE]\n\n"));
    assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
    let chunks = chunks(rendered);
    assert!(chunks.iter().all(|chunk| chunk.get("error").is_none()));
    let finished = chunks
        .iter()
        .filter(|chunk| !chunk["choices"][0]["finish_reason"].is_null())
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["choices"][0]["finish_reason"], finish_reason);
    let usage = chunks
        .iter()
        .filter(|chunk| !chunk["usage"].is_null())
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["usage"]["prompt_tokens"], 3);
    assert_eq!(usage[0]["usage"]["completion_tokens"], 2);
    assert_eq!(usage[0]["usage"]["total_tokens"], 5);
    chunks
}

#[test]
fn native_chat_text_item_done_reconstructs_empty_terminal_without_repeating_deltas() {
    for output in [json!([]), json!([native_text_item()])] {
        let mut translator = CodexChatStreamTranslator::new(Uuid::nil(), "public".into(), true);
        let mut rendered = Vec::new();
        let mut completed = terminal(output);
        completed["response"]["status"] = json!("completed");
        for event in [
            json!({"type": "response.created", "response": {"id": "resp_synthetic", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "msg_synthetic", "role": "assistant", "status": "in_progress", "content": []}}),
            json!({"type": "response.content_part.added", "output_index": 0, "content_index": 0, "item_id": "msg_synthetic", "part": {"type": "output_text", "text": "", "annotations": []}}),
            json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "item_id": "msg_synthetic", "delta": "O"}),
            json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "item_id": "msg_synthetic", "delta": "K"}),
            json!({"type": "response.output_text.done", "output_index": 0, "content_index": 0, "item_id": "msg_synthetic", "text": "OK"}),
            json!({"type": "response.content_part.done", "output_index": 0, "content_index": 0, "item_id": "msg_synthetic", "part": {"type": "output_text", "text": "OK", "annotations": []}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": native_text_item()}),
            completed,
        ] {
            if let Some(bytes) = translator.translate_frame(&frame(&event)).unwrap() {
                rendered.extend_from_slice(&bytes);
            }
        }
        assert!(
            translator
                .translate_frame(b"data: [DONE]\n\n")
                .unwrap()
                .is_none()
        );
        let chunks = assert_completed_chat_stream(&rendered, "stop");
        let text = chunks
            .iter()
            .filter_map(|chunk| chunk["choices"][0]["delta"]["content"].as_str())
            .collect::<String>();
        assert_eq!(text, "OK");
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| chunk["choices"][0]["delta"]["role"] == "assistant")
                .count(),
            1
        );
    }
}

#[test]
fn native_chat_tool_item_done_reconstructs_empty_terminal_without_repeating_arguments() {
    let item = call(0, "{\"key\":\"A\"}");
    for output in [json!([]), json!([item])] {
        let mut translator = CodexChatStreamTranslator::new(Uuid::nil(), "public".into(), true);
        let mut rendered = Vec::new();
        for event in [
            json!({"type": "response.created", "response": {"id": "resp_synthetic"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": call(0, "")}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "fc_0", "delta": "{\"key\":"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "fc_0", "delta": "\"A\"}"}),
            json!({"type": "response.function_call_arguments.done", "output_index": 0, "item_id": "fc_0", "arguments": item["arguments"]}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
            terminal(output),
        ] {
            if let Some(bytes) = translator.translate_frame(&frame(&event)).unwrap() {
                rendered.extend_from_slice(&bytes);
            }
        }
        let chunks = assert_completed_chat_stream(&rendered, "tool_calls");
        let calls = chunks
            .iter()
            .flat_map(|chunk| {
                chunk["choices"][0]["delta"]["tool_calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
            })
            .collect::<Vec<_>>();
        assert!(calls.iter().all(|item| item["index"] == 0));
        assert_eq!(
            calls.iter().filter(|item| item["id"] == "call_0").count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|item| item["function"]["name"] == "lookup")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter_map(|item| item["function"]["arguments"].as_str())
                .collect::<String>(),
            "{\"key\":\"A\"}"
        );
    }
}

#[test]
fn native_chat_item_done_conflicting_with_terminal_output_cannot_complete_successfully() {
    for (done, field, replacement) in [
        (native_text_item(), "id", json!("msg_conflict")),
        (
            native_text_item(),
            "content",
            json!([{"type": "output_text", "text": "different"}]),
        ),
        (call(0, "{}"), "arguments", json!("{\"different\":true}")),
        (call(0, "{}"), "name", json!("different_tool")),
        (call(0, "{}"), "call_id", json!("call_conflict")),
    ] {
        let mut conflicting = done.clone();
        conflicting[field] = replacement;
        let mut translator = CodexChatStreamTranslator::new(Uuid::nil(), "public".into(), true);
        let emitted = translator
            .translate_frame(&frame(
                &json!({"type": "response.output_item.done", "output_index": 0, "item": done}),
            ))
            .unwrap();
        assert!(
            translator
                .translate_frame(&frame(&terminal(json!([conflicting]))))
                .is_err()
        );
        if let Some(emitted) = emitted {
            let rendered = std::str::from_utf8(&emitted).unwrap();
            assert!(!rendered.contains("[DONE]"));
            assert!(
                chunks(&emitted)
                    .iter()
                    .all(|chunk| chunk["choices"][0]["finish_reason"].is_null()
                        && chunk["usage"].is_null())
            );
        }
    }
}

#[test]
fn native_chat_partial_item_done_requires_matching_terminal_item_at_same_index() {
    let done = native_text_item();
    let reasoning = json!({"type": "reasoning", "id": "rs_synthetic", "summary": []});
    let mut conflicting = done.clone();
    conflicting["content"][0]["text"] = json!("different");
    for (output, succeeds) in [
        (json!([reasoning, done, call(0, "{}")]), true),
        (json!([]), false),
        (json!([reasoning, conflicting, call(0, "{}")]), false),
        (json!([done, reasoning, call(0, "{}")]), false),
        (json!([reasoning]), false),
    ] {
        let mut translator = CodexChatStreamTranslator::new(Uuid::nil(), "public".into(), true);
        translator
            .translate_frame(&frame(
                &json!({"type": "response.output_item.done", "output_index": 1, "item": done}),
            ))
            .unwrap();
        let completed = translator.translate_frame(&frame(&terminal(output)));
        if succeeds {
            let rendered = completed.unwrap().unwrap();
            let chunks = assert_completed_chat_stream(&rendered, "tool_calls");
            assert_eq!(
                chunks
                    .iter()
                    .filter_map(|chunk| chunk["choices"][0]["delta"]["content"].as_str())
                    .collect::<String>(),
                "OK"
            );
        } else {
            assert!(completed.is_err());
        }
    }
}
