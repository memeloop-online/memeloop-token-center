use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{CreateServiceTokenInput, CreateUpstreamAccountInput},
    provider::UpstreamCredential,
};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn request(
    state: &AppState,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if !token.is_empty() {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
    )
}

#[tokio::test]
async fn proxy_group_access_reports_only_the_authenticated_provider_reader_capability() {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::initialize(Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("access.db").display()
    )))
    .await
    .unwrap();
    state.db.create_tenant("access-tenant", None).await.unwrap();
    let path = "/internal/v1/transport-proxy-groups/access";
    for (name, tenant, scopes, can_manage) in [
        (
            "tenant-reader",
            Some("access-tenant"),
            vec!["providers:read"],
            false,
        ),
        (
            "tenant-writer",
            Some("access-tenant"),
            vec!["providers:read", "providers:write"],
            false,
        ),
        ("global-reader", None, vec!["providers:read"], false),
        (
            "global-writer",
            None,
            vec!["providers:read", "providers:write"],
            true,
        ),
    ] {
        let credential = state
            .db
            .create_service_token(
                CreateServiceTokenInput {
                    name: name.into(),
                    scopes: scopes.into_iter().map(str::to_owned).collect(),
                    tenant_external_id: tenant.map(str::to_owned),
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let (status, body) = request(&state, "GET", path, &credential.token, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{name}");
        assert_eq!(body, json!({"can_manage": can_manage}), "{name}");
        assert_eq!(
            request(
                &state,
                "GET",
                "/internal/v1/transport-proxy-groups?tenant_external_id=access-tenant",
                &credential.token,
                Value::Null,
            )
            .await
            .0,
            if can_manage {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            },
            "{name}",
        );
    }
    for token in ["", "invalid"] {
        assert_eq!(
            request(&state, "GET", path, token, Value::Null).await.0,
            StatusCode::UNAUTHORIZED,
        );
    }
}

#[tokio::test]
async fn operator_api_enforces_authority_cas_redaction_and_explicit_unbind() {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::initialize(Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("api.db").display()
    )))
    .await
    .unwrap();
    let key = state.config.key_pepper.as_bytes();
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "managed-api".into(),
                name: "Codex".into(),
                driver: "openai-codex".into(),
                config: json!({"base_url":"https://chatgpt.com/backend-api/codex"}),
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
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            key,
        )
        .await
        .unwrap();
    let operator = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "operator".into(),
                scopes: vec!["providers:write".into()],
                tenant_external_id: None,
            },
            key,
        )
        .await
        .unwrap();
    let scoped = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "scoped".into(),
                scopes: vec!["providers:write".into()],
                tenant_external_id: Some("managed-api".into()),
            },
            key,
        )
        .await
        .unwrap();
    let collection = "/internal/v1/transport-proxy-groups";
    let input = json!({"tenant_external_id":"managed-api","name":"egress","members":[{"label":"primary","proxy_url":"socks5h://fixture-user:fixture-password@10.20.30.40:1080"}]});
    assert_eq!(
        request(&state, "POST", collection, &scoped.token, input.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&state, "POST", collection, "invalid", input.clone())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let mut unknown = input.clone();
    unknown["unknown"] = json!(true);
    let invalid = request(&state, "POST", collection, &operator.token, unknown).await;
    assert_eq!(invalid.0, StatusCode::BAD_REQUEST);
    assert_eq!(invalid.1["error"]["code"], "invalid_request");
    let (status, group) = request(&state, "POST", collection, &operator.token, input).await;
    assert_eq!(status, StatusCode::CREATED, "{group}");
    assert_eq!(group["members"][0]["has_auth"], true);
    for secret in ["fixture-user", "fixture-password", "10.20.30.40"] {
        assert!(!group.to_string().contains(secret));
    }
    let path = format!("{collection}/{}", group["id"].as_str().unwrap());
    assert_eq!(
        request(
            &state,
            "GET",
            &format!("{path}?tenant_external_id=other"),
            &operator.token,
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let binding_path = format!(
        "/internal/v1/upstreams/{}/transport-proxy-group",
        account.id
    );
    let bind = json!({"tenant_external_id":"managed-api","group_id":group["id"],"expected_group_version":1,"initial_member_id":group["members"][0]["id"],"expected_binding_version":0,"expected_credential_generation":account.credential_generation,"expected_updated_at":account.updated_at});
    let (status, binding) =
        request(&state, "PUT", &binding_path, &operator.token, bind.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{binding}");
    assert_eq!(binding["runtime"]["configuration_state"], "pending");
    let stale = request(&state, "PUT", &binding_path, &operator.token, bind).await;
    assert_eq!(stale.0, StatusCode::CONFLICT);
    assert_eq!(stale.1["error"]["code"], "proxy_group_binding_conflict");
    let remove = json!({"tenant_external_id":"managed-api","expected_version":1});
    assert_eq!(
        request(&state, "DELETE", &path, &operator.token, remove.clone())
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut unbind = json!({"tenant_external_id":"managed-api","expected_group_version":1,"expected_binding_version":1,"expected_credential_generation":binding["credential_generation"],"expected_updated_at":binding["updated_at"]});
    assert_eq!(
        request(
            &state,
            "DELETE",
            &binding_path,
            &operator.token,
            unbind.clone()
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    unbind["single_proxy_member_id"] = group["members"][0]["id"].clone();
    let (status, unbound) = request(&state, "DELETE", &binding_path, &operator.token, unbind).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{unbound}");
    assert_eq!(unbound["binding_version"], 2);
    assert!(unbound["group_id"].is_null());
    assert_eq!(
        request(&state, "DELETE", &path, &operator.token, remove)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
}
