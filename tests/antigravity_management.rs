use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{CreateServiceTokenInput, CreateUpstreamAccountInput},
    provider::UpstreamCredential,
};
use serde_json::json;
use tower::ServiceExt;
use wiremock::MockServer;

#[tokio::test]
async fn operator_can_configure_bounded_quota_reads_through_the_loaded_plugin_schema() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("quota-policy.db").display()
    ));
    config.plugin_dir = Some("plugins".into());
    let state = AppState::initialize(config).await.unwrap();
    let supplier = MockServer::start().await;
    let original = json!({"base_url":supplier.uri(), "control_url":supplier.uri(), "network_scope":"public", "project_id":"fixture-project"});
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "quota-policy".into(),
                name: "fixture".into(),
                driver: "google-antigravity".into(),
                config: original.clone(),
                credential: UpstreamCredential::OAuth {
                    access_token: "fixture-access".into(),
                    refresh_token: None,
                    expires_at: None,
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: None,
                    proxy_url: None,
                    proxy_network_scope: None,
                },
                oauth_session_id: None,
                oauth_driver: Some("generic_authorization_code".into()),
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let mut updated_at = account.updated_at;
    for (attempts, expected) in [(4, StatusCode::OK), (5, StatusCode::BAD_REQUEST)] {
        let mut updated = original.clone();
        updated["quota_read_policy"] = json!({"max_attempts":attempts, "initial_delay_millis":250, "total_timeout_millis":25000});
        let response = api::router_for_role(state.clone(), RuntimeRole::Control).oneshot(
            Request::put(format!("/internal/v1/upstreams/{}", account.id))
                .header("authorization", format!("Bearer {}", state.config.service_token))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&json!({
                    "tenant_external_id":"quota-policy", "name":"fixture", "expected_updated_at":updated_at, "config":updated
                })).unwrap())).unwrap()
        ).await.unwrap();
        assert_eq!(response.status(), expected);
        let current = state
            .db
            .upstream_account_for_reauthorization(account.id, "quota-policy")
            .await
            .unwrap();
        assert_eq!(current.config["quota_read_policy"]["max_attempts"], 4);
        updated_at = current.updated_at;
    }
    // Account updates may trigger the existing background model-catalog sync;
    // all configured destinations remain on this local mock, never a supplier.
}

#[tokio::test]
async fn tenant_cannot_move_operator_oauth_or_hidden_headers_to_another_origin() {
    assert_no_tenant_rebind("google-antigravity").await;
    assert_no_tenant_rebind("http-json").await;
}

async fn assert_no_tenant_rebind(driver: &str) {
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("native-management.db").display()
    ));
    config.plugin_dir = Some("plugins".into());
    let state = AppState::initialize(config).await.unwrap();
    let original = MockServer::start().await;
    let other = MockServer::start().await;
    let provider_config = if driver == "google-antigravity" {
        json!({"base_url": original.uri(), "network_scope": "public", "project_id": "fixture-project", "request_headers": {"x-private-header": "fixture-private-header"}})
    } else {
        json!({"base_url": original.uri(), "network_scope": "public"})
    };
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "fixture-tenant".into(),
                name: "fixture-native".into(),
                driver: driver.into(),
                config: provider_config.clone(),
                credential: UpstreamCredential::OAuth {
                    access_token: "fixture-access".into(),
                    refresh_token: None,
                    expires_at: None,
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: None,
                    proxy_url: None,
                    proxy_network_scope: None,
                },
                oauth_session_id: None,
                oauth_driver: Some("generic_authorization_code".into()),
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let tenant = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "fixture-tenant-writer".into(),
                scopes: vec!["providers:write".into()],
                tenant_external_id: Some("fixture-tenant".into()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let mut cases = vec![(tenant.token.as_str(), StatusCode::FORBIDDEN)];
    if driver == "google-antigravity" {
        cases.push((state.config.service_token.as_str(), StatusCode::BAD_REQUEST));
    }
    for (token, status) in cases {
        let response = api::router_for_role(state.clone(), RuntimeRole::Control).oneshot(Request::put(format!("/internal/v1/upstreams/{}", account.id))
            .header("authorization", format!("Bearer {token}")).header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&json!({"tenant_external_id": "fixture-tenant", "name": "fixture-update", "expected_updated_at": account.updated_at, "config": {"base_url": other.uri(), "network_scope": "public", "project_id": "fixture-project"}})).unwrap())).unwrap()).await.unwrap();
        assert_eq!(response.status(), status);
        let current = state
            .db
            .upstream_account_for_reauthorization(account.id, "fixture-tenant")
            .await
            .unwrap();
        assert_eq!(current.config, provider_config);
        assert!(other.received_requests().await.unwrap().is_empty());
    }
}
