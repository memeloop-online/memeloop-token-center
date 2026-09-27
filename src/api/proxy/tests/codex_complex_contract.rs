use super::*;

/// Synthetic input only: never replay archived conversations or real ciphertext
/// into a mock fixture. Test the actual native wire adapter, not a pong shortcut.
fn complex_request(model: &str) -> Value {
    json!({
        "model": model,
        "stream": false,
        "instructions": "Use the available function to answer.",
        "client_metadata": {"client_version": "synthetic"},
        "include": ["message.output_text.logprobs", "reasoning.encrypted_content"],
        "tool_choice": {"type": "function", "name": "lookup"},
        "tools": [{
            "type": "function", "name": "lookup", "description": "Lookup a synthetic value",
            "parameters": {"type": "object", "properties": {"key": {"type": "string"}}}
        }],
        "input": [
            {"role": "developer", "content": [{"type": "input_text", "text": "Be concise."}]},
            {"role": "user", "content": [{"type": "input_text", "text": "Look up A."}]},
            {"type": "reasoning", "id": "rs_synthetic", "summary": [],
                "encrypted_content": "synthetic-ciphertext-not-a-credential"},
            {"type": "function_call", "call_id": "call_synthetic", "name": "lookup",
                "arguments": "{\"key\":\"A\"}"},
            {"type": "function_call_output", "call_id": "call_synthetic", "output": "{\"value\":7}"},
            {"type": "compaction", "encrypted_content": "synthetic-compaction"}
        ]
    })
}

#[tokio::test]
async fn native_codex_complex_contract_preserves_wire_and_does_not_replay_ordinary_400() {
    for (label, rejection) in [
        ("complex-ok", None),
        (
            "complex-metadata-400",
            Some(("client_metadata", "unsupported_parameter")),
        ),
        (
            "complex-encrypted-400",
            Some(("input[2].encrypted_content", "invalid_encrypted_content")),
        ),
    ] {
        let rejected = rejection.is_some();
        let fixture = codex_route_fixture(label).await;
        let upstream = MockServer::start().await;
        let reply = if let Some((param, code)) = rejection {
            ResponseTemplate::new(400).set_body_json(json!({
                "error": {
                    "type": "invalid_request_error",
                    "code": code,
                    "param": param,
                    "message": "private-upstream-diagnostic-canary"
                }
            }))
        } else {
            ResponseTemplate::new(200)
                .set_body_raw(completed_codex_sse("synthetic answer"), "text/event-stream")
        };
        Mock::given(method("POST"))
            .and(path(codex_transport::RESPONSES_PATH))
            .and(header_matcher("chatgpt-account-id", "account-123"))
            .respond_with(reply)
            .expect(1)
            .mount(&upstream)
            .await;
        add_codex_standby_route(&fixture, &format!("codex-route-{label}"), "account-456").await;
        Mock::given(method("POST"))
            .and(header_matcher("chatgpt-account-id", "account-456"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&upstream)
            .await;
        let original = complex_request(&fixture.model);
        let response =
            send_codex_route(&fixture, &upstream, "/v1/responses", original.clone()).await;
        let expected_status = if rejected {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::OK
        };
        assert_eq!(response.status(), expected_status);
        let returned = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&returned).contains("private-upstream-diagnostic-canary"));
        wait_for_request_settlement(&fixture, 1).await;
        let records = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].status_code,
            Some(i64::from(expected_status.as_u16()))
        );
        assert_exactly_once_side_effects(
            &fixture,
            records[0].request_id,
            (!rejected).then_some("resp-codex"),
        )
        .await;
        assert_response_archives_omit(&fixture, "private-upstream-diagnostic-canary").await;
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let wire: Value = serde_json::from_slice(&requests[0].body).unwrap();
        for field in [
            "instructions",
            "client_metadata",
            "include",
            "tool_choice",
            "input",
        ] {
            assert_eq!(wire[field], original[field], "{label}: {field}");
        }
        assert_eq!(wire["tools"][0], original["tools"][0]);
        assert_eq!(wire["model"], fixture.upstream_model);
        assert_eq!(wire["stream"], true);
        assert_eq!(wire["store"], false);
        upstream.verify().await;
    }
}
