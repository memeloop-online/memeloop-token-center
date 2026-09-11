use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{
        CreateGroupInput, CreateModelRouteInput, CreateRoutedModelRouteInput,
        CreateServiceTokenInput, CreateUpstreamAccountInput, DiscoveredUpstreamModel, GroupKind,
        ReplaceGroupMembersInput, ReplaceModelCatalogResult, unix_millis,
    },
    error::AppError,
    provider::UpstreamCredential,
};
use serde_json::{Value, json};
use sqlx::Connection;
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
        .and(query_param("client_version", "0.146.0"))
        .and(matches_header("authorization", "Bearer codex-access"))
        .and(matches_header("originator", "codex-tui"))
        .and(matches_header(
            "user-agent",
            "codex-tui/0.146.0 (Mac OS 26.5.0; arm64) iTerm.app/3.6.10 (codex-tui; 0.146.0)",
        ))
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
async fn codex_catalog_sync_preserves_bound_for_explicit_custom_route() {
    let (state, _directory) = state("codex-custom-model-bound").await;
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "codex-custom-tenant".into(),
                name: "codex-custom-upstream".into(),
                driver: "openai-codex".into(),
                config: json!({
                    "base_url": "https://chatgpt.com/backend-api/codex",
                    "network_scope": "public",
                    "reservation_token_bounds": {
                        "catalog-model": 100_000,
                        "gpt-5.6-terra": 100_000,
                        "unused-custom-model": 100_000
                    }
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
    state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: "codex-custom-tenant".into(),
            public_model: "gpt-5.6-terra".into(),
            upstream_account_id: account.id,
            upstream_model: "gpt-5.6-terra".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();

    let lease = Uuid::now_v7();
    assert!(
        state
            .db
            .claim_upstream_model_catalog_sync(
                account.id,
                "codex-custom-tenant",
                account.credential_generation,
                lease
            )
            .await
            .unwrap()
    );
    assert_eq!(
        state
            .db
            .replace_upstream_model_catalog(
                account.id,
                "codex-custom-tenant",
                account.credential_generation,
                lease,
                "codex_models",
                &[DiscoveredUpstreamModel {
                    model_id: "catalog-model".into(),
                    protocol: "openai".into(),
                    context_window: Some(272_000),
                    reservation_token_bound: Some(272_000),
                    reservation_bound_source: Some("mtc_context_window_bound".into()),
                }],
            )
            .await
            .unwrap(),
        ReplaceModelCatalogResult::Replaced
    );

    let (updated, _) = state
        .db
        .upstream_account_with_credential(account.id, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert_eq!(
        updated.config["reservation_token_bounds"],
        json!({
            "catalog-model": 272_000,
            "gpt-5.6-terra": 100_000
        })
    );

    // Catalog pruning may serialize before a new explicit-custom route. The
    // association transaction must recreate its transport reservation bound
    // before publishing that route.
    state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: "codex-custom-tenant".into(),
            public_model: "unused-custom-model".into(),
            upstream_account_id: account.id,
            upstream_model: "unused-custom-model".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    let (updated, _) = state
        .db
        .upstream_account_with_credential(account.id, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert_eq!(
        updated.config["reservation_token_bounds"]["unused-custom-model"],
        1_000_000_000
    );
}

#[tokio::test]
async fn codex_catalog_without_trusted_models_records_error_and_releases_sync_lease() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(query_param("client_version", "0.146.0"))
        .and(matches_header("authorization", "Bearer codex-access"))
        .and(matches_header("originator", "codex-tui"))
        .and(matches_header(
            "user-agent",
            "codex-tui/0.146.0 (Mac OS 26.5.0; arm64) iTerm.app/3.6.10 (codex-tui; 0.146.0)",
        ))
        .and(matches_header("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                {"slug": "not-entitled", "supported_in_api": false, "visibility": "list", "context_window": 272000},
                {"slug": "not-listed", "supported_in_api": true, "visibility": "hide", "context_window": 272000}
            ]
        })))
        // A second call must claim a fresh lease rather than inheriting the
        // failed catalog sync's 30-second lease.
        .expect(2)
        .mount(&server)
        .await;
    let (state, _directory) = state("codex-empty-model-catalog").await;
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "codex-empty-tenant".into(),
                name: "codex-empty-upstream".into(),
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
    for _ in 0..2 {
        let (status, catalog) = request(
            &state,
            "POST",
            &format!(
                "/internal/v1/upstreams/{}/models/sync?tenant_external_id=codex-empty-tenant",
                account.id
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{catalog}");
        assert_eq!(catalog["status"], "error");
        assert_eq!(catalog["error_code"], "codex_no_trusted_models");
        assert_eq!(catalog["models"], json!([]));
    }
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

const POSTGRES_CATALOG_INTERLEAVING_SERIAL_KEY: i64 = 7_341_909_207_811;

async fn postgres_codex_account(
    database: &memeloop_token_center::db::Database,
    tenant: &str,
    account_name: &str,
) -> memeloop_token_center::provider::UpstreamAccountView {
    database
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.to_owned(),
                name: account_name.to_owned(),
                driver: "openai-codex".to_owned(),
                config: json!({
                    "base_url": "https://chatgpt.com/backend-api/codex",
                    "network_scope": "public",
                    "reservation_token_bounds": {
                        "unused-custom-model": 100_000
                    }
                }),
                credential: UpstreamCredential::OAuth {
                    access_token: "postgres-codex-access".to_owned(),
                    refresh_token: Some("postgres-codex-refresh".to_owned()),
                    expires_at: Some(unix_millis() + 60_000),
                    header: "authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                    adapter_state: Some(json!({
                        "schema": "openai-codex-oauth-v1",
                        "account_id": "postgres-account"
                    })),
                    proxy_url: None,
                    proxy_network_scope: None,
                },
                oauth_session_id: Some(Uuid::now_v7()),
                oauth_driver: Some("openai_codex_device".to_owned()),
                oauth_refresh_url: None,
            },
            b"postgres catalog race pepper is long enough",
        )
        .await
        .expect("create PostgreSQL Codex account")
}

async fn install_postgres_pause_trigger(
    pool: &sqlx::AnyPool,
    function_name: &str,
    application_name: &str,
    table: &str,
    event: &str,
    predicate: &str,
    advisory_key: i64,
) {
    let trigger_name = format!("{function_name}_trigger");
    let sql = format!(
        "CREATE FUNCTION {function_name}() RETURNS trigger LANGUAGE plpgsql AS $body$ \
         BEGIN IF {predicate} THEN \
           PERFORM set_config('application_name', '{application_name}', false); \
           PERFORM pg_advisory_xact_lock({advisory_key}); \
         END IF; \
         RETURN NEW; END $body$; \
         CREATE TRIGGER {trigger_name} BEFORE {event} ON {table} \
         FOR EACH ROW EXECUTE FUNCTION {function_name}();"
    );
    // Test-only SQL: every identifier, predicate, and advisory key is derived
    // from UUIDs generated in this process; no external input reaches it.
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(pool)
        .await
        .expect("install PostgreSQL interleaving trigger");
}

async fn wait_for_postgres_advisory_pause(pool: &sqlx::AnyPool, application_name: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity \
                 WHERE application_name = $1 AND state = 'active' \
                   AND wait_event_type = 'Lock' AND wait_event = 'advisory'",
            )
            .bind(application_name)
            .fetch_one(pool)
            .await
            .expect("inspect PostgreSQL advisory pause");
            if waiting > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("expected PostgreSQL advisory pause");
}

async fn wait_for_postgres_transaction_blocked_by(
    pool: &sqlx::AnyPool,
    blocker_application_name: &str,
) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity waiting \
                 JOIN pg_stat_activity blocker \
                   ON blocker.application_name = $1 \
                  AND blocker.pid = ANY(pg_blocking_pids(waiting.pid)) \
                 WHERE waiting.state = 'active' AND waiting.wait_event_type = 'Lock' \
                   AND waiting.wait_event = 'transactionid'",
            )
            .bind(blocker_application_name)
            .fetch_one(pool)
            .await
            .expect("inspect PostgreSQL transaction blocker");
            if waiting > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("expected a PostgreSQL transaction blocked by the paused writer");
}

async fn postgres_catalog_race_fixture(
    database_url: &str,
    label: &str,
) -> (
    memeloop_token_center::db::Database,
    memeloop_token_center::db::Database,
    sqlx::AnyPool,
    String,
    String,
    memeloop_token_center::provider::UpstreamAccountView,
) {
    let tenant = format!("catalog-lock-{label}");
    let custom_model = format!("custom-{label}");
    let setup = memeloop_token_center::db::Database::connect_with_max(database_url, 4)
        .await
        .expect("connect PostgreSQL catalog setup");
    setup
        .migrate()
        .await
        .expect("migrate PostgreSQL catalog setup");
    let account = postgres_codex_account(&setup, &tenant, &format!("postgres-codex-{label}")).await;
    let observer = sqlx::AnyPool::connect(database_url)
        .await
        .expect("connect PostgreSQL catalog observer");
    sqlx::query(
        "INSERT INTO routing_relation_write_locks (tenant_id, generation) VALUES ($1, 0) ON CONFLICT (tenant_id) DO NOTHING",
    )
    .bind(account.tenant_id.to_string())
    .execute(&observer)
    .await
    .expect("seed committed tenant relation lock");
    let route = memeloop_token_center::db::Database::connect_with_max(database_url, 2)
        .await
        .expect("connect PostgreSQL route writer");
    let catalog = memeloop_token_center::db::Database::connect_with_max(database_url, 2)
        .await
        .expect("connect PostgreSQL catalog writer");
    (route, catalog, observer, tenant, custom_model, account)
}

#[tokio::test]
async fn postgres_route_then_catalog_preserves_the_committed_custom_bound() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!("skipping PostgreSQL route/catalog interleaving: MTC_TEST_POSTGRES_URL is unset");
        return;
    };
    let unique = Uuid::now_v7();
    let label = format!("route-first-{unique}");
    let (route_database, catalog_database, observer, tenant, custom_model, account) =
        postgres_catalog_race_fixture(&database_url, &label).await;
    let mut serial_guard = sqlx::AnyConnection::connect(&database_url)
        .await
        .expect("connect route-first serial guard");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(POSTGRES_CATALOG_INTERLEAVING_SERIAL_KEY)
        .execute(&mut serial_guard)
        .await
        .expect("serialize PostgreSQL catalog interleavings");
    let suffix = unique.simple();
    let function_name = format!("pause_route_catalog_{suffix}");
    let paused_application = format!("mtc-pause-route-{unique}");
    let advisory_key = (unique.as_u128() & i64::MAX as u128) as i64;
    install_postgres_pause_trigger(
        &observer,
        &function_name,
        &paused_application,
        "model_routes",
        "INSERT",
        &format!("NEW.tenant_id = '{}'", account.tenant_id),
        advisory_key,
    )
    .await;
    let mut barrier = sqlx::AnyConnection::connect(&database_url)
        .await
        .expect("connect route-first barrier");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(advisory_key)
        .execute(&mut barrier)
        .await
        .expect("hold route-first barrier");

    let route_tenant = tenant.clone();
    let route_model = custom_model.clone();
    let account_id = account.id;
    let route_task = tokio::spawn(async move {
        route_database
            .create_routed_model_route(CreateRoutedModelRouteInput {
                tenant_external_id: route_tenant,
                public_model: format!("public-{route_model}"),
                upstream_model: route_model,
                protocol: "openai".to_owned(),
                priority: 0,
                enabled: true,
                upstream_account_ids: vec![account_id],
                included_provider_group_ids: Vec::new(),
                excluded_provider_group_ids: Vec::new(),
                route_group_ids: Vec::new(),
                route_group_names: Vec::new(),
                granted_credential_ids: Vec::new(),
                custom_model_confirmed: true,
            })
            .await
    });
    wait_for_postgres_advisory_pause(&observer, &paused_application).await;

    let lease = Uuid::now_v7();
    assert!(
        catalog_database
            .claim_upstream_model_catalog_sync(account.id, &tenant, 1, lease)
            .await
            .expect("claim route-first catalog lease")
    );
    let catalog_tenant = tenant.clone();
    let account_id = account.id;
    let catalog_task = tokio::spawn(async move {
        catalog_database
            .replace_upstream_model_catalog(
                account_id,
                &catalog_tenant,
                1,
                lease,
                "codex_models",
                &[DiscoveredUpstreamModel {
                    model_id: "catalog-model".to_owned(),
                    protocol: "openai".to_owned(),
                    context_window: Some(272_000),
                    reservation_token_bound: Some(272_000),
                    reservation_bound_source: Some("mtc_context_window_bound".to_owned()),
                }],
            )
            .await
    });
    wait_for_postgres_transaction_blocked_by(&observer, &paused_application).await;
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(advisory_key)
        .execute(&mut barrier)
        .await
        .expect("release route-first barrier");
    route_task
        .await
        .expect("join route-first writer")
        .expect("commit route-first association");
    assert_eq!(
        catalog_task
            .await
            .expect("join route-first catalog")
            .expect("commit route-first catalog"),
        ReplaceModelCatalogResult::Replaced
    );
    let config_json: String =
        sqlx::query_scalar("SELECT config_json FROM upstream_accounts WHERE id = $1")
            .bind(account.id.to_string())
            .fetch_one(&observer)
            .await
            .expect("load route-first config");
    let config: Value = serde_json::from_str(&config_json).expect("route-first config JSON");
    assert_eq!(
        config["reservation_token_bounds"][custom_model.as_str()],
        1_000_000_000
    );
    assert_eq!(config["reservation_token_bounds"]["catalog-model"], 272_000);
    assert!(
        config["reservation_token_bounds"]
            .get("unused-custom-model")
            .is_none()
    );
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(POSTGRES_CATALOG_INTERLEAVING_SERIAL_KEY)
        .execute(&mut serial_guard)
        .await
        .expect("release PostgreSQL catalog interleaving guard");
}

#[tokio::test]
async fn postgres_catalog_then_route_recreates_the_required_custom_bound_atomically() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!("skipping PostgreSQL catalog/route interleaving: MTC_TEST_POSTGRES_URL is unset");
        return;
    };
    let unique = Uuid::now_v7();
    let label = format!("catalog-first-{unique}");
    let (route_database, catalog_database, observer, tenant, custom_model, account) =
        postgres_catalog_race_fixture(&database_url, &label).await;
    let mut serial_guard = sqlx::AnyConnection::connect(&database_url)
        .await
        .expect("connect catalog-first serial guard");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(POSTGRES_CATALOG_INTERLEAVING_SERIAL_KEY)
        .execute(&mut serial_guard)
        .await
        .expect("serialize PostgreSQL catalog interleavings");
    let suffix = unique.simple();
    let function_name = format!("pause_catalog_route_{suffix}");
    let paused_application = format!("mtc-pause-catalog-{unique}");
    let advisory_key = (unique.as_u128() & i64::MAX as u128) as i64;
    install_postgres_pause_trigger(
        &observer,
        &function_name,
        &paused_application,
        "upstream_accounts",
        "UPDATE",
        &format!("NEW.id = '{}'", account.id),
        advisory_key,
    )
    .await;
    let mut barrier = sqlx::AnyConnection::connect(&database_url)
        .await
        .expect("connect catalog-first barrier");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(advisory_key)
        .execute(&mut barrier)
        .await
        .expect("hold catalog-first barrier");
    let lease = Uuid::now_v7();
    assert!(
        catalog_database
            .claim_upstream_model_catalog_sync(account.id, &tenant, 1, lease)
            .await
            .expect("claim catalog-first lease")
    );
    let catalog_tenant = tenant.clone();
    let account_id = account.id;
    let catalog_task = tokio::spawn(async move {
        catalog_database
            .replace_upstream_model_catalog(
                account_id,
                &catalog_tenant,
                1,
                lease,
                "codex_models",
                &[DiscoveredUpstreamModel {
                    model_id: "catalog-model".to_owned(),
                    protocol: "openai".to_owned(),
                    context_window: Some(272_000),
                    reservation_token_bound: Some(272_000),
                    reservation_bound_source: Some("mtc_context_window_bound".to_owned()),
                }],
            )
            .await
    });
    wait_for_postgres_advisory_pause(&observer, &paused_application).await;

    let route_tenant = tenant.clone();
    let route_model = custom_model.clone();
    let account_id = account.id;
    let route_task = tokio::spawn(async move {
        route_database
            .create_routed_model_route(CreateRoutedModelRouteInput {
                tenant_external_id: route_tenant,
                public_model: format!("public-{route_model}"),
                upstream_model: route_model,
                protocol: "openai".to_owned(),
                priority: 0,
                enabled: true,
                upstream_account_ids: vec![account_id],
                included_provider_group_ids: Vec::new(),
                excluded_provider_group_ids: Vec::new(),
                route_group_ids: Vec::new(),
                route_group_names: Vec::new(),
                granted_credential_ids: Vec::new(),
                custom_model_confirmed: true,
            })
            .await
    });
    wait_for_postgres_transaction_blocked_by(&observer, &paused_application).await;
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(advisory_key)
        .execute(&mut barrier)
        .await
        .expect("release catalog-first barrier");
    assert_eq!(
        catalog_task
            .await
            .expect("join catalog-first catalog")
            .expect("commit catalog-first catalog"),
        ReplaceModelCatalogResult::Replaced
    );
    route_task
        .await
        .expect("join catalog-first route")
        .expect("commit catalog-first association");
    let config_json: String =
        sqlx::query_scalar("SELECT config_json FROM upstream_accounts WHERE id = $1")
            .bind(account.id.to_string())
            .fetch_one(&observer)
            .await
            .expect("load catalog-first config");
    let config: Value = serde_json::from_str(&config_json).expect("catalog-first config JSON");
    assert_eq!(
        config["reservation_token_bounds"][custom_model.as_str()],
        1_000_000_000
    );
    assert_eq!(config["reservation_token_bounds"]["catalog-model"], 272_000);
    assert!(
        config["reservation_token_bounds"]
            .get("unused-custom-model")
            .is_none()
    );
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(POSTGRES_CATALOG_INTERLEAVING_SERIAL_KEY)
        .execute(&mut serial_guard)
        .await
        .expect("release PostgreSQL catalog interleaving guard");
}
