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
