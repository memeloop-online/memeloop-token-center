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
