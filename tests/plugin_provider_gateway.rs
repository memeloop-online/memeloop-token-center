use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{
        CreateKeyInput, CreateModelRouteInput, CreateUpstreamAccountInput,
        ReplaceCredentialRoutingInput, StatsFilter, unix_millis,
    },
    model::KeyPolicy,
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, header as matches_header, method, path},
};

#[tokio::test]
async fn plugin_provider_onboards_through_the_real_management_api() {
    let mock = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("plugin-onboarding.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some("examples/plugins".to_owned());
    let state = AppState::initialize(config).await.unwrap();
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::post("/internal/v1/upstreams")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", state.config.service_token),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "tenant_external_id": "plugin-onboarding",
                        "name": "plugin-api-primary",
                        "driver": "example-oauth-http",
                        "config": {
                            "base_url": mock.uri(),
                            "network_scope": "public"
                        },
                        "credential": {
                            "type": "api_key",
                            "value": "raw-provider-secret"
                        }
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let account: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::CREATED, "{account}");
    assert_eq!(account["driver"], "example-oauth-http");
    assert_eq!(account["connection_method"], "api_key");
    assert_eq!(account["can_rotate"], true);
    assert_eq!(account["can_refresh"], false);
    assert!(!account.to_string().contains("raw-provider-secret"));

    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::post("/internal/v1/model-routes")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", state.config.service_token),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "tenant_external_id": "plugin-onboarding",
                        "public_model": "plugin-public-model",
                        "upstream_account_id": account["id"],
                        "upstream_model": "plugin-private-model",
                        "protocol": "openai",
                        "priority": 0,
                        "custom_model_confirmed": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn real_component_provider_normalizes_non_openai_upstream_and_core_owns_secrets_billing_and_archive()
 {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/vendor/infer"))
        .and(matches_header("x-plugin-shape", "buffered-v1"))
        .and(body_json(json!({
            "prompt": "from-component",
            "model": "vendor-model"
        })))
        .respond_with(ResponseTemplate::new(207).set_body_json(json!({
            "vendor_answer": "non-openai-shape"
        })))
        .expect(2)
        .mount(&mock)
        .await;

    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("component-provider.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some("examples/plugins".to_owned());
    let state = AppState::initialize(config).await.unwrap();
    let pepper = state.config.key_pepper.as_bytes();
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "component-provider-tenant".into(),
                principal_external_id: "component-provider-user".into(),
                alias: "stable-component-key".into(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    allowed_models: vec!["requested-model".into(), "example-rewritten".into()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    state
        .db
        .upsert_model_price("example-rewritten", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "component-provider-tenant".into(),
                name: "same-account-api-or-oauth".into(),
                driver: "example-oauth-http".into(),
                config: json!({"base_url": mock.uri(), "network_scope": "public"}),
                credential: UpstreamCredential::ApiKey {
                    value: "component-api-secret".into(),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                },
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let requested_route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: "component-provider-tenant".into(),
            public_model: "requested-model".into(),
            upstream_account_id: account.id,
            upstream_model: "vendor-route-model".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    let rewritten_route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: "component-provider-tenant".into(),
            public_model: "example-rewritten".into(),
            upstream_account_id: account.id,
            upstream_model: "vendor-route-model".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    let current_routing = state
        .db
        .credential_routing(issued.key_id, "component-provider-tenant")
        .await
        .unwrap();
    let mut route_ids = current_routing.route_ids;
    route_ids.extend([requested_route.id, rewritten_route.id]);
    state
        .db
        .replace_credential_routing(
            issued.key_id,
            ReplaceCredentialRoutingInput {
                tenant_external_id: "component-provider-tenant".into(),
                route_ids,
                route_group_ids: current_routing.route_group_ids,
                expected_grant_revision: current_routing.grant_revision,
            },
        )
        .await
        .unwrap();

    let first = call_component_provider(&state, &issued.key).await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    assert_eq!(
        first.1["choices"][0]["message"]["content"],
        "normalized by component"
    );

    let rotated = state
        .db
        .rotate_upstream_credential(
            account.id,
            UpstreamCredential::ApiKey {
                value: "component-api-secret-rotated".into(),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
            },
            "component-provider-api-rotation",
            pepper,
        )
        .await
        .unwrap();
    assert_eq!(rotated.id, account.id);
    assert_eq!(rotated.connection_method, "api_key");

    let second = call_component_provider(&state, &issued.key).await;
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(second.1["usage"]["prompt_tokens"], 7);

    let received = mock.received_requests().await.unwrap();
    let credentials = received
        .iter()
        .map(|request| {
            request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        credentials,
        [
            "Bearer component-api-secret",
            "Bearer component-api-secret-rotated"
        ]
    );
    for request in &received {
        let body = String::from_utf8_lossy(&request.body);
        assert!(!body.contains("component-api-secret"));
        assert!(!body.contains("component-api-secret-rotated"));
    }

    let key = state
        .db
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
    let requests = state.db.list_requests(key.key_id, 10).await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| {
        request.model == "example-rewritten"
            && request.status_code == Some(200)
            && request.input_tokens == 7
            && request.output_tokens == 3
            && request.cost != "0"
    }));
    let exact_account = state
        .db
        .stats_filtered(
            key.key_id,
            StatsFilter {
                from_created_at: Some(unix_millis().saturating_sub(60_000)),
                to_created_at: Some(unix_millis().saturating_add(1)),
                upstream_account_id: Some(account.id),
                ..StatsFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(exact_account.summary.total_requests, 2);

    for request in requests {
        let refs = state
            .db
            .request_archive_refs(key.key_id, request.request_id)
            .await
            .unwrap();
        let archived_request = state
            .archive
            .get_bounded(&refs.request_object, 1024 * 1024)
            .await
            .unwrap();
        let archived_response = state
            .archive
            .get_bounded(refs.response_object.as_deref().unwrap(), 1024 * 1024)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&archived_request).contains("requested-model"));
        assert_eq!(
            serde_json::from_slice::<Value>(&archived_response).unwrap()["choices"][0]["message"]["content"],
            "normalized by component"
        );
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&archived_request),
            String::from_utf8_lossy(&archived_response)
        );
        assert!(!combined.contains("component-api-secret"));
        assert!(!combined.contains("component-api-secret-rotated"));
    }
}

async fn call_component_provider(state: &AppState, key: &str) -> (StatusCode, Value) {
    let response = api::router_for_role(state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/chat/completions")
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "model": "requested-model",
                        "messages": [{"role": "user", "content": "canonical"}],
                        "stream": false,
                        "max_tokens": 32
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}
