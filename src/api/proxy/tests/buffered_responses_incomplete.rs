use super::*;

#[tokio::test]
async fn chat_transport_preserves_complete_incomplete_and_rejects_invalid_terminals() {
    for finish_reason in ["length", "content_filter", "stop", "unknown"] {
        let upstream = MockServer::start().await;
        let fixture = response_usage_fixture_with_uri_contract_driver_model_and_config(
            "chat-incomplete-terminal",
            upstream.uri(),
            0,
            None,
            "http-json",
            "chat-model",
            json!({"responses_transport":"chat_completions"}),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_partial_json(json!({"max_tokens":32,"stream":false})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"chat-incomplete",
                "object":"chat.completion",
                "choices":[{"index":0,"finish_reason":finish_reason,
                    "message":{"role":"assistant","content":"Partial output",
                        "reasoning_content":"Reasoning summary"}}],
                "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12,
                    "completion_tokens_details":{"reasoning_tokens":1}}
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        let response = send_response_usage_request(
            &fixture,
            &json!({"model":fixture.model,"input":"bounded request",
                "stream":false,"max_output_tokens":32}),
        )
        .await;
        let expected_status = if finish_reason == "unknown" { 502 } else { 200 };
        assert_eq!(response.status().as_u16(), expected_status);
        let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&delivered).unwrap();
        if finish_reason != "unknown" {
            let truncated = finish_reason != "stop";
            assert_eq!(
                value["status"],
                if truncated { "incomplete" } else { "completed" }
            );
            if truncated {
                assert_eq!(
                    value["incomplete_details"]["reason"],
                    if finish_reason == "length" {
                        "max_output_tokens"
                    } else {
                        "content_filter"
                    }
                );
            }
            assert_eq!(value["output"][0]["type"], "reasoning");
            assert_eq!(value["output"][1]["content"][0]["text"], "Partial output");
            assert_eq!(
                value["usage"]["output_tokens_details"]["reasoning_tokens"],
                1
            );
        } else {
            assert!(value["error"].is_object());
        }
        wait_for_request_settlement(&fixture, 1).await;
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status_code, Some(i64::from(expected_status)));
        assert_eq!(
            rows[0].lifecycle_state,
            if finish_reason == "stop" {
                crate::model::RequestLifecycleState::Succeeded
            } else {
                crate::model::RequestLifecycleState::Failed
            }
        );
        if finish_reason != "unknown" {
            assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (10, 2));
            assert!(rows[0].cost.parse::<Decimal>().unwrap() > Decimal::ZERO);
            drain_completed_response_archive(&fixture).await;
            let refs = fixture
                .state
                .db
                .request_archive_refs(fixture.key_id, rows[0].request_id)
                .await
                .unwrap();
            let locator = refs.response_object.unwrap();
            assert_eq!(
                fixture.state.archive.get(&locator).await.unwrap(),
                delivered.as_ref()
            );
        }
        assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
        upstream.verify().await;
    }
}

fn incomplete(reason: &str) -> Value {
    json!({"id":"resp-incomplete-fixture", "object":"response", "status":"incomplete",
        "error":null, "incomplete_details":{"reason":reason}, "output":[
            {"type":"message","role":"assistant","status":"incomplete",
                "content":[{"type":"output_text","text":"Partial output","annotations":[]}]}],
        "usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12,
            "input_tokens_details":{"cached_tokens":6}}})
}

#[test]
fn only_explicit_valid_buffered_incomplete_counters_are_trusted() {
    for reason in ["max_output_tokens", "content_filter"] {
        let usage = trusted_buffered_responses_incomplete_usage(
            &serde_json::to_vec(&incomplete(reason)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            (
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.output_tokens
            ),
            (4, 6, 2)
        );
    }
    for (pointer, replacement) in [
        ("/status", json!("completed")),
        ("/status", json!("in_progress")),
        ("/status", json!("failed")),
        ("/status", json!("cancelled")),
        ("/error", json!({"message":"private provider failure"})),
        ("/id", json!("\n")),
        ("/object", json!("chat.completion")),
        ("/type", json!("error")),
        ("/output", json!("invalid")),
        (
            "/incomplete_details/reason",
            json!("unknown_terminal_reason"),
        ),
        ("/usage", Value::Null),
        ("/usage/output_tokens", Value::Null),
        ("/usage/total_tokens", json!("12")),
        ("/usage/total_tokens", json!(13)),
        ("/usage/input_tokens_details/cached_tokens", json!(11)),
    ] {
        let mut value = incomplete("max_output_tokens");
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        value.pointer_mut(parent).unwrap()[key] = replacement;
        assert!(
            trusted_buffered_responses_incomplete_usage(&serde_json::to_vec(&value).unwrap())
                .is_none()
        );
    }
    let mut value = incomplete("max_output_tokens");
    value["usage"]
        .as_object_mut()
        .unwrap()
        .remove("total_tokens");
    assert!(
        trusted_buffered_responses_incomplete_usage(&serde_json::to_vec(&value).unwrap()).is_none()
    );
    let duplicate = serde_json::to_string(&incomplete("max_output_tokens"))
        .unwrap()
        .replacen(
            "\"status\":\"incomplete\"",
            "\"status\":\"failed\",\"status\":\"incomplete\"",
            1,
        );
    assert!(trusted_buffered_responses_incomplete_usage(duplicate.as_bytes()).is_none());
}

#[tokio::test]
async fn buffered_incomplete_preserves_http_and_settles_only_valid_actual_usage_once() {
    for case in [
        "valid",
        "invalid-usage",
        "malformed-message-content",
        "malformed-output-text",
        "malformed-reasoning-summary",
        "malformed-function-call",
        "extension",
    ] {
        let valid_usage = case != "invalid-usage";
        let valid_output = matches!(case, "valid" | "extension");
        let upstream = MockServer::start().await;
        let fixture = response_usage_fixture("buffered-incomplete", &upstream, 0).await;
        fixture
            .state
            .db
            .upsert_model_price_tier(
                &fixture.model,
                "USD",
                "default",
                Decimal::from(2),
                Decimal::ONE,
                Decimal::from(2),
                Decimal::from(3),
                false,
            )
            .await
            .unwrap();
        let mut body = incomplete("max_output_tokens");
        match case {
            "invalid-usage" => {
                body["usage"]
                    .as_object_mut()
                    .unwrap()
                    .remove("total_tokens");
            }
            "malformed-message-content" => body["output"][0]["content"] = json!("invalid"),
            "malformed-output-text" => body["output"][0]["content"][0]["text"] = json!(7),
            "malformed-reasoning-summary" => {
                body["output"] = json!([{"type":"reasoning","summary":"invalid"}]);
            }
            "malformed-function-call" => {
                body["output"] = json!([{"type":"function_call","call_id":"call_partial",
                    "name":"partial","arguments":{}}]);
            }
            "extension" => {
                body["provider_extension"] = json!({"opaque":true});
                body["output"].as_array_mut().unwrap().push(json!({
                    "type":"provider_extension","payload":{"opaque":true}}));
                body["output"][0]["content"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({
                    "type":"provider_content_extension","payload":{"opaque":true}}));
            }
            _ => {}
        }
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .expect(1)
            .mount(&upstream)
            .await;
        let response = send_response_usage_request(&fixture,
            &json!({"model":fixture.model,"input":"bounded fixture request", "stream":false,"max_output_tokens":32}))
            .await;
        assert_eq!(
            response.status(),
            if valid_usage && valid_output {
                StatusCode::OK
            } else {
                StatusCode::BAD_GATEWAY
            }
        );
        let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        let delivered_json: Value = serde_json::from_slice(&delivered).unwrap();
        if valid_usage && valid_output {
            assert_eq!(delivered_json, body);
        } else {
            assert!(delivered_json["error"].is_object());
        }
        if valid_usage {
            wait_for_request_settlement(&fixture, 1).await;
        } else {
            cancellation::assert_single_unknown_receipt(
                &fixture,
                502,
                Some("upstream_failed_response"),
            )
            .await;
        }
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(
            row.status_code,
            Some(if valid_usage && valid_output {
                200
            } else {
                502
            })
        );
        assert_eq!(
            row.lifecycle_state,
            crate::model::RequestLifecycleState::Failed
        );
        if valid_usage {
            assert_eq!(
                row.usage_basis,
                Some(crate::model::RequestUsageBasis::ProviderReported)
            );
        } else {
            assert!(row.usage.tokens.is_none());
            assert_eq!(row.billing.cost, None);
        }
        assert_eq!(
            (row.input_tokens, row.cached_input_tokens, row.output_tokens),
            if valid_usage { (10, 6, 2) } else { (0, 0, 0) }
        );
        assert_eq!(
            row.cost.parse::<Decimal>().unwrap(),
            if valid_usage {
                Decimal::new(20, 6)
            } else {
                Decimal::ZERO
            }
        );
        assert_eq!(
            row.error_code.as_deref(),
            Some(if !valid_usage {
                "upstream_failed_response"
            } else if !valid_output {
                "upstream_invalid_response"
            } else {
                "upstream_incomplete_response"
            })
        );
        let events = fixture
            .state
            .db
            .all_request_events_after(0, None, 500)
            .await
            .unwrap();
        if valid_usage {
            let finished = events
                .iter()
                .find(|event| event.request_id == row.request_id && event.event_kind == "finished")
                .unwrap();
            assert_eq!(finished.status_code, row.status_code);
            assert_eq!(finished.lifecycle_state, row.lifecycle_state);
            assert_eq!(finished.error_code, row.error_code);
            assert_exactly_once_side_effects(&fixture, row.request_id, None).await;
            drain_completed_response_archive(&fixture).await;
        } else {
            assert!(
                !events
                    .iter()
                    .any(|event| event.request_id == row.request_id
                        && event.event_kind == "finished")
            );
        }
        let refs = fixture
            .state
            .db
            .request_archive_refs(fixture.key_id, row.request_id)
            .await
            .unwrap();
        assert_eq!(refs.view.status_code, row.status_code);
        assert_eq!(refs.view.lifecycle_state, row.lifecycle_state);
        let locator = refs.response_object.unwrap();
        if valid_usage {
            assert!(!locator.starts_with("gap://"));
            assert_eq!(
                fixture.state.archive.get(&locator).await.unwrap(),
                delivered.as_ref()
            );
        } else {
            let inline = locator
                .strip_prefix("inline-json:")
                .expect("fixed local error is retained inline");
            assert_eq!(inline.as_bytes(), delivered.as_ref());
        }
        upstream.verify().await;
    }
}
