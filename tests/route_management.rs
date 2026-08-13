use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{
        CreateKeyInput, CreateModelRouteInput, CreateServiceTokenInput, CreateUpstreamAccountInput,
        NewRequest,
    },
    model::KeyPolicy,
    provider::{ModelRouteView, UpstreamCredential},
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

async fn json_request(
    state: &AppState,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
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

fn route_from(value: Value) -> ModelRouteView {
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn route_mutations_are_scoped_optimistic_idempotent_and_history_safe() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("route-management.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap();
    let pepper = state.config.key_pepper.as_bytes();
    let key = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "route-tenant-a".into(),
                principal_external_id: "member-a".into(),
                alias: "route-history".into(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    allowed_models: vec!["*".into()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "route-tenant-b".into(),
                principal_external_id: "member-b".into(),
                alias: "tenant-anchor".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let upstream = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "route-tenant-a".into(),
                name: "unified-api-or-oauth-upstream".into(),
                driver: "http-json".into(),
                config: json!({"base_url": "https://upstream.example.test"}),
                credential: UpstreamCredential::ApiKey {
                    value: "not-a-real-secret".into(),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                },
                oauth_session_id: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: "route-tenant-a".into(),
            public_model: "public-a".into(),
            upstream_account_id: upstream.id,
            upstream_model: "upstream-a".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    let tenant_service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "route-manager-a".into(),
                scopes: vec!["routes:read".into(), "routes:write".into()],
                tenant_external_id: Some("route-tenant-a".into()),
            },
            pepper,
        )
        .await
        .unwrap();
    let read_only_service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "route-reader-a".into(),
                scopes: vec!["routes:read".into()],
                tenant_external_id: Some("route-tenant-a".into()),
            },
            pepper,
        )
        .await
        .unwrap();

    let (status, _) = json_request(
        &state,
        "PATCH",
        &format!("/internal/v1/model-routes/{}", route.id),
        &tenant_service.token,
        Some(json!({
            "tenant_external_id": "route-tenant-b",
            "enabled": false,
            "expected_updated_at": route.updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = json_request(
        &state,
        "PATCH",
        &format!("/internal/v1/model-routes/{}", route.id),
        &read_only_service.token,
        Some(json!({
            "tenant_external_id": "route-tenant-a",
            "enabled": false,
            "expected_updated_at": route.updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let update = json!({
        "tenant_external_id": "route-tenant-a",
        "public_model": "public-a-v2",
        "upstream_account_id": upstream.id,
        "upstream_model": "upstream-a-v2",
        "protocol": "openai",
        "priority": 10,
        "expected_updated_at": route.updated_at
    });
    let (status, value) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/model-routes/{}", route.id),
        &tenant_service.token,
        Some(update.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let updated = route_from(value);
    assert!(updated.updated_at > route.updated_at);

    let (status, replay) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/model-routes/{}", route.id),
        &tenant_service.token,
        Some(update),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(route_from(replay).updated_at, updated.updated_at);

    let (status, _) = json_request(
        &state,
        "PUT",
        &format!("/internal/v1/model-routes/{}", route.id),
        &tenant_service.token,
        Some(json!({
            "tenant_external_id": "route-tenant-a",
            "public_model": "stale-change",
            "upstream_account_id": upstream.id,
            "upstream_model": "upstream-a-v3",
            "protocol": "openai",
            "priority": 11,
            "expected_updated_at": route.updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, value) = json_request(
        &state,
        "PATCH",
        &format!("/internal/v1/model-routes/{}", route.id),
        &tenant_service.token,
        Some(json!({
            "tenant_external_id": "route-tenant-a",
            "enabled": false,
            "expected_updated_at": updated.updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let disabled = route_from(value);

    let (status, _) = json_request(
        &state,
        "DELETE",
        &format!(
            "/internal/v1/model-routes/{}?tenant_external_id=route-tenant-a&expected_updated_at={}",
            route.id, disabled.updated_at
        ),
        &tenant_service.token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = json_request(
        &state,
        "DELETE",
        &format!(
            "/internal/v1/model-routes/{}?tenant_external_id=route-tenant-a&expected_updated_at={}",
            route.id, disabled.updated_at
        ),
        &tenant_service.token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let global_create = json!({
        "tenant_external_id": "route-tenant-a",
        "public_model": "global-managed-public",
        "upstream_account_id": upstream.id,
        "upstream_model": "global-managed-upstream",
        "protocol": "openai",
        "priority": 12
    });
    let (status, value) = json_request(
        &state,
        "POST",
        "/internal/v1/model-routes",
        &state.config.service_token,
        Some(global_create.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let global_route = route_from(value);
    let (status, replay) = json_request(
        &state,
        "POST",
        "/internal/v1/model-routes",
        &state.config.service_token,
        Some(global_create),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(route_from(replay).id, global_route.id);
    let (status, value) = json_request(
        &state,
        "PATCH",
        &format!("/internal/v1/model-routes/{}", global_route.id),
        &state.config.service_token,
        Some(json!({
            "tenant_external_id": "route-tenant-a",
            "enabled": false,
            "expected_updated_at": global_route.updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let global_route = route_from(value);
    let (status, _) = json_request(
        &state,
        "DELETE",
        &format!(
            "/internal/v1/model-routes/{}?tenant_external_id=route-tenant-a&expected_updated_at={}",
            global_route.id, global_route.updated_at
        ),
        &state.config.service_token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let historical_route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: "route-tenant-a".into(),
            public_model: "historical-public".into(),
            upstream_account_id: upstream.id,
            upstream_model: "historical-upstream".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    let authenticated_key = state.db.authenticate_key(&key.key, pepper).await.unwrap();
    let price = state
        .db
        .upsert_model_price("historical-public", "USD", Decimal::ZERO, Decimal::ZERO)
        .await
        .unwrap();
    let reservation = state
        .db
        .reserve_usage(&authenticated_key, &price, 0, 0)
        .await
        .unwrap();
    state
        .db
        .record_request_started(NewRequest {
            request_id: Uuid::now_v7(),
            key_id: authenticated_key.key_id,
            tenant_id: authenticated_key.tenant_id,
            protocol: "openai-chat".into(),
            model: "historical-public".into(),
            request_object: "inline-json:{}".into(),
            reservation_id: reservation.id,
            upstream_account_id: Some(upstream.id),
            model_route_id: Some(historical_route.id),
        })
        .await
        .unwrap();
    let (status, value) = json_request(
        &state,
        "PATCH",
        &format!("/internal/v1/model-routes/{}", historical_route.id),
        &tenant_service.token,
        Some(json!({
            "tenant_external_id": "route-tenant-a",
            "enabled": false,
            "expected_updated_at": historical_route.updated_at
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let historical_route = route_from(value);
    let (status, _) = json_request(
        &state,
        "DELETE",
        &format!(
            "/internal/v1/model-routes/{}?tenant_external_id=route-tenant-a&expected_updated_at={}",
            historical_route.id, historical_route.updated_at
        ),
        &tenant_service.token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}
