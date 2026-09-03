use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{
        CreateGroupInput, CreateServiceTokenInput, CreateUpstreamAccountInput,
        DiscoveredUpstreamModel, GroupKind, ReplaceGroupMembersInput, ReplaceModelCatalogResult,
        unix_millis,
    },
    error::AppError,
    provider::UpstreamCredential,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header as matches_header, method, path, query_param},
};

async fn state(label: &str) -> (AppState, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join(format!("{label}.db")).display()
    );
    (
        AppState::initialize(Config::for_test(database_url))
            .await
            .unwrap(),
        directory,
    )
}

async fn request(state: &AppState, method_name: &str, uri: &str) -> (StatusCode, Value) {
    request_as(state, method_name, uri, &state.config.service_token).await
}

async fn request_as(
    state: &AppState,
    method_name: &str,
    uri: &str,
    token: &str,
) -> (StatusCode, Value) {
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::builder()
                .method(method_name)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, value)
}

#[tokio::test]
async fn openai_catalog_sync_is_authenticated_bounded_and_failure_preserves_snapshot() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(matches_header("authorization", "Bearer catalog-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "gpt-alpha"},
                {"id": "gpt-beta", "protocol": "openai"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (state, _directory) = state("openai-model-catalog").await;
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "catalog-tenant".into(),
                name: "catalog-upstream".into(),
                driver: "http-json".into(),
                config: json!({"base_url": server.uri()}),
                credential: UpstreamCredential::ApiKey {
                    value: "catalog-secret".into(),
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
    let (status, synced) = request(
        &state,
        "POST",
        &format!(
            "/internal/v1/upstreams/{}/models/sync?tenant_external_id=catalog-tenant",
            account.id
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{synced}");
    assert_eq!(synced["status"], "ready");
    assert_eq!(synced["models"].as_array().unwrap().len(), 2);

    // The one expected mock has been consumed. A 404 is reduced to a static
    // code while the previous complete snapshot remains searchable.
    server.reset().await;
    let (status, failed) = request(
        &state,
        "POST",
        &format!(
            "/internal/v1/upstreams/{}/models/sync?tenant_external_id=catalog-tenant",
            account.id
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{failed}");
    assert_eq!(failed["status"], "stale");
    assert_eq!(failed["error_code"], "upstream_unavailable");
    assert_eq!(failed["models"].as_array().unwrap().len(), 2);

    let (status, cross_tenant) = request(
        &state,
        "GET",
        &format!(
            "/internal/v1/upstreams/{}/models?tenant_external_id=other-tenant",
            account.id
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{cross_tenant}");
}

#[tokio::test]
async fn codex_catalog_uses_native_contract_and_persists_context_window_reservation_bound() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(query_param("client_version", env!("CARGO_PKG_VERSION")))
        .and(matches_header("authorization", "Bearer codex-access"))
        .and(matches_header("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                {"slug": "gpt-codex", "supported_in_api": true, "visibility": "list", "context_window": 272000},
                {"slug": "hidden", "supported_in_api": true, "visibility": "hide", "context_window": 272000},
                {"slug": "unsupported", "supported_in_api": false, "visibility": "list", "context_window": 272000}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (state, _directory) = state("codex-model-catalog").await;
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "codex-tenant".into(),
                name: "codex-upstream".into(),
                driver: "openai-codex".into(),
                config: json!({
                    "base_url": server.uri(),
                    "network_scope": "public",
                    "output_token_limits": {}
                }),
                credential: UpstreamCredential::OAuth {
                    access_token: "codex-access".into(),
                    refresh_token: Some("codex-refresh".into()),
                    expires_at: Some(unix_millis() + 60_000),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: Some(json!({
                        "schema": "openai-codex-oauth-v1",
                        "account_id": "account-123"
                    })),
                    proxy_url: None,
                    proxy_network_scope: None,
                },
                oauth_session_id: Some(Uuid::now_v7()),
                oauth_driver: Some("openai_codex_device".into()),
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let (status, synced) = request(
        &state,
        "POST",
        &format!(
            "/internal/v1/upstreams/{}/models/sync?tenant_external_id=codex-tenant",
            account.id
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{synced}");
    assert_eq!(synced["models"].as_array().unwrap().len(), 1);
    assert_eq!(synced["models"][0]["id"], "gpt-codex");
    assert_eq!(synced["models"][0]["context_window"], 272000);
    assert_eq!(synced["models"][0]["reservation_token_bound"], 272000);
    assert_eq!(
        synced["models"][0]["reservation_bound_source"],
        "mtc_context_window_bound"
    );
    let (updated, _) = state
        .db
        .upstream_account_with_credential(account.id, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert_eq!(
        updated.config["reservation_token_bounds"]["gpt-codex"],
        272000
    );
}

#[tokio::test]
async fn aggregate_uses_only_provider_groups_and_exclusion_wins() {
    let (state, _directory) = state("catalog-aggregate").await;
    let mut accounts = Vec::new();
    for name in ["alpha", "mixed", "beta"] {
        accounts.push(
            state
                .db
                .create_upstream_account(
                    CreateUpstreamAccountInput {
                        tenant_external_id: "aggregate-tenant".into(),
                        name: name.into(),
                        driver: "http-json".into(),
                        config: json!({"base_url": "https://example.com"}),
                        credential: UpstreamCredential::None,
                        oauth_session_id: None,
                        oauth_driver: None,
                        oauth_refresh_url: None,
                    },
                    state.config.key_pepper.as_bytes(),
                )
                .await
                .unwrap(),
        );
    }
    for (index, models) in [
        vec!["model-alpha"],
        vec!["model-alpha", "model-beta"],
        vec!["model-beta"],
    ]
    .into_iter()
    .enumerate()
    {
        let lease = Uuid::now_v7();
        assert!(
            state
                .db
                .claim_upstream_model_catalog_sync(accounts[index].id, "aggregate-tenant", 1, lease)
                .await
                .unwrap()
        );
        let discovered = models
            .into_iter()
            .map(|model_id| DiscoveredUpstreamModel {
                model_id: model_id.into(),
                protocol: "any".into(),
                context_window: None,
                reservation_token_bound: None,
                reservation_bound_source: None,
            })
            .collect::<Vec<_>>();
        state
            .db
            .replace_upstream_model_catalog(
                accounts[index].id,
                "aggregate-tenant",
                1,
                lease,
                "openai_v1",
                &discovered,
            )
            .await
            .unwrap();
    }
    let included = state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: "aggregate-tenant".into(),
                name: "included".into(),
            },
        )
        .await
        .unwrap();
    let included = state
        .db
        .replace_group_members(
            GroupKind::Provider,
            included.id,
            ReplaceGroupMembersInput {
                tenant_external_id: "aggregate-tenant".into(),
                member_ids: vec![accounts[0].id, accounts[1].id],
                expected_updated_at: included.updated_at,
            },
        )
        .await
        .unwrap();
    let excluded = state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: "aggregate-tenant".into(),
                name: "excluded".into(),
            },
        )
        .await
        .unwrap();
    let excluded = state
        .db
        .replace_group_members(
            GroupKind::Provider,
            excluded.id,
            ReplaceGroupMembersInput {
                tenant_external_id: "aggregate-tenant".into(),
                member_ids: vec![accounts[1].id],
                expected_updated_at: excluded.updated_at,
            },
        )
        .await
        .unwrap();

    let view = state
        .db
        .aggregate_upstream_models(
            "aggregate-tenant",
            &[accounts[2].id],
            &[included.id],
            &[excluded.id],
            None,
            100,
        )
        .await
        .unwrap();
    assert_eq!(view.eligible_account_count, 2);
    assert_eq!(view.data.len(), 2);
    assert!(
        view.data
            .iter()
            .all(|model| model.supported_account_count == 1 && !model.complete_coverage)
    );

    let route_group = state
        .db
        .create_group(
            GroupKind::Route,
            CreateGroupInput {
                tenant_external_id: "aggregate-tenant".into(),
                name: "routing-only".into(),
            },
        )
        .await
        .unwrap();
    let ignored = state
        .db
        .aggregate_upstream_models("aggregate-tenant", &[], &[route_group.id], &[], None, 100)
        .await
        .unwrap();
    assert_eq!(ignored.eligible_account_count, 0);
    assert!(ignored.data.is_empty());

    let credential_group = state
        .db
        .create_group(
            GroupKind::Credential,
            CreateGroupInput {
                tenant_external_id: "aggregate-tenant".into(),
                name: "display-only".into(),
            },
        )
        .await
        .unwrap();
    let ignored = state
        .db
        .aggregate_upstream_models(
            "aggregate-tenant",
            &[],
            &[credential_group.id],
            &[],
            None,
            100,
        )
        .await
        .unwrap();
    assert_eq!(ignored.eligible_account_count, 0);
    assert!(ignored.data.is_empty());

    let routes_reader = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "catalog-routes-reader".into(),
                scopes: vec!["routes:read".into()],
                tenant_external_id: Some("aggregate-tenant".into()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let (status, _) = request_as(
        &state,
        "GET",
        "/internal/v1/upstream-models?account_ids=&q=model",
        &routes_reader.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request_as(
        &state,
        "POST",
        &format!("/internal/v1/upstreams/{}/models/sync", accounts[0].id),
        &routes_reader.token,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn catalog_sync_rejects_ssrf_and_oversized_responses_with_static_codes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 2 * 1024 * 1024 + 1]))
        .expect(1)
        .mount(&server)
        .await;
    let (state, _directory) = state("catalog-security").await;
    let oversized = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "security-tenant".into(),
                name: "oversized".into(),
                driver: "http-json".into(),
                config: json!({"base_url": server.uri()}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let (status, body) = request(
        &state,
        "POST",
        &format!(
            "/internal/v1/upstreams/{}/models/sync?tenant_external_id=security-tenant",
            oversized.id
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "error");
    assert_eq!(body["error_code"], "response_too_large");

    let metadata = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "security-tenant".into(),
                name: "metadata".into(),
                driver: "http-json".into(),
                config: json!({"base_url": "http://169.254.169.254/latest/meta-data"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let (status, body) = request(
        &state,
        "POST",
        &format!(
            "/internal/v1/upstreams/{}/models/sync?tenant_external_id=security-tenant",
            metadata.id
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "error");
    assert_eq!(body["error_code"], "destination_invalid");
    assert_eq!(body["models"], json!([]));
}

#[tokio::test]
async fn catalog_generation_cas_lease_and_account_deletion_are_safe() {
    let (state, _directory) = state("catalog-cas").await;
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "cas-tenant".into(),
                name: "cas-upstream".into(),
                driver: "http-json".into(),
                config: json!({"base_url": "https://example.com"}),
                credential: UpstreamCredential::ApiKey {
                    value: "old-secret".into(),
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
    let first_lease = Uuid::now_v7();
    assert!(
        state
            .db
            .claim_upstream_model_catalog_sync(account.id, "cas-tenant", 1, first_lease)
            .await
            .unwrap()
    );
    assert!(
        !state
            .db
            .claim_upstream_model_catalog_sync(account.id, "cas-tenant", 1, Uuid::now_v7())
            .await
            .unwrap()
    );
    let result = state
        .db
        .replace_upstream_model_catalog(
            account.id,
            "cas-tenant",
            1,
            first_lease,
            "openai_v1",
            &[DiscoveredUpstreamModel {
                model_id: "stable-model".into(),
                protocol: "any".into(),
                context_window: None,
                reservation_token_bound: None,
                reservation_bound_source: None,
            }],
        )
        .await
        .unwrap();
    assert_eq!(result, ReplaceModelCatalogResult::Replaced);

    let rotated = state
        .db
        .rotate_upstream_credential(
            account.id,
            UpstreamCredential::ApiKey {
                value: "new-secret".into(),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
            },
            "catalog-cas-rotation",
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let stale = state
        .db
        .replace_upstream_model_catalog(account.id, "cas-tenant", 1, first_lease, "openai_v1", &[])
        .await
        .unwrap();
    assert_eq!(
        stale,
        ReplaceModelCatalogResult::CredentialGenerationChanged
    );

    let disabled = state
        .db
        .set_upstream_account_status(account.id, "cas-tenant", "disabled", rotated.updated_at)
        .await
        .unwrap();
    state
        .db
        .delete_upstream_account(account.id, "cas-tenant", disabled.updated_at)
        .await
        .unwrap();
    assert!(matches!(
        state
            .db
            .upstream_model_catalog(account.id, "cas-tenant", None, 10)
            .await,
        Err(AppError::NotFound)
    ));
}

#[tokio::test]
async fn postgres_catalog_snapshot_and_generation_cas_use_the_same_contract() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!(
            "skipping real PostgreSQL model-catalog contract: MTC_TEST_POSTGRES_URL is unset"
        );
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "postgres-model", "context_window": 128000},
                {"id": "not-a-match"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap();
    let unique = Uuid::now_v7();
    let tenant = format!("catalog-postgres-{unique}");
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "postgres-catalog".into(),
                driver: "http-json".into(),
                config: json!({"base_url": server.uri()}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();

    let (status, synced) = request(
        &state,
        "POST",
        &format!(
            "/internal/v1/upstreams/{}/models/sync?tenant_external_id={tenant}",
            account.id
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{synced}");
    assert_eq!(synced["status"], "ready");

    let (status, searched) = request(
        &state,
        "GET",
        &format!(
            "/internal/v1/upstreams/{}/models?tenant_external_id={tenant}&q=POSTGRES&limit=10",
            account.id
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{searched}");
    assert_eq!(searched["models"].as_array().unwrap().len(), 1);
    assert_eq!(searched["models"][0]["id"], "postgres-model");

    let (status, aggregate) = request(
        &state,
        "GET",
        &format!(
            "/internal/v1/upstream-models?tenant_external_id={tenant}&account_ids={}&q=postgres&limit=10",
            account.id
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{aggregate}");
    assert_eq!(aggregate["eligible_account_count"], 1);
    assert_eq!(aggregate["data"][0]["id"], "postgres-model");
    assert_eq!(aggregate["data"][0]["complete_coverage"], true);

    let lease = Uuid::now_v7();
    assert!(
        state
            .db
            .claim_upstream_model_catalog_sync(account.id, &tenant, 1, lease)
            .await
            .unwrap()
    );
    let rotated = state
        .db
        .rotate_upstream_credential(
            account.id,
            UpstreamCredential::ApiKey {
                value: "postgres-new-secret".into(),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
            },
            &format!("postgres-catalog-rotate-{unique}"),
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(rotated.credential_generation, 2);
    assert_eq!(
        state
            .db
            .replace_upstream_model_catalog(account.id, &tenant, 1, lease, "openai_v1", &[])
            .await
            .unwrap(),
        ReplaceModelCatalogResult::CredentialGenerationChanged
    );
}
