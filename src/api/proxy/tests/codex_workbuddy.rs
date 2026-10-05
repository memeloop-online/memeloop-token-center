use super::*;
use futures_util::StreamExt;

fn agent_request(model: &str, stream: bool) -> Value {
    let mut request = json!({
        "model": model, "stream": stream,
        "messages": [{"role": "user", "content": "Look up a synthetic value."}],
        "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object", "properties": {"key": {"type": "string"}}}}}],
        "tool_choice": "auto", "parallel_tool_calls": false, "reasoning_effort": "medium",
        "metadata": {"fixture": "workbuddy"}, "logprobs": false,
        "temperature": 1, "top_p": 1, "max_tokens": 16
    });
    if stream {
        request["stream_options"] = json!({"include_usage": true});
    }
    request
}

fn tool_response(empty_terminal: bool) -> String {
    let item = json!({"type": "function_call", "id": "fc_synthetic", "call_id": "call_synthetic", "name": "lookup", "arguments": "{\"key\":\"A\"}"});
    let mut added = item.clone();
    added["arguments"] = json!("");
    let output = if empty_terminal {
        json!([])
    } else {
        json!([item])
    };
    [
        json!({"type": "response.created", "response": {"id": "resp-workbuddy"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": added}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "fc_synthetic", "delta": "{\"key\":"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "fc_synthetic", "delta": "\"A\"}"}),
        json!({"type": "response.function_call_arguments.done", "output_index": 0, "item_id": "fc_synthetic", "arguments": item["arguments"]}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": {"id": "resp-workbuddy", "object": "response", "status": "completed", "output": output, "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}}}),
    ].iter().map(|event| format!("data: {event}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}

#[tokio::test]
async fn workbuddy_agent_authentication_and_credit_admission_are_required() {
    let fixture = codex_route_fixture("workbuddy-admission").await;
    codex_output_limits::enable_provider_default_limits(&fixture).await;
    let upstream = MockServer::start().await;
    let request = Request::post("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&agent_request(&fixture.model, false)).unwrap(),
        ))
        .unwrap();
    let response = codex_transport::with_test_endpoint(
        upstream.uri(),
        router_for_role(fixture.state.clone(), RuntimeRole::Gateway).oneshot(request),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE credit_accounts SET available_micros = 0 WHERE id = $1")
        .bind(fixture.credit_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/chat/completions",
        agent_request(&fixture.model, false),
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_reservations")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    pool.close().await;
}

#[tokio::test]
async fn workbuddy_agent_tool_round_trip_streaming_and_buffered_settles_once_per_turn() {
    for stream in [false, true] {
        let fixture = codex_route_fixture(&format!("workbuddy-{stream}")).await;
        codex_output_limits::enable_provider_default_limits(&fixture).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(codex_transport::RESPONSES_PATH))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(tool_response(false), "text/event-stream"),
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
        assert!(wire.get("max_tokens").is_none());
        assert!(wire.get("metadata").is_none());
        assert_eq!(wire["reasoning"]["effort"], "medium");
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

fn native_text_response(conflicting_terminal: bool) -> String {
    let item = json!({"type": "message", "id": "msg_synthetic", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": "OK", "annotations": []}]});
    let mut conflicting = item.clone();
    conflicting["content"][0]["text"] = json!("different");
    let output = if conflicting_terminal {
        json!([conflicting])
    } else {
        json!([])
    };
    [
        json!({"type": "response.created", "response": {"id": "resp-workbuddy", "status": "in_progress"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "msg_synthetic", "role": "assistant", "status": "in_progress", "content": []}}),
        json!({"type": "response.content_part.added", "output_index": 0, "content_index": 0, "item_id": "msg_synthetic", "part": {"type": "output_text", "text": "", "annotations": []}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "item_id": "msg_synthetic", "delta": "O"}),
        json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "item_id": "msg_synthetic", "delta": "K"}),
        json!({"type": "response.output_text.done", "output_index": 0, "content_index": 0, "item_id": "msg_synthetic", "text": "OK"}),
        json!({"type": "response.content_part.done", "output_index": 0, "content_index": 0, "item_id": "msg_synthetic", "part": item["content"][0]}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": {"id": "resp-workbuddy", "object": "response", "status": "completed", "output": output, "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}}}),
    ].iter().map(|event| format!("data: {event}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}

fn chat_stream_chunks(rendered: &str) -> Vec<Value> {
    rendered
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

#[tokio::test]
async fn workbuddy_native_empty_terminal_text_and_tools_complete_and_settle_through_router() {
    for stream in [false, true] {
        for tool in [false, true] {
            let fixture = codex_route_fixture(&format!("workbuddy-empty-{stream}-{tool}")).await;
            codex_output_limits::enable_provider_default_limits(&fixture).await;
            let upstream = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(codex_transport::RESPONSES_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_raw(
                    if tool {
                        tool_response(true)
                    } else {
                        native_text_response(false)
                    },
                    "text/event-stream",
                ))
                .expect(1)
                .mount(&upstream)
                .await;
            let response = send_codex_route(
                &fixture,
                &upstream,
                "/v1/chat/completions",
                agent_request(&fixture.model, stream),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let request_id =
                Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
            let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                .await
                .unwrap();
            let finish_reason = if tool { "tool_calls" } else { "stop" };
            if stream {
                let rendered = std::str::from_utf8(&body).unwrap();
                assert!(rendered.ends_with("data: [DONE]\n\n"));
                assert_eq!(rendered.matches("data: [DONE]\n\n").count(), 1);
                let chunks = chat_stream_chunks(rendered);
                assert!(chunks.iter().all(|chunk| chunk.get("error").is_none()));
                let finished = chunks
                    .iter()
                    .filter_map(|chunk| chunk["choices"][0]["finish_reason"].as_str())
                    .collect::<Vec<_>>();
                assert_eq!(finished, vec![finish_reason]);
                let usage = chunks
                    .iter()
                    .filter(|chunk| !chunk["usage"].is_null())
                    .collect::<Vec<_>>();
                assert_eq!(usage.len(), 1);
                assert_eq!(usage[0]["usage"]["prompt_tokens"], 3);
                assert_eq!(usage[0]["usage"]["completion_tokens"], 2);
                assert_eq!(usage[0]["usage"]["total_tokens"], 5);
                let text = chunks
                    .iter()
                    .filter_map(|chunk| chunk["choices"][0]["delta"]["content"].as_str())
                    .collect::<String>();
                assert_eq!(text, if tool { "" } else { "OK" });
                let calls = chunks
                    .iter()
                    .flat_map(|chunk| {
                        chunk["choices"][0]["delta"]["tool_calls"]
                            .as_array()
                            .into_iter()
                            .flatten()
                    })
                    .collect::<Vec<_>>();
                if tool {
                    assert!(calls.iter().all(|call| call["index"] == 0));
                    assert_eq!(
                        calls
                            .iter()
                            .filter(|call| call["id"] == "call_synthetic")
                            .count(),
                        1
                    );
                    assert_eq!(
                        calls
                            .iter()
                            .filter(|call| call["function"]["name"] == "lookup")
                            .count(),
                        1
                    );
                    assert_eq!(
                        calls
                            .iter()
                            .filter_map(|call| call["function"]["arguments"].as_str())
                            .collect::<String>(),
                        "{\"key\":\"A\"}"
                    );
                } else {
                    assert!(calls.is_empty());
                }
            } else {
                let body: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["model"], fixture.model);
                assert_eq!(body["choices"][0]["finish_reason"], finish_reason);
                assert_eq!(body["usage"]["prompt_tokens"], 3);
                assert_eq!(body["usage"]["completion_tokens"], 2);
                assert_eq!(body["usage"]["total_tokens"], 5);
                if tool {
                    assert!(body["choices"][0]["message"]["content"].is_null());
                    assert_eq!(
                        body["choices"][0]["message"]["tool_calls"],
                        json!([
                            {"id": "call_synthetic", "type": "function", "function": {"name": "lookup", "arguments": "{\"key\":\"A\"}"}}
                        ])
                    );
                } else {
                    assert_eq!(body["choices"][0]["message"]["content"], "OK");
                }
            }
            let requests = upstream.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            let wire: Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert!(wire.get("metadata").is_none());
            assert_eq!(wire["reasoning"]["effort"], "medium");
            assert_eq!(wire["parallel_tool_calls"], false);
            assert_eq!(wire["tool_choice"], "auto");
            assert_eq!(wire["tools"][0]["name"], "lookup");
            assert_eq!(wire["tools"][0]["strict"], false);
            assert_eq!(wire["input"][0]["role"], "user");
            assert_eq!(
                wire["input"][0]["content"][0]["text"],
                "Look up a synthetic value."
            );
            wait_for_request_settlement(&fixture, 1).await;
            drain_archive_capture(&fixture).await;
            let rows = fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].request_id, request_id);
            assert_eq!(rows[0].status_code, Some(200));
            assert!(rows[0].error_code.is_none());
            assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 2));
            assert_exactly_once_side_effects(&fixture, request_id, None).await;
            upstream.verify().await;
        }
    }
}

#[tokio::test]
async fn workbuddy_native_conflicting_item_done_and_terminal_fails_through_streaming_router() {
    let fixture = codex_route_fixture("workbuddy-conflicting-terminal").await;
    codex_output_limits::enable_provider_default_limits(&fixture).await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(native_text_response(true), "text/event-stream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/chat/completions",
        agent_request(&fixture.model, true),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    assert_failed_chat_stream(response.into_body()).await;
    wait_for_request_settlement(&fixture, 1).await;
    drain_archive_capture(&fixture).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].request_id, request_id);
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_invalid_response")
    );
    assert_exactly_once_side_effects(&fixture, request_id, None).await;
    assert_chat_failure_account_health(&fixture, true).await;
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    upstream.verify().await;
}

async fn assert_failed_chat_stream(body: Body) {
    let mut stream = body.into_data_stream();
    let mut delivered = Vec::new();
    let mut failed = false;
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => delivered.extend_from_slice(&bytes),
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
    })
    .await
    .expect("Chat mapping failure must terminate the body");
    assert!(failed, "unmappable Chat output must not silently finish");
    let rendered = std::str::from_utf8(&delivered).unwrap();
    assert!(!rendered.contains("data: [DONE]"));
    assert!(chat_stream_chunks(rendered).iter().all(|chunk| chunk["choices"][0]["finish_reason"].is_null() && chunk["usage"].is_null()));
}

async fn assert_chat_failure_account_health(fixture: &CodexRouteFixture, invalid_response: bool) {
    let _completed_lifecycles = tokio::time::timeout(
        Duration::from_secs(3),
        fixture
            .state
            .proxy_lifecycle_permits
            .acquire_many(fixture.state.config.proxy_lifecycle_concurrency),
    )
    .await
    .expect("request lifecycle must finish before inspecting account health")
    .unwrap();
    fixture.state.routing_persistence.drain_for_test().await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    if invalid_response {
        let health = sqlx::query("SELECT consecutive_failures, cooldown_until, last_failure_kind FROM upstream_account_health WHERE upstream_account_id = $1")
            .bind(fixture.upstream_account_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(health.get::<i64, _>("consecutive_failures"), 1);
        assert_eq!(
            health.get::<String, _>("last_failure_kind"),
            "invalid_response"
        );
        assert!(health.get::<i64, _>("cooldown_until") > 0);
    } else {
        let failures: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upstream_account_health WHERE upstream_account_id = $1 AND (consecutive_failures <> 0 OR cooldown_until <> 0 OR last_failure_kind <> '')")
            .bind(fixture.upstream_account_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(
            failures, 0,
            "a local Chat mapping limitation must not cool down the shared account"
        );
    }
    pool.close().await;
}

#[tokio::test]
async fn workbuddy_unmappable_native_chat_output_fails_without_replay_or_account_cooldown() {
    for stream in [false, true] {
        let fixture = codex_route_fixture(&format!("workbuddy-unmappable-{stream}")).await;
        codex_output_limits::enable_provider_default_limits(&fixture).await;
        let upstream = MockServer::start().await;
        add_native_chat_standby_route(
            &fixture,
            &format!("workbuddy-standby-{stream}"),
            &upstream.uri(),
        )
        .await;
        let item = json!({"type": "web_search_call", "id": "ws_synthetic", "status": "completed",
            "action": {"type": "search", "query": "synthetic"}});
        let body = [
            json!({"type": "response.created", "response": {"id": "resp-workbuddy"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
            json!({"type": "response.completed", "response": {"id": "resp-workbuddy", "status": "completed", "output": [item], "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}}}),
        ].iter().map(|event| format!("data: {event}\n\n")).collect::<String>() + "data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path(codex_transport::RESPONSES_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": []})))
            .expect(0)
            .mount(&upstream)
            .await;
        let response = send_codex_route(
            &fixture,
            &upstream,
            "/v1/chat/completions",
            agent_request(&fixture.model, stream),
        )
        .await;
        let request_id =
            Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
        if stream {
            assert_eq!(response.status(), StatusCode::OK);
            assert_failed_chat_stream(response.into_body()).await;
        } else {
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert!(body.get("error").is_some());
            assert!(body.get("choices").is_none());
        }
        wait_for_request_settlement(&fixture, 1).await;
        drain_archive_capture(&fixture).await;
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, request_id);
        assert_eq!(rows[0].status_code, Some(502));
        assert_eq!(
            rows[0].error_code.as_deref(),
            Some("upstream_unsupported_chat_output")
        );
        assert_exactly_once_side_effects(&fixture, request_id, None).await;
        assert_chat_failure_account_health(&fixture, false).await;
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
        upstream.verify().await;
    }
}
