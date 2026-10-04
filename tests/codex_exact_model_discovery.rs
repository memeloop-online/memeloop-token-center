use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{CreateServiceTokenInput, CreateUpstreamAccountInput, unix_millis},
    provider::UpstreamCredential,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header as matches_header, method, path, query_param},
};

const SELECTED: &str = "gpt-6.1-sol";

async fn fixture() -> (AppState, tempfile::TempDir, MockServer, Uuid) {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::initialize(Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("exact.db").display()
    )))
    .await
    .unwrap();
    let server = MockServer::start().await;
    let account = state.db.create_upstream_account(CreateUpstreamAccountInput {
        tenant_external_id: "exact-tenant".into(),
        name: "exact-codex".into(),
        driver: "openai-codex".into(),
        config: json!({"base_url": server.uri(), "network_scope": "public", "output_token_limits": {}}),
        credential: UpstreamCredential::OAuth {
            access_token: "private-access-canary".into(),
            refresh_token: Some("private-refresh-canary".into()),
            expires_at: Some(unix_millis() + 600_000),
            header: "authorization".into(), prefix: "Bearer ".into(),
            adapter_state: Some(json!({"schema":"openai-codex-oauth-v1", "account_id":"private-account-canary"})),
            proxy_url: None, proxy_network_scope: None,
        },
        oauth_session_id: Some(Uuid::now_v7()),
        oauth_driver: Some("openai_codex_device".into()), oauth_refresh_url: None,
    }, state.config.key_pepper.as_bytes()).await.unwrap();
    (state, directory, server, account.id)
}

async fn catalog(server: &MockServer, body: Value) {
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(query_param("client_version", "0.160.0"))
        .and(matches_header(
            "authorization",
            "Bearer private-access-canary",
        ))
        .and(matches_header(
            "chatgpt-account-id",
            "private-account-canary",
        ))
        .and(matches_header("originator", "codex-tui"))
        .and(matches_header("accept-encoding", "identity"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn request(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Value,
    token: &str,
) -> (StatusCode, Value, bool) {
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let no_store = response
        .headers()
        .get(header::CACHE_CONTROL)
        .is_some_and(|value| value == "no-store");
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    for secret in [
        "private-access-canary",
        "private-refresh-canary",
        "private-account-canary",
        "raw-body-canary",
    ] {
        assert!(!text.contains(secret), "diagnostic leaked upstream data");
    }
    (status, serde_json::from_slice(&bytes).unwrap(), no_store)
}

fn upstream_body() -> Value {
    json!({"models":[
        {"slug":"gpt-6-sol", "visibility":"list", "context_window":272000},
        {"slug":SELECTED, "visibility":"hide", "supported_in_api":false, "context_window":400000, "instructions":"raw-body-canary"},
        {"slug":"unrequested-hidden", "visibility":"hide", "context_window":128000}
    ]})
}

#[tokio::test]
async fn hidden_exact_selection_bootstraps_route_creation_and_candidate_admission() {
    let (state, _directory, server, account) = fixture().await;
    catalog(&server, upstream_body()).await;
    let base = format!("/internal/v1/upstreams/{account}/models");
    let token = &state.config.service_token;
    let (status, diagnostic, no_store) = request(
        &state,
        "GET",
        &format!("{base}/discovery?model={SELECTED}"),
        Value::Null,
        token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{diagnostic}");
    assert!(no_store);
    assert_eq!(diagnostic["present"], true);
    assert_eq!(diagnostic["visibility"], "hide");
    assert_eq!(diagnostic["filter_decision"], "excluded_visibility");
    assert_eq!(diagnostic["metadata_valid"], true);
    assert_eq!(diagnostic["client_version"], "0.160.0");
    let (_, default_catalog, _) =
        request(&state, "POST", &format!("{base}/sync"), Value::Null, token).await;
    assert_eq!(default_catalog["models"].as_array().unwrap().len(), 1);
    assert!(
        state
            .db
            .validate_managed_codex_route_catalog("exact-tenant", &[account], SELECTED)
            .await
            .is_err()
    );
    assert!(
        state
            .db
            .filter_accounts_supporting_upstream_model("exact-tenant", &[account], SELECTED)
            .await
            .unwrap()
            .is_empty()
    );

    let (status, selected, _) = request(
        &state,
        "POST",
        &format!("{base}/sync?model={SELECTED}"),
        Value::Null,
        token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{selected}");
    assert_eq!(selected["status"], "ready");
    assert_eq!(selected["credential_generation"], 1);
    assert_eq!(selected["models"].as_array().unwrap().len(), 2);
    let model = selected["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["id"] == SELECTED)
        .unwrap();
    assert_eq!(model["context_window"], 400000);
    assert_eq!(model["reservation_token_bound"], 400000);
    assert!(
        state
            .db
            .configured_upstream_model_ids(account)
            .await
            .unwrap()
            .is_empty()
    );
    state
        .db
        .validate_managed_codex_route_catalog("exact-tenant", &[account], SELECTED)
        .await
        .unwrap();
    assert_eq!(
        state
            .db
            .filter_accounts_supporting_upstream_model("exact-tenant", &[account], SELECTED)
            .await
            .unwrap(),
        vec![account]
    );
    assert!(
        state
            .db
            .filter_accounts_supporting_upstream_model(
                "exact-tenant",
                &[account],
                "unrequested-hidden"
            )
            .await
            .unwrap()
            .is_empty()
    );

    let (status, route, _) = request(
        &state,
        "POST",
        "/internal/v1/model-routes",
        json!({
            "tenant_external_id":"exact-tenant", "public_model":SELECTED, "upstream_model":SELECTED,
            "protocol":"openai", "priority":0, "enabled":false,
            "upstream_account_ids":[account], "custom_model_confirmed":true
        }),
        token,
    )
    .await;
    assert!(status.is_success(), "{status}: {route}");
    assert_eq!(route["enabled"], false);
    assert_eq!(
        state
            .db
            .configured_upstream_model_ids(account)
            .await
            .unwrap(),
        vec![SELECTED.to_owned()]
    );
    let (_, refreshed, _) =
        request(&state, "POST", &format!("{base}/sync"), Value::Null, token).await;
    assert!(
        refreshed["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value["id"] == SELECTED)
    );
    let (_, diagnostic, _) = request(
        &state,
        "GET",
        &format!("{base}/discovery?model={SELECTED}"),
        Value::Null,
        token,
    )
    .await;
    assert_eq!(diagnostic["filter_decision"], "included");
}

#[tokio::test]
async fn absent_or_invalid_metadata_cannot_bootstrap_an_alias() {
    let (state, _directory, server, account) = fixture().await;
    let observer = sqlx::AnyPool::connect(&state.config.database_url)
        .await
        .unwrap();
    let base = format!("/internal/v1/upstreams/{account}/models");
    let token = &state.config.service_token;
    catalog(
        &server,
        json!({"models":[{"slug":"gpt-6-sol", "visibility":"list", "context_window":272000}]}),
    )
    .await;
    let (status, working_catalog, _) =
        request(&state, "POST", &format!("{base}/sync"), Value::Null, token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(working_catalog["status"], "ready");
    let working_snapshot: String = sqlx::query_scalar(
        "SELECT current_snapshot_id FROM upstream_model_catalog_state WHERE upstream_account_id = $1",
    )
    .bind(account.to_string())
    .fetch_one(&observer)
    .await
    .unwrap();
    let (_, diagnostic, _) = request(
        &state,
        "GET",
        &format!("{base}/discovery?model={SELECTED}"),
        Value::Null,
        token,
    )
    .await;
    assert_eq!(diagnostic["present"], false);
    assert_eq!(diagnostic["filter_decision"], "absent");
    assert!(diagnostic["visibility"].is_null());
    let (status, _, _) = request(
        &state,
        "POST",
        &format!("{base}/sync?model={SELECTED}"),
        Value::Null,
        token,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, retained_catalog, _) = request(&state, "GET", &base, Value::Null, token).await;
    assert_eq!(retained_catalog["models"], working_catalog["models"]);
    assert_eq!(
        retained_catalog["disabled_models"],
        working_catalog["disabled_models"]
    );
    assert_eq!(
        retained_catalog["last_success_at"],
        working_catalog["last_success_at"]
    );
    let retained_snapshot: String = sqlx::query_scalar(
        "SELECT current_snapshot_id FROM upstream_model_catalog_state WHERE upstream_account_id = $1",
    )
    .bind(account.to_string())
    .fetch_one(&observer)
    .await
    .unwrap();
    assert_eq!(retained_snapshot, working_snapshot);
    state
        .db
        .validate_managed_codex_route_catalog("exact-tenant", &[account], "gpt-6-sol")
        .await
        .unwrap();
    assert_eq!(
        state
            .db
            .filter_accounts_supporting_upstream_model("exact-tenant", &[account], "gpt-6-sol")
            .await
            .unwrap(),
        vec![account]
    );
    assert!(
        state
            .db
            .validate_managed_codex_route_catalog("exact-tenant", &[account], SELECTED)
            .await
            .is_err()
    );

    for body in [
        json!({"models":[{"slug":SELECTED,"visibility":"hide"}]}),
        json!({"models":[{"slug":SELECTED,"visibility":"hide","context_window":400000}],"has_more":true}),
        json!({"models":[{"slug":SELECTED,"visibility":"hide","context_window":400000},{"slug":SELECTED,"visibility":"list","context_window":400000}]}),
    ] {
        server.reset().await;
        catalog(&server, body).await;
        let (status, _, _) = request(
            &state,
            "POST",
            &format!("{base}/sync?model={SELECTED}"),
            Value::Null,
            token,
        )
        .await;
        assert!(!status.is_success());
        assert!(
            state
                .db
                .filter_accounts_supporting_upstream_model("exact-tenant", &[account], SELECTED)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn exact_discovery_enforces_scopes_and_tenant_before_upstream_io() {
    let (state, _directory, server, account) = fixture().await;
    state.db.create_tenant("other-tenant", None).await.unwrap();
    let base = format!("/internal/v1/upstreams/{account}/models");
    for (tenant, scopes, method, suffix, expected) in [
        (
            "other-tenant",
            vec!["providers:read"],
            "GET",
            "discovery",
            StatusCode::FORBIDDEN,
        ),
        (
            "exact-tenant",
            vec!["routes:read"],
            "GET",
            "discovery",
            StatusCode::FORBIDDEN,
        ),
        (
            "exact-tenant",
            vec!["providers:read"],
            "POST",
            "sync",
            StatusCode::FORBIDDEN,
        ),
    ] {
        let service = state
            .db
            .create_service_token(
                CreateServiceTokenInput {
                    name: format!("{tenant}-{method}-{suffix}"),
                    tenant_external_id: Some(tenant.into()),
                    scopes: scopes.into_iter().map(str::to_owned).collect(),
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let (status, _, _) = request(
            &state,
            method,
            &format!("{base}/{suffix}?model={SELECTED}"),
            Value::Null,
            &service.token,
        )
        .await;
        assert_eq!(status, expected);
    }
    let (status, _, _) = request(
        &state,
        "GET",
        &format!("{base}/discovery?model=%20"),
        Value::Null,
        &state.config.service_token,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = request(
        &state,
        "POST",
        &format!("{base}/sync-routes?model={SELECTED}"),
        Value::Null,
        &state.config.service_token,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(server.received_requests().await.unwrap().is_empty());
}
