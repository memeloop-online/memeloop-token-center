use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{CreateModelRouteInput, CreateServiceTokenInput, CreateUpstreamAccountInput},
    provider::UpstreamCredential,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    state: AppState,
    database_url: String,
    _directory: tempfile::TempDir,
    tenant: String,
    route_id: Uuid,
    source_id: Uuid,
    target_id: Uuid,
    route_updated_at: i64,
    source_updated_at: i64,
    target_updated_at: i64,
    token: String,
}

async fn fixture(tenant: &str) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory
            .path()
            .join("ledger-resource-route-switch.db")
            .display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap();
    let create_account = |name: &str| CreateUpstreamAccountInput {
        tenant_external_id: tenant.to_owned(),
        name: name.to_owned(),
        driver: "http-json".to_owned(),
        config: json!({"base_url": "https://provider.example.test"}),
        credential: UpstreamCredential::None,
        oauth_session_id: None,
        oauth_driver: None,
        oauth_refresh_url: None,
    };
    let source = state
        .db
        .create_upstream_account(create_account("source"), state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let target = state
        .db
        .create_upstream_account(create_account("target"), state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.to_owned(),
            public_model: "switchable-model".to_owned(),
            upstream_account_id: source.id,
            upstream_model: "provider-model".to_owned(),
            protocol: "openai".to_owned(),
            priority: 0,
        })
        .await
        .unwrap();
    let token = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "ledger-route-operator".to_owned(),
                scopes: vec!["credits:write".to_owned(), "routes:write".to_owned()],
                tenant_external_id: Some(tenant.to_owned()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    Fixture {
        state,
        database_url,
        _directory: directory,
        tenant: tenant.to_owned(),
        route_id: route.id,
        source_id: source.id,
        target_id: target.id,
        route_updated_at: route.updated_at,
        source_updated_at: source.updated_at,
        target_updated_at: target.updated_at,
        token: token.token,
    }
}

fn plan_body(fixture: &Fixture) -> Value {
    json!({
        "tenant_external_id": fixture.tenant,
        "route_id": fixture.route_id,
        "source_upstream_account_id": fixture.source_id,
        "target_upstream_account_id": fixture.target_id,
        "expected_route_updated_at": fixture.route_updated_at,
        "expected_source_updated_at": fixture.source_updated_at,
        "expected_target_updated_at": fixture.target_updated_at,
        "expected_grant_revision": 0,
    })
}

async fn request_json(
    state: &AppState,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

async fn plan(fixture: &Fixture) -> (StatusCode, Value) {
    request_json(
        &fixture.state,
        "POST",
        "/internal/v1/ledger/resource-route-switches/plan",
        &fixture.token,
        plan_body(fixture),
    )
    .await
}

#[tokio::test]
async fn concurrent_apply_has_one_winner_and_rollback_requires_the_applied_snapshot() {
    let fixture = fixture("resource-switch-tenant").await;
    let (status, operation) = plan(&fixture).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(operation["status"], "planned");
    assert_ne!(operation["before_snapshot"], operation["after_snapshot"]);
    let operation_id = operation["operation_id"].as_str().unwrap();
    let apply_path = format!("/internal/v1/ledger/resource-route-switches/{operation_id}/apply");
    let apply_body = json!({"tenant_external_id": fixture.tenant});
    let (left, right) = tokio::join!(
        request_json(
            &fixture.state,
            "POST",
            &apply_path,
            &fixture.token,
            apply_body.clone()
        ),
        request_json(
            &fixture.state,
            "POST",
            &apply_path,
            &fixture.token,
            apply_body
        ),
    );
    let statuses = [left.0, right.0];
    assert!(statuses.contains(&StatusCode::OK));
    assert!(statuses.contains(&StatusCode::CONFLICT));
    let applied = if left.0 == StatusCode::OK {
        left.1
    } else {
        right.1
    };
    assert_eq!(applied["status"], "applied");
    assert_eq!(
        applied["after_snapshot"]["route"]["upstream_candidates"][0]["upstream_account_id"],
        fixture.target_id.to_string()
    );

    let rollback_path =
        format!("/internal/v1/ledger/resource-route-switches/{operation_id}/rollback");
    let (status, rolled_back) = request_json(
        &fixture.state,
        "POST",
        &rollback_path,
        &fixture.token,
        json!({
            "tenant_external_id": fixture.tenant,
            "expected_route_updated_at": applied["after_snapshot"]["route"]["updated_at"],
            "expected_grant_revision": 0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rolled_back["status"], "rolled_back");
    assert_eq!(
        rolled_back["before_snapshot"]["route"]["upstream_candidates"][0]["upstream_account_id"],
        fixture.source_id.to_string()
    );
}

#[tokio::test]
async fn tenant_and_wildcard_selectors_are_rejected() {
    let fixture = fixture("resource-switch-tenant").await;
    let mut wrong_tenant = plan_body(&fixture);
    wrong_tenant["tenant_external_id"] = json!("another-tenant");
    let (status, _) = request_json(
        &fixture.state,
        "POST",
        "/internal/v1/ledger/resource-route-switches/plan",
        &fixture.token,
        wrong_tenant,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let mut wildcard_tenant = plan_body(&fixture);
    wildcard_tenant["tenant_external_id"] = json!("*");
    let (status, _) = request_json(
        &fixture.state,
        "POST",
        "/internal/v1/ledger/resource-route-switches/plan",
        &fixture.token,
        wildcard_tenant,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let foreign = fixture
        .state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "foreign-resource-tenant".to_owned(),
                name: "foreign-target".to_owned(),
                driver: "http-json".to_owned(),
                config: json!({"base_url": "https://provider.example.test"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let mut foreign_target = plan_body(&fixture);
    foreign_target["target_upstream_account_id"] = json!(foreign.id);
    let (status, _) = request_json(
        &fixture.state,
        "POST",
        "/internal/v1/ledger/resource-route-switches/plan",
        &fixture.token,
        foreign_target,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE model_routes SET public_model = '*' WHERE id = $1")
        .bind(fixture.route_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = plan(&fixture).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn apply_cas_conflict_preserves_planned_operation_and_wrong_tenant_cannot_apply() {
    let fixture = fixture("resource-switch-tenant").await;
    let (status, operation) = plan(&fixture).await;
    assert_eq!(status, StatusCode::OK);
    let operation_id = operation["operation_id"].as_str().unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE model_routes SET updated_at = updated_at + 1 WHERE id = $1")
        .bind(fixture.route_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let path = format!("/internal/v1/ledger/resource-route-switches/{operation_id}/apply");
    let (status, _) = request_json(
        &fixture.state,
        "POST",
        &path,
        &fixture.token,
        json!({"tenant_external_id": fixture.tenant}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = request_json(
        &fixture.state,
        "POST",
        &path,
        &fixture.token,
        json!({"tenant_external_id": "other-tenant"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rollback_rejects_any_intervening_route_edit() {
    let fixture = fixture("resource-switch-tenant").await;
    let (status, operation) = plan(&fixture).await;
    assert_eq!(status, StatusCode::OK);
    let operation_id = operation["operation_id"].as_str().unwrap();
    let (status, applied) = request_json(
        &fixture.state,
        "POST",
        &format!("/internal/v1/ledger/resource-route-switches/{operation_id}/apply"),
        &fixture.token,
        json!({"tenant_external_id": fixture.tenant}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE model_routes SET updated_at = updated_at + 1 WHERE id = $1")
        .bind(fixture.route_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = request_json(
        &fixture.state,
        "POST",
        &format!("/internal/v1/ledger/resource-route-switches/{operation_id}/rollback"),
        &fixture.token,
        json!({
            "tenant_external_id": fixture.tenant,
            "expected_route_updated_at": applied["after_snapshot"]["route"]["updated_at"],
            "expected_grant_revision": 0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}
