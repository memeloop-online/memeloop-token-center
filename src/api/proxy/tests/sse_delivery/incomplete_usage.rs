use super::*;

#[tokio::test]
async fn provider_incomplete_settles_reported_usage_through_the_codex_pipeline() {
    for reason in [
        "max_output_tokens",
        "content_filter",
        "future_provider_reason",
    ] {
        let fixture = codex_route_fixture(&format!("incomplete-usage-{reason}")).await;
        let upstream = MockServer::start().await;
        let mut terminal = completed_response_with_usage(3, 7);
        terminal["status"] = json!("incomplete");
        terminal["incomplete_details"] = json!({"reason":reason});
        let sse = format!(
            "event: response.incomplete\ndata: {{\"type\":\"response.incomplete\",\"response\":{terminal}}}\n\ndata: [DONE]\n\n"
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;
        let response = send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model":fixture.model,"input":"test","stream":true}),
        )
        .await;
        let status = response.status();
        let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(text.matches("event: response.incomplete").count(), 1);
        assert!(!text.contains("event: error"));
        assert!(text.contains(reason));
        wait_for_request_settlement(&fixture, 1).await;
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(
            rows[0].error_code.as_deref(),
            Some("upstream_incomplete_response")
        );
        assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 7));
        assert_eq!(
            rows[0].usage_basis,
            Some(crate::model::RequestUsageBasis::ProviderReported)
        );
        assert_ne!(rows[0].cost, "0");
        assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
        upstream.verify().await;
    }
}

#[tokio::test]
async fn incomplete_error_invalid_usage_and_out_of_budget_usage_remain_conservative() {
    for defect in ["error", "inconsistent", "over_budget"] {
        let fixture = codex_route_fixture(&format!("incomplete-untrusted-{defect}")).await;
        let upstream = MockServer::start().await;
        let mut terminal = completed_response_with_usage(3, 7);
        terminal["status"] = json!("incomplete");
        terminal["incomplete_details"] = json!({"reason":"max_output_tokens"});
        match defect {
            "error" => terminal["error"] = json!({"message":"PRIVATE_PROVIDER_ERROR"}),
            "inconsistent" => terminal["usage"]["total_tokens"] = json!(999),
            _ => {
                // Codex uses the fixture's trusted reservation bound of 64,
                // not a client-supplied output limit (which it rejects).
                terminal["usage"]["output_tokens"] = json!(65);
                terminal["usage"]["total_tokens"] = json!(68);
            }
        }
        let sse = format!(
            concat!(
                "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-usage-contract\"}}}}\n\n",
                "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}}\n\n",
                "event: response.incomplete\ndata: {{\"type\":\"response.incomplete\",\"response\":{terminal}}}\n\n"
            ),
            terminal = terminal
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;
        let response = send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model":fixture.model,"input":"test","stream":true}),
        )
        .await;
        let status = response.status();
        let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert!(!String::from_utf8_lossy(&body).contains("PRIVATE_PROVIDER_ERROR"));
        wait_for_request_settlement(&fixture, 1).await;
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(
            rows[0].usage_basis,
            Some(crate::model::RequestUsageBasis::ContractCeiling),
            "{defect}"
        );
        assert_eq!(rows[0].output_tokens, 64, "{defect}");
        assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
        upstream.verify().await;
    }
}
