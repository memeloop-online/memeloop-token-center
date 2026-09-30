use super::*;

fn agent_request(model: &str, stream: bool) -> Value {
    let mut request = json!({
        "model": model, "stream": stream,
        "messages": [{"role": "user", "content": "Look up a synthetic value."}],
        "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object", "properties": {"key": {"type": "string"}}}}}],
        "tool_choice": "auto", "parallel_tool_calls": false, "reasoning_effort": "medium",
        "metadata": {"fixture": "workbuddy"}, "logprobs": false,
        "temperature": 1, "top_p": 1, "max_completion_tokens": 64
    });
    if stream {
        request["stream_options"] = json!({"include_usage": true});
    }
    request
}

fn tool_response() -> String {
    let item = json!({"type": "function_call", "id": "fc_synthetic", "call_id": "call_synthetic", "name": "lookup", "arguments": "{\"key\":\"A\"}"});
    let mut added = item.clone();
    added["arguments"] = json!("");
    [
        json!({"type": "response.created", "response": {"id": "resp-workbuddy"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": added}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "fc_synthetic", "delta": "{\"key\":"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "fc_synthetic", "delta": "\"A\"}"}),
        json!({"type": "response.function_call_arguments.done", "output_index": 0, "item_id": "fc_synthetic", "arguments": item["arguments"]}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": {"id": "resp-workbuddy", "object": "response", "output": [item], "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}}}),
    ].iter().map(|event| format!("data: {event}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}

#[tokio::test]
async fn workbuddy_agent_tool_round_trip_streaming_and_buffered_settles_once_per_turn() {
    for stream in [false, true] {
        let fixture = codex_route_fixture(&format!("workbuddy-{stream}")).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(codex_transport::RESPONSES_PATH))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(tool_response(), "text/event-stream"),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        let original = agent_request(&fixture.model, stream);
        let response = send_codex_route(
            &fixture,
            &upstream,
            "/v1/chat/completions",
            original.clone(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        let tool_call = if stream {
            let rendered = String::from_utf8(body.to_vec()).unwrap();
            assert!(rendered.ends_with("data: [DONE]\n\n"));
            assert!(rendered.contains("\"finish_reason\":\"tool_calls\""));
            assert!(rendered.contains("\"prompt_tokens\":3"));
            let chunks = rendered
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter(|data| *data != "[DONE]")
                .map(|data| serde_json::from_str::<Value>(data).unwrap())
                .collect::<Vec<_>>();
            let mut tool_call = Value::Null;
            let mut arguments = String::new();
            for chunk in chunks {
                if let Some(calls) = chunk["choices"][0]["delta"]["tool_calls"].as_array() {
                    for call in calls {
                        assert_eq!(call["index"], 0);
                        if tool_call.is_null() {
                            tool_call = call.clone();
                            tool_call.as_object_mut().unwrap().remove("index");
                        }
                        arguments.push_str(call["function"]["arguments"].as_str().unwrap());
                    }
                }
            }
            tool_call["function"]["arguments"] = json!(arguments);
            tool_call
        } else {
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["model"], fixture.model);
            assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
            assert!(body["choices"][0]["message"]["content"].is_null());
            body["choices"][0]["message"]["tool_calls"][0].clone()
        };
        assert_eq!(
            tool_call,
            json!({"id": "call_synthetic", "type": "function", "function": {"name": "lookup", "arguments": "{\"key\":\"A\"}"}})
        );
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].headers[header::AUTHORIZATION],
            "Bearer upstream-access-secret"
        );
        let wire: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(wire["parallel_tool_calls"], false);
        assert_eq!(wire["tools"][0]["name"], "lookup");
        assert_eq!(wire["tools"][0]["strict"], false);
        assert_eq!(wire["store"], false);
        assert_eq!(wire["model"], fixture.upstream_model);
        wait_for_request_settlement(&fixture, 1).await;
        upstream.verify().await;
        upstream.reset().await;
        Mock::given(method("POST"))
            .and(path(codex_transport::RESPONSES_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                streaming_codex_sse("Value is 7", "Value ", "is 7"),
                "text/event-stream",
            ))
            .expect(1)
            .mount(&upstream)
            .await;
        let mut continuation = original;
        continuation["messages"].as_array_mut().unwrap().extend([
            json!({"role": "assistant", "content": null, "tool_calls": [tool_call]}),
            json!({"role": "tool", "tool_call_id": "call_synthetic", "content": "{\"value\":7}"}),
        ]);
        let response =
            send_codex_route(&fixture, &upstream, "/v1/chat/completions", continuation).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        if stream {
            let rendered = String::from_utf8(body.to_vec()).unwrap();
            assert!(rendered.contains("\"finish_reason\":\"stop\""));
            assert!(rendered.ends_with("data: [DONE]\n\n"));
        } else {
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["choices"][0]["message"]["content"], "Value is 7");
        }
        let requests = upstream.received_requests().await.unwrap();
        let wire: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            wire["input"][1],
            json!({"type": "function_call", "call_id": "call_synthetic", "name": "lookup", "arguments": "{\"key\":\"A\"}"})
        );
        assert_eq!(
            wire["input"][2],
            json!({"type": "function_call_output", "call_id": "call_synthetic", "output": "{\"value\":7}"})
        );
        wait_for_request_settlement(&fixture, 2).await;
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(row.status_code, Some(200));
            assert_eq!((row.input_tokens, row.output_tokens), (3, 2));
            assert_exactly_once_side_effects(&fixture, row.request_id, None).await;
        }
        upstream.verify().await;
    }
}
