use super::*;

fn incomplete(reason: &str) -> Value {
    json!({"id":"resp-incomplete-fixture", "object":"response", "status":"incomplete",
        "error":null, "incomplete_details":{"reason":reason}, "output":[],
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
async fn buffered_incomplete_keeps_502_and_settles_only_valid_actual_usage_once() {
    for valid in [true, false] {
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
        if !valid {
            body["usage"]
                .as_object_mut()
                .unwrap()
                .remove("total_tokens");
        }
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&upstream)
            .await;
        let response = send_response_usage_request(&fixture,
            &json!({"model":fixture.model,"input":"bounded fixture request", "stream":false,"max_output_tokens":32}))
            .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        assert!(
            serde_json::from_slice::<Value>(&delivered)
                .unwrap()
                .get("error")
                .is_some()
        );
        wait_for_request_settlement(&fixture, 1).await;
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.status_code, Some(502));
        assert_eq!(
            row.usage_basis,
            Some(if valid {
                crate::model::RequestUsageBasis::ProviderReported
            } else {
                crate::model::RequestUsageBasis::NotObserved
            })
        );
        assert_eq!(
            (row.input_tokens, row.cached_input_tokens, row.output_tokens),
            if valid { (10, 6, 2) } else { (0, 0, 0) }
        );
        assert_eq!(
            row.cost.parse::<Decimal>().unwrap(),
            if valid {
                Decimal::new(20, 6)
            } else {
                Decimal::ZERO
            }
        );
        assert_eq!(
            row.error_code.as_deref(),
            Some(if valid {
                "upstream_incomplete_response"
            } else {
                "upstream_failed_response"
            })
        );
        assert_exactly_once_side_effects(&fixture, row.request_id, None).await;
        drain_completed_response_archive(&fixture).await;
        let refs = fixture
            .state
            .db
            .request_archive_refs(fixture.key_id, row.request_id)
            .await
            .unwrap();
        let locator = refs.response_object.unwrap();
        if valid {
            assert_eq!(locator, format!("gap://{}/response", row.request_id));
        } else {
            let inline = locator
                .strip_prefix("inline-json:")
                .expect("fixed local error is retained inline");
            assert_eq!(inline.as_bytes(), delivered.as_ref());
        }
        upstream.verify().await;
    }
}
