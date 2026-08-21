use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{CreateKeyInput, CreateServiceTokenInput, CreateUpstreamAccountInput},
    model::KeyPolicy,
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn request_json(
    state: &AppState,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&body).expect("serialize request body"))
        }
        None => Body::empty(),
    };
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request.body(body).expect("build request"))
        .await
        .expect("control response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("bounded response body");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON response body")
    };
    (status, body)
}

fn strings(value: &Value, field: &str) -> Vec<String> {
    value[field]
        .as_array()
        .unwrap_or_else(|| panic!("{field} must be an array"))
        .iter()
        .map(|item| {
            item.as_str()
                .unwrap_or_else(|| panic!("{field} item must be a string"))
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn model_route_list_batches_enrichment_and_keeps_tenants_isolated() {
    let directory = tempfile::tempdir().expect("routing list directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("routing-list.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .expect("routing list state");
    let pepper = state.config.key_pepper.as_bytes();

    let account_a = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "list-tenant-a".to_owned(),
                name: "account-a".to_owned(),
                driver: "http-json".to_owned(),
                config: json!({
                    "base_url": "http://127.0.0.1:18101",
                    "network_scope": "private"
                }),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            pepper,
        )
        .await
        .expect("tenant A account");
    let account_b = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "list-tenant-b".to_owned(),
                name: "account-b".to_owned(),
                driver: "http-json".to_owned(),
                config: json!({
                    "base_url": "http://127.0.0.1:18102",
                    "network_scope": "private"
                }),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            pepper,
        )
        .await
        .expect("tenant B account");
    let key_a = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "list-tenant-a".to_owned(),
                principal_external_id: "member-a".to_owned(),
                alias: "member-a".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .expect("tenant A credential");
    let service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "routing-list-manager".to_owned(),
                scopes: vec!["routes:read".to_owned(), "routes:write".to_owned()],
                tenant_external_id: None,
            },
            pepper,
        )
        .await
        .expect("global service credential");

    let (status, included_group) = request_json(
        &state,
        "POST",
        "/internal/v1/provider-groups",
        &service.token,
        Some(json!({"tenant_external_id": "list-tenant-a", "name": "included"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{included_group}");
    let included_group_id = included_group["id"].as_str().expect("included group id");
    let (status, body) = request_json(
        &state,
        "PUT",
        &format!("/internal/v1/provider-groups/{included_group_id}/members"),
        &service.token,
        Some(json!({
            "tenant_external_id": "list-tenant-a",
            "member_ids": [account_a.id],
            "expected_updated_at": included_group["updated_at"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, excluded_group) = request_json(
        &state,
        "POST",
        "/internal/v1/provider-groups",
        &service.token,
        Some(json!({"tenant_external_id": "list-tenant-a", "name": "excluded"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{excluded_group}");
    let excluded_group_id = excluded_group["id"].as_str().expect("excluded group id");

    let (status, route_a) = request_json(
        &state,
        "POST",
        "/internal/v1/model-routes",
        &service.token,
        Some(json!({
            "tenant_external_id": "list-tenant-a",
            "public_model": "public-a",
            "upstream_account_ids": [account_a.id],
            "included_provider_group_ids": [included_group_id],
            "excluded_provider_group_ids": [excluded_group_id],
            "route_group_names": ["route-group-a"],
            "granted_credential_ids": [key_a.key_id],
            "upstream_model": "custom-a",
            "protocol": "openai",
            "priority": 0,
            "custom_model_confirmed": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{route_a}");

    let (status, route_b) = request_json(
        &state,
        "POST",
        "/internal/v1/model-routes",
        &service.token,
        Some(json!({
            "tenant_external_id": "list-tenant-b",
            "public_model": "public-b",
            "upstream_account_ids": [account_b.id],
            "upstream_model": "custom-b",
            "protocol": "openai",
            "priority": 0,
            "custom_model_confirmed": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{route_b}");

    let (status, listed) = request_json(
        &state,
        "GET",
        "/internal/v1/model-routes?tenant_external_id=list-tenant-a&limit=100",
        &service.token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let listed = listed.as_array().expect("route list");
    assert_eq!(listed.len(), 1, "tenant filter must scope the base page");
    let listed = &listed[0];
    assert_eq!(listed["id"], route_a["id"]);
    assert_ne!(listed["id"], route_b["id"]);
    assert_eq!(
        strings(listed, "upstream_account_ids"),
        [account_a.id.to_string()]
    );
    assert_eq!(
        strings(listed, "included_provider_group_ids"),
        [included_group_id]
    );
    assert_eq!(
        strings(listed, "excluded_provider_group_ids"),
        [excluded_group_id]
    );
    assert_eq!(
        strings(listed, "granted_credential_ids"),
        [key_a.key_id.to_string()]
    );
    assert_eq!(
        strings(listed, "candidate_upstream_account_ids"),
        [account_a.id.to_string()]
    );
    assert_eq!(listed["custom_model_confirmed"], true);
    assert!(listed["grant_revision"].is_i64());
    assert_eq!(strings(listed, "route_group_ids").len(), 1);
    assert!(listed.get("credential_group_ids").is_none());
    assert!(!listed.to_string().contains(&account_b.id.to_string()));
}
