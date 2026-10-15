use super::*;
use crate::{
    config::RuntimeRole,
    db::{CreateKeyInput, CreateServiceTokenInput},
    model::KeyPolicy,
};
use tower::ServiceExt;

fn inference(token: &str, key_id: Uuid) -> Request {
    Request::post("/internal/v1/inference/chat/completions")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"key_id":key_id,"request":{"model":"fixture","messages":[{"role":"user","content":"hello"}],"stream":false}}).to_string()))
        .unwrap()
}

#[tokio::test]
async fn inference_requires_explicit_key_copy_permission_and_tenant() {
    let (state, _directory) = crate::api::tests::test_state().await;
    let key = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "inference-owner".into(),
                principal_external_id: "owner".into(),
                alias: "billing".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    state
        .db
        .create_tenant("inference-other", None)
        .await
        .unwrap();
    for (name, scopes, tenant) in [
        ("reader", vec!["keys:read".to_owned()], "inference-owner"),
        (
            "other-writer",
            vec!["keys:write".to_owned()],
            "inference-other",
        ),
    ] {
        let service = state
            .db
            .create_service_token(
                CreateServiceTokenInput {
                    name: name.into(),
                    scopes,
                    tenant_external_id: Some(tenant.into()),
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let response = router_for_role(state.clone(), RuntimeRole::Control)
            .oneshot(inference(&service.token, key.key_id))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{name}");
        let bytes = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(&key.key));
    }
    let writer = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "owner-writer".into(),
                scopes: vec!["keys:write".into()],
                tenant_external_id: Some("inference-owner".into()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    // Copy authority does not add any downstream model grants.
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(inference(&writer.token, key.key_id))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // An active tenant query cannot override a scoped service identity.
    let mut request = inference(&writer.token, key.key_id);
    *request.uri_mut() =
        "/internal/v1/inference/chat/completions?tenant_external_id=inference-other"
            .parse()
            .unwrap();
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // Global authority is narrowed by the UI's explicit active tenant too.
    let mut request = inference(&state.config.service_token, key.key_id);
    *request.uri_mut() =
        "/internal/v1/inference/chat/completions?tenant_external_id=inference-other"
            .parse()
            .unwrap();
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // Invalid requests are rejected before a nonexistent key is copied.
    let mut request = inference(&writer.token, Uuid::now_v7());
    *request.body_mut() =
        Body::from(json!({"key_id":Uuid::now_v7(),"request":{"stream":true}}).to_string());
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    state
        .db
        .set_key_status(key.key_id, "suspended")
        .await
        .unwrap();
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(inference(&writer.token, key.key_id))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // A downstream key is never accepted as operator authorization.
    let response = router_for_role(state, RuntimeRole::Control)
        .oneshot(inference(&key.key, key.key_id))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn operator_bridge_requires_nonstreaming_json() {
    assert!(validate_request(&json!({"stream":true})).is_err());
    assert!(validate_request(&json!({"stream":"false"})).is_err());
    assert!(validate_request(&json!([])).is_err());
    assert!(validate_request(&json!({"stream":false})).is_ok());
    assert!(validate_request(&json!({})).is_ok());
}

#[tokio::test]
async fn control_inference_returns_tool_calls_and_settles_selected_billing_key() {
    use crate::{
        db::{CreateModelRouteInput, CreateUpstreamAccountInput},
        provider::UpstreamCredential,
    };
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header as header_matcher, method, path},
    };
    let upstream = MockServer::start().await;
    let completion = json!({
        "id":"chatcmpl-operator-fixture", "object":"chat.completion", "created":1, "model":"fixture",
        "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{
            "id":"call-fixture", "type":"function", "function":{"name":"inspect","arguments":"{}"}
        }]},"finish_reason":"tool_calls"}],
        "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header_matcher("authorization", "Bearer provider-fixture"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&completion))
        .expect(1)
        .mount(&upstream)
        .await;
    let (state, _directory) = crate::api::tests::test_state().await;
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "inference-success".into(),
                name: "inference-upstream".into(),
                driver: "http-json".into(),
                config: json!({"base_url":upstream.uri(),"network_scope":"public"}),
                credential: UpstreamCredential::ApiKey {
                    value: "provider-fixture".into(),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                },
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: "inference-success".into(),
            public_model: "fixture".into(),
            upstream_account_id: account.id,
            upstream_model: "fixture".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    let key = state
        .db
        .create_key_with_routing(
            CreateKeyInput {
                tenant_external_id: "inference-success".into(),
                principal_external_id: "billing-owner".into(),
                alias: "selected-billing-key".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            &[route.id],
            &[],
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    state
        .db
        .upsert_model_price("fixture", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "inference-writer".into(),
                scopes: vec!["keys:write".into()],
                tenant_external_id: Some("inference-success".into()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(inference(&service.token, key.key_id))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let bytes = axum::body::to_bytes(response.into_body(), 65_536)
        .await
        .unwrap();
    let actual: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(actual["choices"], completion["choices"]);
    assert_eq!(actual["usage"], completion["usage"]);
    let output = String::from_utf8_lossy(&bytes);
    for secret in [&key.key, &service.token] {
        assert!(!output.contains(secret));
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let rows = state.db.list_requests(key.key_id, 10).await.unwrap();
            if rows.len() == 1 && rows[0].status_code.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let rows = state.db.list_requests(key.key_id, 10).await.unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 2));
    let settlements = state
        .db
        .list_account_settlements(key.account_id, 10, None, None)
        .await
        .unwrap();
    assert_eq!(settlements.items.len(), 1);
    assert_eq!(settlements.items[0].key_id, key.key_id);
    assert_eq!(
        Decimal::from_str(&settlements.items[0].cost).unwrap(),
        Decimal::new(5, 6)
    );
    let forwarded = upstream.received_requests().await.unwrap();
    assert_eq!(forwarded.len(), 1);
    for value in forwarded[0].headers.values() {
        assert!(!value.to_str().unwrap_or_default().contains(&key.key));
        assert!(!value.to_str().unwrap_or_default().contains(&service.token));
    }
    assert!(!String::from_utf8_lossy(&forwarded[0].body).contains(&key.key));
    assert!(!String::from_utf8_lossy(&forwarded[0].body).contains(&service.token));
}
