use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{CreateServiceTokenInput, CreateUpstreamAccountInput},
    provider::{OAuthAdapterContribution, OAuthFlowKind, UpstreamCredential},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

async fn request(state: &AppState, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    request_with_token(
        state,
        method,
        path,
        body,
        state.config.service_token.as_str(),
    )
    .await
}

async fn request_with_token(
    state: &AppState,
    method: &str,
    path: &str,
    body: Value,
    token: &str,
) -> (StatusCode, Value) {
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(if method == "GET" {
                    Body::empty()
                } else {
                    Body::from(serde_json::to_vec(&body).unwrap())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn recursive_schema_accounts_are_cycle_aware_and_creation_is_atomic() {
    let directory = tempfile::tempdir().unwrap();
    let mut state = AppState::initialize(Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("recursive-secrets.db").display()
    )))
    .await
    .unwrap();
    let mock = wiremock::MockServer::start().await;
    for index in 0..4 {
        let secret = index >= 2;
        let node = json!({"type":"object","properties":{"label":{"type":"string"},"token":{"type":"string","writeOnly":secret},"next":{"$ref":"#/$defs/node"}}});
        let tree = match index {
            0 => json!({"type":"object"}),
            3 => json!({"type":"object","additionalProperties":{"$ref":"#/$defs/node"}}),
            _ => json!({"$ref":"#/$defs/node"}),
        };
        let mut provider = state.providers.get("http-json").unwrap().clone();
        provider.id = format!("recursive-config-{index}");
        provider.config_schema = json!({"type":"object","$defs":{"node":node,"unused_secret":{"writeOnly":true}},"properties":{"base_url":{"type":"string"},"tree":tree}});
        let driver = provider.id.clone();
        state.providers.extend([provider]).unwrap();
        let tenant = format!("recursive-secret-{index}");
        let tree_data = if index == 3 {
            json!({"entry":{"token":"synthetic-cycle","next":{}}})
        } else if secret {
            json!({"token":"synthetic-cycle","next":{}})
        } else {
            json!({"label":"public","next":{"label":"child"}})
        };
        let config = json!({"base_url":mock.uri(),"tree":tree_data});
        let (status, created) = request(&state,"POST","/internal/v1/upstreams",json!({"tenant_external_id":tenant,"name":"fixture","driver":driver,"config":config,"credential":{"type":"api_key","value":"synthetic-credential"}})).await;
        let list_path = format!("/internal/v1/upstreams?tenant_external_id={tenant}");
        if secret {
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let (status, listed) = request(&state, "GET", &list_path, Value::Null).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                listed.as_array().unwrap().len(),
                0,
                "rejected create must not insert an account"
            );
            // A historical row must still be listable and fully redacted; it
            // cannot turn a read into a post-commit schema error.
            let legacy = state
                .db
                .create_upstream_account(
                    CreateUpstreamAccountInput {
                        tenant_external_id: tenant.clone(),
                        name: "legacy".into(),
                        driver: driver.clone(),
                        config: config.clone(),
                        credential: UpstreamCredential::ApiKey {
                            value: "synthetic-credential".into(),
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
            let (status, listed) = request(&state, "GET", &list_path, Value::Null).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(listed[0]["config"], json!({}));
            let (status, _) = request(&state,"PUT",&format!("/internal/v1/upstreams/{}",legacy.id),json!({"tenant_external_id":tenant,"name":"forbidden","expected_updated_at":legacy.updated_at,"config":config})).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let stored = state
                .db
                .upstream_account_with_current_credential(
                    legacy.id,
                    state.config.key_pepper.as_bytes(),
                )
                .await
                .unwrap()
                .0;
            assert_eq!(stored.updated_at, legacy.updated_at);
            assert_eq!(stored.name, "legacy");
        } else {
            assert_eq!(status, StatusCode::CREATED);
            assert_eq!(created["config"], config);
            let (status, updated) = request(&state,"PUT",&format!("/internal/v1/upstreams/{}",created["id"].as_str().unwrap()),json!({"tenant_external_id":tenant,"name":"updated","expected_updated_at":created["updated_at"],"config":config})).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(updated["config"], config);
            let (status, listed) = request(&state, "GET", &list_path, Value::Null).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(listed.as_array().unwrap().len(), 1);
            assert_eq!(listed[0]["config"], config);
        }
    }
}

#[tokio::test]
async fn provider_adapter_secret_cycles_are_rejected_before_oauth_start_or_reauthorization() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("oauth-secret-gate.db").display()
    );
    let mut state = AppState::initialize(Config::for_test(database_url.clone()))
        .await
        .unwrap();
    let mock = wiremock::MockServer::start().await;
    let pool = sqlx::AnyPool::connect(&database_url).await.unwrap();
    // These reserved URLs are only syntax-checked into a login URL. This test
    // never polls or makes a provider call; the account endpoint is a mock.
    let adapter = OAuthAdapterContribution {
        api_version: "oauth-adapter-v1".into(),
        flow_kind: OAuthFlowKind::CursorPkce,
        login_url: "https://provider.example/login".into(),
        poll_url: "https://provider.example/poll".into(),
        refresh_url: "https://provider.example/refresh".into(),
    };
    let config = json!({"base_url":mock.uri(),"tree":{"next":{}}});
    for secret in [false, true] {
        let mut provider = state.providers.get("http-json").unwrap().clone();
        provider.id = format!("oauth-recursive-{secret}");
        provider.oauth_adapter = Some(adapter.clone());
        provider.config_schema = json!({"type":"object","$defs":{"node":{"type":"object","properties":{"token":{"type":"string","writeOnly":secret},"next":{"$ref":"#/$defs/node"}}}},"properties":{"base_url":{"type":"string"},"tree":{"$ref":"#/$defs/node"}}});
        let driver = provider.id.clone();
        state.providers.extend([provider]).unwrap();
        let (status, _) = request(&state,"POST","/internal/v1/oauth/provider-adapter/start",json!({"tenant_external_id":"oauth-secret-test","account_name":"new","provider_driver":driver,"provider_config":config})).await;
        assert_eq!(
            status,
            if secret {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::OK
            }
        );
        let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_login_sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            sessions, 1,
            "only the non-secret positive control may create an OAuth session"
        );
        if secret {
            let legacy = state
                .db
                .create_upstream_account(
                    CreateUpstreamAccountInput {
                        tenant_external_id: "oauth-secret-test".into(),
                        name: "legacy".into(),
                        driver: driver.clone(),
                        config: config.clone(),
                        credential: UpstreamCredential::OAuth {
                            access_token: "synthetic-access".into(),
                            refresh_token: Some("synthetic-refresh".into()),
                            expires_at: Some(memeloop_token_center::db::unix_millis() + 3_600_000),
                            header: "authorization".into(),
                            prefix: "Bearer ".into(),
                            adapter_state: None,
                            proxy_url: None,
                            proxy_network_scope: None,
                        },
                        oauth_session_id: None,
                        oauth_driver: Some("provider_adapter".into()),
                        oauth_refresh_url: Some(adapter.refresh_url.clone()),
                    },
                    state.config.key_pepper.as_bytes(),
                )
                .await
                .unwrap();
            let (status, _) = request(&state,"POST","/internal/v1/oauth/provider-adapter/start",json!({"tenant_external_id":"oauth-secret-test","account_name":"legacy","provider_driver":driver,"provider_config":config,"upstream_account_id":legacy.id})).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_login_sessions")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(sessions, 1);
            let stored = state
                .db
                .upstream_account_with_current_credential(
                    legacy.id,
                    state.config.key_pepper.as_bytes(),
                )
                .await
                .unwrap()
                .0;
            assert_eq!(stored.updated_at, legacy.updated_at);
            assert_eq!(stored.credential_generation, legacy.credential_generation);
        }
    }
}

#[tokio::test]
async fn provider_adapter_reauthorization_restores_only_the_current_secret_config() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("oauth-secret-restore.db").display()
    );
    let mut state = AppState::initialize(Config::for_test(database_url.clone()))
        .await
        .unwrap();
    let pool = sqlx::AnyPool::connect(&database_url).await.unwrap();
    let adapter = OAuthAdapterContribution {
        api_version: "oauth-adapter-v1".into(),
        flow_kind: OAuthFlowKind::CursorPkce,
        login_url: "https://provider.example/login".into(),
        poll_url: "https://provider.example/poll".into(),
        refresh_url: "https://provider.example/refresh".into(),
    };
    let mut provider = state.providers.get("http-json").unwrap().clone();
    provider.id = "oauth-static-secret".into();
    provider.oauth_adapter = Some(adapter.clone());
    provider.config_schema = json!({
        "type":"object", "additionalProperties":false,
        "required":["base_url","client_secret"],
        "properties":{
            "base_url":{"type":"string"},
            "label":{"type":"string"},
            "client_secret":{"type":"string","minLength":1,"writeOnly":true}
        }
    });
    state.providers.extend([provider]).unwrap();
    let tenant = "oauth-static-secret-test";
    let config = json!({
        "base_url": "https://provider.example/api",
        "label": "unchanged",
        "client_secret": "synthetic-current-secret"
    });
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.into(),
                name: "reauthorize-me".into(),
                driver: "oauth-static-secret".into(),
                config: config.clone(),
                credential: UpstreamCredential::OAuth {
                    access_token: "synthetic-access".into(),
                    refresh_token: Some("synthetic-refresh".into()),
                    expires_at: Some(memeloop_token_center::db::unix_millis() + 3_600_000),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: None,
                    proxy_url: Some("socks5h://127.0.0.1:1080".into()),
                    proxy_network_scope: Some(
                        memeloop_token_center::network::OutboundScope::Private,
                    ),
                },
                oauth_session_id: Some(Uuid::now_v7()),
                oauth_driver: Some("provider_adapter".into()),
                oauth_refresh_url: Some(adapter.refresh_url),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let (status, listed) = request(
        &state,
        "GET",
        &format!("/internal/v1/upstreams?tenant_external_id={tenant}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let public_config = json!({"base_url":"https://provider.example/api","label":"unchanged"});
    assert_eq!(listed[0]["config"], public_config);
    assert!(!listed.to_string().contains("synthetic-current-secret"));

    let start = |provider_config: Value| {
        json!({
            "tenant_external_id":tenant,
            "account_name":account.name,
            "provider_driver":account.driver,
            "provider_config":provider_config,
            "upstream_account_id":account.id
        })
    };
    let tenant_service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "tenant-oauth-writer".into(),
                scopes: vec!["oauth:write".into()],
                tenant_external_id: Some(tenant.into()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let (status, started) = request_with_token(
        &state,
        "POST",
        "/internal/v1/oauth/provider-adapter/start",
        start(public_config.clone()),
        &tenant_service.token,
    )
    .await;
    let started_diagnostic = started.to_string();
    assert!(!started_diagnostic.contains("synthetic-"));
    assert_eq!(
        status,
        StatusCode::OK,
        "sanitized response: {started_diagnostic}"
    );
    assert!(started["session_token"].is_string());

    for tampered in [
        json!({
            "base_url":"https://provider.example/api", "label":"unchanged",
            "client_secret":"synthetic-replacement"
        }),
        json!({"base_url":"https://provider.example/api","label":"changed"}),
    ] {
        let (status, rejected) = request(
            &state,
            "POST",
            "/internal/v1/oauth/provider-adapter/start",
            start(tampered),
        )
        .await;
        let rejected_diagnostic = rejected.to_string();
        assert!(!rejected_diagnostic.contains("synthetic-"));
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "sanitized response: {rejected_diagnostic}"
        );
    }
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_login_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        sessions, 1,
        "tampered configuration cannot create a login session"
    );
    let stored = state
        .db
        .upstream_account_with_current_credential(account.id, state.config.key_pepper.as_bytes())
        .await
        .unwrap()
        .0;
    assert_eq!(stored.config, config);
    assert_eq!(stored.updated_at, account.updated_at);
    assert_eq!(stored.credential_generation, account.credential_generation);
}

#[tokio::test]
async fn account_secret_config_is_write_only_preserved_and_compare_and_swap_fenced() {
    let directory = tempfile::tempdir().unwrap();
    let mut state = AppState::initialize(Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("secrets.db").display()
    )))
    .await
    .unwrap();
    let mock = wiremock::MockServer::start().await;
    let mut provider = state.providers.get("http-json").unwrap().clone();
    provider.id = "secret-config-fixture".into();
    provider.config_schema = json!({
        "type":"object", "additionalProperties":false, "required":["base_url","nested"],
        "$defs":{"secret":{"type":"string","minLength":1,"writeOnly":true}},
        "properties":{
            "base_url":{"type":"string"},
            "rows":{"type":"array","items":{"type":"object","properties":{"token":{"$ref":"#/$defs/secret"}}}},
            "nested":{"type":"object","additionalProperties":false,"required":["token"],"properties":{
                "token":{"allOf":[{"$ref":"#/$defs/secret"}]},"label":{"type":"string"}
            }}
        }
    });
    state.providers.extend([provider]).unwrap();
    let config = json!({"base_url":mock.uri(),"nested":{"token":"synthetic-old","label":"before"},"rows":[{"token":"synthetic-array"}]});
    let (status, created) = request(&state,"POST","/internal/v1/upstreams",json!({
        "tenant_external_id":"secret-config-test","name":"fixture","driver":"secret-config-fixture",
        "config":config,"credential":{"type":"api_key","value":"synthetic-credential"}
    })).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(!created.to_string().contains("synthetic-"));
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let path = format!("/internal/v1/upstreams/{id}");
    let update = |revision: Value, config: Value| json!({"tenant_external_id":"secret-config-test","name":"edited","expected_updated_at":revision,"config":config});
    let (status, preserved) = request(
        &state,
        "PUT",
        &path,
        update(
            created["updated_at"].clone(),
            json!({"base_url":mock.uri(),"nested":{"label":"after"}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(preserved["config"]["nested"].get("token").is_none());
    let stored = state
        .db
        .upstream_account_with_current_credential(id, state.config.key_pepper.as_bytes())
        .await
        .unwrap()
        .0;
    assert!(stored.config["nested"]["token"] == "synthetic-old");
    assert!(stored.config["rows"][0]["token"] == "synthetic-array");
    for invalid in [Value::Null, json!(""), json!({})] {
        let (status, _) = request(
            &state,
            "PUT",
            &path,
            update(
                preserved["updated_at"].clone(),
                json!({"base_url":mock.uri(),"nested":{"token":invalid}}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let (status, _) = request(
        &state,
        "PUT",
        &path,
        update(
            preserved["updated_at"].clone(),
            json!({"base_url":mock.uri(),"nested":{"unknown":"not-merged"}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let first = update(
        preserved["updated_at"].clone(),
        json!({"base_url":mock.uri(),"nested":{"token":"synthetic-new"}}),
    );
    let second = update(
        preserved["updated_at"].clone(),
        json!({"base_url":mock.uri(),"nested":{"token":"synthetic-other"}}),
    );
    let (a, b) = tokio::join!(
        request(&state, "PUT", &path, first),
        request(&state, "PUT", &path, second)
    );
    assert!(
        (a.0 == StatusCode::OK && b.0 == StatusCode::CONFLICT)
            || (b.0 == StatusCode::OK && a.0 == StatusCode::CONFLICT)
    );
    assert!(!a.1.to_string().contains("synthetic-"));
    assert!(!b.1.to_string().contains("synthetic-"));
    let stored = state
        .db
        .upstream_account_with_current_credential(id, state.config.key_pepper.as_bytes())
        .await
        .unwrap()
        .0;
    assert!(matches!(
        stored.config["nested"]["token"].as_str(),
        Some("synthetic-new" | "synthetic-other")
    ));
    let (status, listed) = request(
        &state,
        "GET",
        "/internal/v1/upstreams?tenant_external_id=secret-config-test",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!listed.to_string().contains("synthetic-"));
}

#[tokio::test]
async fn dynamic_and_conditional_account_config_responses_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let mut state = AppState::initialize(Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("dynamic-secrets.db").display()
    )))
    .await
    .unwrap();
    let mock = wiremock::MockServer::start().await;
    for (index, shape) in [
        json!({"type":"object","additionalProperties":{"type":"object","properties":{"token":{"type":"string","writeOnly":true}}}}),
        json!({"type":"object","if":{"properties":{"mode":{"const":"private"}}},"then":{"properties":{"token":{"type":"string","writeOnly":true}}}}),
    ].into_iter().enumerate() {
        let mut provider = state.providers.get("http-json").unwrap().clone();
        provider.id = format!("dynamic-secret-{index}");
        provider.config_schema = json!({"type":"object","properties":{"base_url":{"type":"string"},"settings":shape}});
        let driver = provider.id.clone();
        state.providers.extend([provider]).unwrap();
        let settings = if index == 0 { json!({"entry":{"token":"synthetic-dynamic"}}) } else { json!({"mode":"private","token":"synthetic-conditional"}) };
        let config = json!({"base_url":mock.uri(),"settings":settings});
        let (status, created) = request(&state,"POST","/internal/v1/upstreams",json!({"tenant_external_id":"dynamic-secret-test","name":format!("fixture-{index}"),"driver":driver,"config":config,"credential":{"type":"api_key","value":"synthetic-credential"}})).await;
        assert_eq!(status,StatusCode::CREATED);
        assert_eq!(created["config"],json!({}));
        let id = created["id"].as_str().unwrap();
        let (status, rejected) = request(&state,"PUT",&format!("/internal/v1/upstreams/{id}"),json!({"tenant_external_id":"dynamic-secret-test","name":"edited","expected_updated_at":created["updated_at"],"config":config})).await;
        assert_eq!(status,StatusCode::BAD_REQUEST);
        assert!(!rejected.to_string().contains("synthetic-"));
    }
    let (status, listed) = request(
        &state,
        "GET",
        "/internal/v1/upstreams?tenant_external_id=dynamic-secret-test",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .all(|account| account["config"] == json!({}))
    );
}
