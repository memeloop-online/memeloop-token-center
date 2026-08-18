use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{CreateModelRouteInput, CreateServiceTokenInput, CreateUpstreamAccountInput},
    provider::{UpstreamAccountView, UpstreamCredential},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header as matches_header, method, path},
};

async fn json_request(
    state: &AppState,
    method_name: &str,
    path: &str,
    token: &str,
    idempotency_key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method_name)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(idempotency_key) = idempotency_key {
        builder = builder.header("idempotency-key", idempotency_key);
    }
    let body = if let Some(body) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(serde_json::to_vec(&body).unwrap())
    } else {
        Body::empty()
    };
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

fn account(value: Value) -> UpstreamAccountView {
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn oauth_refresh_lease_is_account_generation_scoped_and_stale_safe() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("oauth-refresh-lease.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap();
    let pepper = state.config.key_pepper.as_bytes();
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "oauth-lease-tenant".into(),
                name: "managed-oauth".into(),
                driver: "http-json".into(),
                config: json!({"base_url": "https://api.example.test"}),
                credential: UpstreamCredential::OAuth {
                    access_token: "access-v1".into(),
                    refresh_token: Some("refresh-v1".into()),
                    expires_at: Some(memeloop_token_center::db::unix_millis() + 60_000),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: None,
                },
                oauth_session_id: Some(Uuid::now_v7()),
                oauth_driver: Some("cursor".into()),
                oauth_refresh_url: Some("https://oauth.example.test/refresh".into()),
            },
            pepper,
        )
        .await
        .unwrap();
    assert!(account.can_refresh);
    assert!(account.config.get("oauth").is_none());

    assert!(
        state
            .db
            .begin_upstream_oauth_refresh(account.id, "refresh-lease-a", pepper)
            .await
            .unwrap()
            .is_none()
    );
    let concurrent = state
        .db
        .begin_upstream_oauth_refresh(account.id, "refresh-lease-b", pepper)
        .await
        .unwrap_err();
    assert!(matches!(
        concurrent,
        memeloop_token_center::error::AppError::Conflict(_)
    ));

    let manually_rotated = state
        .db
        .rotate_upstream_credential(
            account.id,
            UpstreamCredential::OAuth {
                access_token: "access-manual".into(),
                refresh_token: Some("refresh-manual".into()),
                expires_at: Some(memeloop_token_center::db::unix_millis() + 120_000),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
                adapter_state: None,
            },
            "manual-during-refresh",
            pepper,
        )
        .await
        .unwrap();
    assert_eq!(manually_rotated.credential_generation, 2);
    let stale = state
        .db
        .finish_upstream_oauth_refresh(
            account.id,
            UpstreamCredential::OAuth {
                access_token: "stale-access".into(),
                refresh_token: Some("stale-refresh".into()),
                expires_at: Some(memeloop_token_center::db::unix_millis() + 3_600_000),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
                adapter_state: None,
            },
            "refresh-lease-a",
            pepper,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        stale,
        memeloop_token_center::error::AppError::Conflict(_)
    ));
    state
        .db
        .abort_upstream_oauth_refresh(account.id, "refresh-lease-a")
        .await
        .unwrap();

    assert!(
        state
            .db
            .begin_upstream_oauth_refresh(account.id, "refresh-lease-b", pepper)
            .await
            .unwrap()
            .is_none()
    );
    let refreshed = state
        .db
        .finish_upstream_oauth_refresh(
            account.id,
            UpstreamCredential::OAuth {
                access_token: "access-v3".into(),
                refresh_token: Some("refresh-v3".into()),
                expires_at: Some(memeloop_token_center::db::unix_millis() + 3_600_000),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
                adapter_state: None,
            },
            "refresh-lease-b",
            pepper,
        )
        .await
        .unwrap();
    assert_eq!(refreshed.credential_generation, 3);
    assert_eq!(refreshed.id, account.id);
}

#[tokio::test]
async fn oauth_refresh_finalize_failure_recovers_pending_ciphertext_without_remote_replay() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("oauth-finalize-fault.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url.clone()))
        .await
        .unwrap();
    let pepper = state.config.key_pepper.as_bytes();
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "oauth-fault-tenant".into(),
                name: "managed-oauth-fault".into(),
                driver: "http-json".into(),
                config: json!({"base_url": "https://api.example.test"}),
                credential: UpstreamCredential::OAuth {
                    access_token: "access-before-fault".into(),
                    refresh_token: Some("refresh-before-fault".into()),
                    expires_at: Some(memeloop_token_center::db::unix_millis() + 60_000),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: None,
                },
                oauth_session_id: Some(Uuid::now_v7()),
                oauth_driver: Some("cursor".into()),
                oauth_refresh_url: Some("https://oauth.example.test/refresh".into()),
            },
            pepper,
        )
        .await
        .unwrap();
    let idempotency_key = "refresh-finalize-fault";
    assert!(
        state
            .db
            .begin_upstream_oauth_refresh(account.id, idempotency_key, pepper)
            .await
            .unwrap()
            .is_none()
    );

    sqlx::any::install_default_drivers();
    let fault_pool = sqlx::AnyPool::connect(&database_url).await.unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER inject_oauth_finalize_failure BEFORE UPDATE OF credential_generation ON upstream_accounts WHEN NEW.id = '{}' BEGIN SELECT RAISE(ABORT, 'injected OAuth finalize failure'); END",
        account.id
    ))
    .execute(&fault_pool)
    .await
    .unwrap();
    let refreshed_credential = UpstreamCredential::OAuth {
        access_token: "access-after-remote-success".into(),
        refresh_token: Some("refresh-after-remote-success".into()),
        expires_at: Some(memeloop_token_center::db::unix_millis() + 3_600_000),
        header: "authorization".into(),
        prefix: "Bearer ".into(),
        adapter_state: None,
    };
    let failed = state
        .db
        .finish_upstream_oauth_refresh(account.id, refreshed_credential, idempotency_key, pepper)
        .await
        .unwrap_err();
    assert!(matches!(
        failed,
        memeloop_token_center::error::AppError::Internal
    ));
    sqlx::query("DROP TRIGGER inject_oauth_finalize_failure")
        .execute(&fault_pool)
        .await
        .unwrap();

    // Retrying the same key finalizes the durable encrypted pending result.
    // No authorization-server call or plaintext token is needed here.
    let recovered = state
        .db
        .begin_upstream_oauth_refresh(account.id, idempotency_key, pepper)
        .await
        .unwrap()
        .expect("pending OAuth result finalized");
    assert_eq!(recovered.id, account.id);
    assert_eq!(recovered.credential_generation, 2);
    let replay = state
        .db
        .begin_upstream_oauth_refresh(account.id, idempotency_key, pepper)
        .await
        .unwrap()
        .expect("committed result replayed exactly");
    assert_eq!(replay.id, recovered.id);
    assert_eq!(
        replay.credential_generation,
        recovered.credential_generation
    );
}

#[tokio::test]
async fn unified_upstream_management_is_scoped_optimistic_and_history_safe() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(matches_header("authorization", "Bearer original-secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [], "never_return": "original-secret"})),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("upstream-management.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap();
    let pepper = state.config.key_pepper.as_bytes();
    let upstream = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "tenant-upstream-a".into(),
                name: "primary-openai".into(),
                driver: "http-json".into(),
                config: json!({"base_url": mock.uri()}),
                credential: UpstreamCredential::ApiKey {
                    value: "original-secret".into(),
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
    // Anchor a second tenant so a tenant-scoped operator cannot smuggle a
    // different tenant ID into a path-addressed mutation.
    state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "tenant-upstream-b".into(),
                name: "tenant-b-anchor".into(),
                driver: "http-json".into(),
                config: json!({"base_url": mock.uri()}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: "tenant-upstream-a".into(),
            public_model: "public-model".into(),
            upstream_account_id: upstream.id,
            upstream_model: "provider-model".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    let service_a = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "provider-manager-a".into(),
                scopes: vec!["providers:read".into(), "providers:write".into()],
                tenant_external_id: Some("tenant-upstream-a".into()),
            },
            pepper,
        )
        .await
        .unwrap();
    let service_b = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "provider-manager-b".into(),
                scopes: vec!["providers:write".into()],
                tenant_external_id: Some("tenant-upstream-b".into()),
            },
            pepper,
        )
        .await
        .unwrap();

    let (status, listed) = json_request(
        &state,
        "GET",
        "/internal/v1/upstreams?tenant_external_id=tenant-upstream-a",
        &service_a.token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed = account(listed.as_array().unwrap()[0].clone());
    assert_eq!(listed.connection_method, "api_key");
    assert_eq!(listed.route_count, 1);

    let (status, health) = json_request(
        &state,
        "POST",
        &format!(
            "/internal/v1/upstreams/{}/health?tenant_external_id=tenant-upstream-a",
            upstream.id
        ),
        &service_a.token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["status"], "healthy");
    assert_eq!(health["upstream_status"], 200);
    assert!(!health.to_string().contains("original-secret"));
    assert!(!health.to_string().contains("never_return"));

    let (status, _) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/upstreams/{}", upstream.id),
        &service_b.token,
        None,
        Some(json!({
            "tenant_external_id": "tenant-upstream-b",
            "name": "cross-tenant-change",
            "config": {"base_url": mock.uri()},
            "expected_updated_at": upstream.updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/upstreams/{}", upstream.id),
        &service_a.token,
        None,
        Some(json!({
            "tenant_external_id": "tenant-upstream-a",
            "name": "blocked-private-target",
            "config": {"base_url": "http://10.0.0.1:8080"},
            "expected_updated_at": upstream.updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, updated) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/upstreams/{}", upstream.id),
        &service_a.token,
        None,
        Some(json!({
            "tenant_external_id": "tenant-upstream-a",
            "name": "primary-openai-renamed",
            "config": {"base_url": mock.uri(), "timeout_seconds": 30},
            "expected_updated_at": upstream.updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let updated = account(updated);
    assert!(updated.updated_at > upstream.updated_at);

    let (status, _) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/upstreams/{}", upstream.id),
        &service_a.token,
        None,
        Some(json!({
            "tenant_external_id": "tenant-upstream-a",
            "name": "stale-name",
            "config": {"base_url": mock.uri()},
            "expected_updated_at": upstream.updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let rotation = json!({"credential": {
        "type": "api_key", "value": "rotated-secret",
        "header": "authorization", "prefix": "Bearer "
    }});
    let (status, rotated) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/upstreams/{}/credential", upstream.id),
        &service_a.token,
        Some("upstream-rotation-1"),
        Some(rotation.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rotated = account(rotated);
    assert_eq!(rotated.id, upstream.id);
    assert_eq!(rotated.credential_generation, 2);
    let (status, replayed) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/upstreams/{}/credential", upstream.id),
        &service_a.token,
        Some("upstream-rotation-1"),
        Some(rotation),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(account(replayed).credential_generation, 2);
    let (status, _) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/upstreams/{}/credential", upstream.id),
        &service_a.token,
        Some("upstream-rotation-1"),
        Some(json!({"credential": {
            "type": "api_key", "value": "different-secret"
        }})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/upstreams/{}/credential", upstream.id),
        &service_a.token,
        Some("invalid-api-to-oauth"),
        Some(json!({"credential": {
            "type": "oauth", "access_token": ""
        }})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let unchanged = state
        .db
        .list_upstream_accounts("tenant-upstream-a")
        .await
        .unwrap()
        .into_iter()
        .find(|account| account.id == upstream.id)
        .unwrap();
    assert_eq!(unchanged.auth_kind, "api_key");
    assert_eq!(unchanged.credential_generation, 2);
    assert_eq!(unchanged.route_count, 1);

    let (status, oauth) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/upstreams/{}/credential", upstream.id),
        &service_a.token,
        Some("upstream-api-to-oauth"),
        Some(json!({"credential": {
            "type": "oauth",
            "access_token": "oauth-access-secret",
            "refresh_token": "oauth-refresh-secret",
            "expires_at": memeloop_token_center::db::unix_millis() + 3_600_000
        }})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let oauth = account(oauth);
    assert_eq!(oauth.id, upstream.id);
    assert_eq!(oauth.auth_kind, "oauth");
    assert_eq!(oauth.connection_method, "oauth");
    assert_eq!(oauth.credential_generation, 3);
    assert_eq!(oauth.route_count, 1);
    assert!(oauth.can_rotate);
    assert!(
        !oauth.can_refresh,
        "manual OAuth has no managed refresh lifecycle"
    );

    let (status, rotated) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/upstreams/{}/credential", upstream.id),
        &service_a.token,
        Some("upstream-oauth-to-api"),
        Some(json!({"credential": {
            "type": "api_key", "value": "final-api-secret"
        }})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rotated = account(rotated);
    assert_eq!(rotated.id, upstream.id);
    assert_eq!(rotated.auth_kind, "api_key");
    assert_eq!(rotated.credential_generation, 4);
    assert_eq!(rotated.route_count, 1);
    assert!(rotated.can_rotate);
    assert!(!rotated.can_refresh);

    let (status, disabled) = json_request(
        &state,
        "PATCH",
        &format!("/internal/v1/upstreams/{}", upstream.id),
        &service_a.token,
        None,
        Some(json!({
            "tenant_external_id": "tenant-upstream-a",
            "status": "disabled",
            "expected_updated_at": rotated.updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let disabled = account(disabled);
    let tenant_id = disabled.tenant_id;
    assert!(
        state
            .db
            .resolve_upstream(
                tenant_id,
                "public-model",
                "openai",
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap()
            .is_none()
    );
    let (status, _) = json_request(
        &state,
        "DELETE",
        &format!(
            "/internal/v1/upstreams/{}?tenant_external_id=tenant-upstream-a&expected_updated_at={}",
            upstream.id, disabled.updated_at
        ),
        &service_a.token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let disabled_route = state
        .db
        .set_model_route_enabled(route.id, "tenant-upstream-a", false, route.updated_at)
        .await
        .unwrap();
    state
        .db
        .delete_model_route(route.id, "tenant-upstream-a", disabled_route.updated_at)
        .await
        .unwrap();
    let (status, body) = json_request(
        &state,
        "DELETE",
        &format!(
            "/internal/v1/upstreams/{}?tenant_external_id=tenant-upstream-a&expected_updated_at={}",
            upstream.id, disabled.updated_at
        ),
        &service_a.token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(body, Value::Null);
}

#[tokio::test]
async fn global_operator_update_still_requires_the_resource_tenant_and_supported_schema() {
    let mock = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("upstream-global.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap();
    let upstream = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "global-managed-tenant".into(),
                name: "global-managed".into(),
                driver: "http-json".into(),
                config: json!({"base_url": mock.uri()}),
                credential: UpstreamCredential::None,
                oauth_session_id: Some(Uuid::now_v7()),
                oauth_driver: Some("cursor".into()),
                oauth_refresh_url: Some("https://api2.cursor.sh/auth/exchange_user_api_key".into()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();

    let (status, _) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/upstreams/{}", upstream.id),
        &state.config.service_token,
        None,
        Some(json!({
            "tenant_external_id": "wrong-tenant",
            "name": "not-written",
            "config": {"base_url": mock.uri()},
            "expected_updated_at": upstream.updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/upstreams/{}", upstream.id),
        &state.config.service_token,
        None,
        Some(json!({
            "tenant_external_id": "global-managed-tenant",
            "name": "not-written",
            "config": {"base_url": mock.uri(), "credential": "must-not-live-in-config"},
            "expected_updated_at": upstream.updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
