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
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, body)
}

fn string_array(value: &Value, field: &str) -> Vec<String> {
    value[field]
        .as_array()
        .unwrap_or_else(|| panic!("{field} must be an array"))
        .iter()
        .map(|item| {
            item.as_str()
                .unwrap_or_else(|| panic!("{field} items must be strings"))
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn group_and_routing_http_contract_is_tenant_scoped_cas_safe_and_enriched() {
    let directory = tempfile::tempdir().expect("routing HTTP contract directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("routing-http.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .expect("routing HTTP state");
    let pepper = state.config.key_pepper.as_bytes();
    let account_a = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "routing-http-a".to_owned(),
                name: "provider-a".to_owned(),
                driver: "http-json".to_owned(),
                config: json!({
                    "base_url": "http://127.0.0.1:18081",
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
        .expect("tenant A upstream");
    let account_b = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "routing-http-b".to_owned(),
                name: "provider-b".to_owned(),
                driver: "http-json".to_owned(),
                config: json!({
                    "base_url": "http://127.0.0.1:18082",
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
        .expect("tenant B upstream");
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "routing-http-a".to_owned(),
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
        .expect("stable credential");
    let service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "routing-http-manager".to_owned(),
                scopes: vec![
                    "routes:read".to_owned(),
                    "routes:write".to_owned(),
                    "keys:read".to_owned(),
                    "keys:write".to_owned(),
                ],
                tenant_external_id: Some("routing-http-a".to_owned()),
            },
            pepper,
        )
        .await
        .expect("tenant-scoped service credential");

    let (status, provider_group) = request_json(
        &state,
        "POST",
        "/internal/v1/provider-groups",
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "name": "Primary providers"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let provider_group_id = provider_group["id"].as_str().expect("provider group ID");
    let provider_group_updated_at = provider_group["updated_at"]
        .as_i64()
        .expect("provider group version");

    let (status, _) = request_json(
        &state,
        "GET",
        "/internal/v1/provider-groups?tenant_external_id=routing-http-b",
        &service.token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = request_json(
        &state,
        "PUT",
        &format!("/internal/v1/provider-groups/{provider_group_id}"),
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "name": "stale rename",
            "expected_updated_at": provider_group_updated_at - 1
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _) = request_json(
        &state,
        "PUT",
        &format!("/internal/v1/provider-groups/{provider_group_id}/members"),
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "member_ids": [account_b.id],
            "expected_updated_at": provider_group_updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, provider_group) = request_json(
        &state,
        "PUT",
        &format!("/internal/v1/provider-groups/{provider_group_id}/members"),
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "member_ids": [account_a.id],
            "expected_updated_at": provider_group_updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        string_array(&provider_group, "member_ids"),
        [account_a.id.to_string()]
    );

    let (status, route) = request_json(
        &state,
        "POST",
        "/internal/v1/model-routes",
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "public_model": "public-codex",
            "upstream_account_ids": [account_a.id],
            "upstream_model": "unlisted-custom-codex",
            "protocol": "openai",
            "priority": 0,
            "route_group_names": ["Codex routes"],
            "granted_credential_ids": [issued.key_id],
            "custom_model_confirmed": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");
    let route_id = route["id"].as_str().expect("route ID");
    assert_eq!(route["custom_model_confirmed"], true);
    assert_eq!(
        string_array(&route, "upstream_account_ids"),
        [account_a.id.to_string()]
    );
    assert_eq!(
        string_array(&route, "granted_credential_ids"),
        [issued.key_id.to_string()]
    );
    assert_eq!(
        string_array(&route, "candidate_upstream_account_ids"),
        [account_a.id.to_string()]
    );
    let route_group_id = string_array(&route, "route_group_ids")
        .pop()
        .expect("created route group ID");

    let (status, routes) = request_json(
        &state,
        "GET",
        "/internal/v1/model-routes?tenant_external_id=routing-http-a",
        &service.token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed = routes
        .as_array()
        .and_then(|items| items.iter().find(|item| item["id"] == route_id))
        .expect("enriched route in list");
    for field in [
        "upstream_account_ids",
        "included_provider_group_ids",
        "excluded_provider_group_ids",
        "route_group_ids",
        "granted_credential_ids",
        "candidate_upstream_account_ids",
        "custom_model_confirmed",
        "grant_revision",
    ] {
        assert!(
            listed.get(field).is_some(),
            "enriched route is missing {field}"
        );
    }

    let (status, credential_routing) = request_json(
        &state,
        "GET",
        &format!(
            "/internal/v1/keys/{}/routing?tenant_external_id=routing-http-a",
            issued.key_id
        ),
        &service.token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(string_array(&credential_routing, "route_ids"), [route_id]);
    let initial_grant_revision = credential_routing["grant_revision"]
        .as_i64()
        .expect("credential direct-grant revision");

    let route_updated_at = route["updated_at"].as_i64().expect("route version");
    let route_grant_revision = route["grant_revision"]
        .as_i64()
        .expect("route direct-grant revision");
    let (status, _) = request_json(
        &state,
        "PUT",
        &format!("/internal/v1/model-routes/{route_id}/routing"),
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "upstream_account_ids": [account_a.id],
            "included_provider_group_ids": [],
            "excluded_provider_group_ids": [],
            "route_group_ids": [route_group_id],
            "route_group_names": [],
            "granted_credential_ids": [issued.key_id],
            "custom_model_confirmed": true,
            "expected_updated_at": route_updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, route_with_provider_group) = request_json(
        &state,
        "PUT",
        &format!("/internal/v1/model-routes/{route_id}/routing"),
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "upstream_account_ids": [account_a.id],
            "included_provider_group_ids": [provider_group_id],
            "excluded_provider_group_ids": [],
            "route_group_ids": [route_group_id],
            "route_group_names": [],
            "granted_credential_ids": [issued.key_id],
            "custom_model_confirmed": true,
            "expected_updated_at": route_updated_at,
            "expected_grant_revision": route_grant_revision
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{route_with_provider_group}");
    assert_eq!(
        string_array(&route_with_provider_group, "included_provider_group_ids"),
        [provider_group_id]
    );
    assert_eq!(
        route_with_provider_group["grant_revision"], route_grant_revision,
        "non-grant association edits must not bump the direct-grant revision"
    );

    let (status, route_without_reverse_grant) = request_json(
        &state,
        "PUT",
        &format!("/internal/v1/model-routes/{route_id}/routing"),
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "upstream_account_ids": [account_a.id],
            "included_provider_group_ids": [provider_group_id],
            "excluded_provider_group_ids": [],
            "route_group_ids": [route_group_id],
            "route_group_names": [],
            "granted_credential_ids": [],
            "custom_model_confirmed": true,
            "expected_updated_at": route_with_provider_group["updated_at"],
            "expected_grant_revision": route_with_provider_group["grant_revision"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{route_without_reverse_grant}");
    assert!(
        route_without_reverse_grant["grant_revision"]
            .as_i64()
            .expect("new route direct-grant revision")
            > route_grant_revision
    );

    let (status, _) = request_json(
        &state,
        "PUT",
        &format!("/internal/v1/model-routes/{route_id}/routing"),
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "upstream_account_ids": [account_a.id],
            "included_provider_group_ids": [provider_group_id],
            "excluded_provider_group_ids": [],
            "route_group_ids": [route_group_id],
            "route_group_names": [],
            "granted_credential_ids": [issued.key_id],
            "custom_model_confirmed": true,
            "expected_updated_at": route_without_reverse_grant["updated_at"],
            "expected_grant_revision": route_grant_revision
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, credential_routing) = request_json(
        &state,
        "GET",
        &format!(
            "/internal/v1/keys/{}/routing?tenant_external_id=routing-http-a",
            issued.key_id
        ),
        &service.token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        string_array(&credential_routing, "route_ids"),
        Vec::<String>::new()
    );
    let reverse_edit_grant_revision = credential_routing["grant_revision"]
        .as_i64()
        .expect("credential revision after reverse edit");
    assert!(reverse_edit_grant_revision > initial_grant_revision);

    let (status, _) = request_json(
        &state,
        "PUT",
        &format!("/internal/v1/keys/{}/routing", issued.key_id),
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "route_ids": [],
            "route_group_ids": [route_group_id],
            "expected_updated_at": credential_routing["updated_at"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, credential_routing) = request_json(
        &state,
        "PUT",
        &format!("/internal/v1/keys/{}/routing", issued.key_id),
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "route_ids": [],
            "route_group_ids": [route_group_id],
            "expected_grant_revision": reverse_edit_grant_revision
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{credential_routing}");
    let group_grant_revision = credential_routing["grant_revision"]
        .as_i64()
        .expect("new credential direct-grant revision");
    assert!(group_grant_revision > reverse_edit_grant_revision);
    assert_eq!(
        string_array(&credential_routing, "route_ids"),
        Vec::<String>::new()
    );
    assert_eq!(
        string_array(&credential_routing, "effective_route_ids"),
        [route_id]
    );

    let (status, _) = request_json(
        &state,
        "PUT",
        &format!("/internal/v1/keys/{}/routing", issued.key_id),
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "route_ids": [route_id],
            "route_group_ids": [],
            "expected_grant_revision": reverse_edit_grant_revision
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, route_groups) = request_json(
        &state,
        "GET",
        "/internal/v1/route-groups?tenant_external_id=routing-http-a",
        &service.token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let route_group = route_groups
        .as_array()
        .and_then(|groups| groups.iter().find(|group| group["id"] == route_group_id))
        .expect("route group created with the route");
    let (status, _) = request_json(
        &state,
        "PUT",
        &format!("/internal/v1/route-groups/{route_group_id}/members"),
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "member_ids": [],
            "expected_updated_at": route_group["updated_at"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, after_membership_change) = request_json(
        &state,
        "GET",
        &format!(
            "/internal/v1/keys/{}/routing?tenant_external_id=routing-http-a",
            issued.key_id
        ),
        &service.token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        after_membership_change["grant_revision"], group_grant_revision,
        "derived route-group expansion must not bump direct-grant revision"
    );
    assert_eq!(
        string_array(&after_membership_change, "effective_route_ids"),
        Vec::<String>::new()
    );

    let (status, credential_group) = request_json(
        &state,
        "POST",
        "/internal/v1/credential-groups",
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "name": "Review credentials"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = request_json(
        &state,
        "PUT",
        &format!(
            "/internal/v1/credential-groups/{}/members",
            credential_group["id"]
                .as_str()
                .expect("credential group ID")
        ),
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "member_ids": [issued.key_id],
            "expected_updated_at": credential_group["updated_at"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, after_classification) = request_json(
        &state,
        "GET",
        &format!(
            "/internal/v1/keys/{}/routing?tenant_external_id=routing-http-a",
            issued.key_id
        ),
        &service.token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after_classification, after_membership_change);

    let (status, _) = request_json(
        &state,
        "PUT",
        &format!("/internal/v1/keys/{}/routing", issued.key_id),
        &service.token,
        Some(json!({
            "tenant_external_id": "routing-http-a",
            "route_ids": [],
            "route_group_ids": [],
            "credential_group_ids": [credential_group["id"].clone()],
            "expected_grant_revision": group_grant_revision
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
