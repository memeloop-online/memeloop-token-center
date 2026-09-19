use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{
        CreateModelRouteInput, CreateServiceTokenInput, CreateUpstreamAccountInput,
        DiscoveredUpstreamModel,
    },
    error::AppError,
    provider::UpstreamCredential,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn state() -> (AppState, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("managed.db").display()
    );
    (
        AppState::initialize(Config::for_test(url)).await.unwrap(),
        directory,
    )
}

async fn account(state: &AppState, tenant: &str, base_url: &str) -> Uuid {
    state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.into(),
                name: "managed-sync".into(),
                driver: "http-json".into(),
                config: json!({"base_url": base_url, "network_scope": "private"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap()
        .id
}

async fn request(
    state: &AppState,
    account: Uuid,
    token: &str,
    suffix: &str,
) -> (StatusCode, Value) {
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/internal/v1/upstreams/{account}/models/{suffix}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn serve(server: &MockServer, body: Value) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn models(names: &[&str]) -> Vec<DiscoveredUpstreamModel> {
    names
        .iter()
        .map(|name| DiscoveredUpstreamModel {
            model_id: (*name).into(),
            protocol: "any".into(),
            context_window: None,
            reservation_token_bound: None,
            reservation_bound_source: None,
        })
        .collect()
}

async fn publish(
    state: &AppState,
    account: Uuid,
    tenant: &str,
    names: &[&str],
) -> Vec<DiscoveredUpstreamModel> {
    let lease = Uuid::now_v7();
    assert!(
        state
            .db
            .claim_upstream_model_catalog_sync(account, tenant, 1, lease)
            .await
            .unwrap()
    );
    let models = models(names);
    state
        .db
        .replace_upstream_model_catalog(account, tenant, 1, lease, "openai_v1", &models)
        .await
        .unwrap();
    models
}

#[tokio::test]
async fn explicit_endpoint_creates_owned_candidates_idempotently_without_adopting_manual_routes() {
    let (state, _directory) = state().await;
    let server = MockServer::start().await;
    let tenant = "managed-endpoint";
    let account = account(&state, tenant, &server.uri()).await;
    let manual = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.into(),
            public_model: "model-a".into(),
            upstream_account_id: account,
            upstream_model: "model-a".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    serve(
        &server,
        json!({"data": [{"id": "model-a"}, {"id": "model-b"}]}),
    )
    .await;
    let (status, first) =
        request(&state, account, &state.config.service_token, "sync-routes").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["routes"]["added"], 2);
    assert_eq!(first["price_sync"]["status"], "deferred");
    assert_eq!(first["price_sync"]["imported"], 0);
    let (_, second) = request(&state, account, &state.config.service_token, "sync-routes").await;
    assert_eq!(second["routes"]["added"], 0);
    assert_eq!(second["routes"]["unchanged"], 2);
    let routes = state.db.list_model_routes(Some(tenant)).await.unwrap();
    assert_eq!(
        routes.len(),
        3,
        "same public model may have multiple candidates"
    );
    assert_eq!(
        routes
            .iter()
            .find(|route| route.id == manual.id)
            .unwrap()
            .updated_at,
        manual.updated_at
    );
    for route in routes.iter().filter(|route| route.id != manual.id) {
        let routing = state.db.route_routing(route.id, tenant).await.unwrap();
        assert_eq!(routing.upstream_account_ids, vec![account]);
        assert!(
            routing.granted_credential_ids.is_empty(),
            "sync never grants credentials"
        );
    }
}

#[tokio::test]
async fn failed_partial_and_empty_discovery_never_change_owned_routes_or_catalog_snapshot() {
    let (state, _directory) = state().await;
    let server = MockServer::start().await;
    let tenant = "managed-incomplete";
    let account = account(&state, tenant, &server.uri()).await;
    serve(&server, json!({"data": [{"id": "keep"}]})).await;
    let (status, _) = request(&state, account, &state.config.service_token, "sync-routes").await;
    assert_eq!(status, StatusCode::OK);
    for (body, code) in [
        (
            json!({"data": [{"id": "new"}], "has_more": true}),
            "partial_catalog",
        ),
        (
            json!({"data": [{"id": "new"}], "next_page_token": "page-2"}),
            "partial_catalog",
        ),
        (
            json!({"data": [{"id": "new"}], "total": 2}),
            "partial_catalog",
        ),
        (json!({"data": [{"id": "new"}, {}]}), "invalid_response"),
        (json!({"data": []}), "empty_catalog_protected"),
    ] {
        serve(&server, body).await;
        let (status, result) =
            request(&state, account, &state.config.service_token, "sync-routes").await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["routes"]["added"], 0);
        assert_eq!(result["routes"]["disabled"], 0);
        assert_eq!(result["routes"]["warnings"][0], code);
        assert_eq!(result["catalog"]["models"][0]["id"], "keep");
    }
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let (_, result) = request(&state, account, &state.config.service_token, "sync-routes").await;
    assert_eq!(result["routes"]["warnings"][0], "upstream_unavailable");
    let routes = state.db.list_model_routes(Some(tenant)).await.unwrap();
    assert_eq!(routes.len(), 1);
    assert!(routes[0].enabled);
}

#[tokio::test]
async fn only_catalog_missing_routes_restore_and_noop_manual_disable_is_protected() {
    let (state, _directory) = state().await;
    let tenant = "managed-manual";
    let account = account(&state, tenant, "http://127.0.0.1:18081").await;
    let first = publish(&state, account, tenant, &["restore", "manual", "deleted"]).await;
    state
        .db
        .reconcile_managed_model_routes(account, tenant, 1, &first)
        .await
        .unwrap();
    let missing = publish(&state, account, tenant, &["other"]).await;
    let result = state
        .db
        .reconcile_managed_model_routes(account, tenant, 1, &missing)
        .await
        .unwrap();
    assert_eq!(result.disabled, 3);
    let routes = state.db.list_model_routes(Some(tenant)).await.unwrap();
    let manual = routes
        .iter()
        .find(|route| route.public_model == "manual")
        .unwrap();
    assert!(!manual.enabled);
    state
        .db
        .set_model_route_enabled(manual.id, tenant, false, manual.updated_at)
        .await
        .unwrap();
    let deleted = routes
        .iter()
        .find(|route| route.public_model == "deleted")
        .unwrap();
    state
        .db
        .delete_model_route(deleted.id, tenant, deleted.updated_at)
        .await
        .unwrap();
    let returned = publish(
        &state,
        account,
        tenant,
        &["restore", "manual", "deleted", "other"],
    )
    .await;
    let result = state
        .db
        .reconcile_managed_model_routes(account, tenant, 1, &returned)
        .await
        .unwrap();
    assert_eq!(result.restored, 1);
    assert_eq!(result.added, 0);
    assert_eq!(result.skipped, 2);
    let routes = state.db.list_model_routes(Some(tenant)).await.unwrap();
    assert!(
        routes
            .iter()
            .find(|route| route.public_model == "restore")
            .unwrap()
            .enabled
    );
    assert!(
        !routes
            .iter()
            .find(|route| route.id == manual.id)
            .unwrap()
            .enabled
    );
    assert!(routes.iter().all(|route| route.id != deleted.id));
}

async fn exercise_concurrency(state: &AppState, tenant: &str) {
    let account = account(state, tenant, "http://127.0.0.1:18081").await;
    let models = publish(state, account, tenant, &["one", "two"]).await;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let db = state.db.clone();
        let tenant = tenant.to_owned();
        let models = models.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            db.reconcile_managed_model_routes(account, &tenant, 1, &models)
                .await
                .unwrap()
        }));
    }
    let mut added = 0;
    for task in tasks {
        added += task.await.unwrap().added;
    }
    assert_eq!(added, 2);
    assert_eq!(
        state
            .db
            .list_model_routes(Some(tenant))
            .await
            .unwrap()
            .len(),
        2
    );
    let stale = models.clone();
    publish(state, account, tenant, &["different"]).await;
    let result = state
        .db
        .reconcile_managed_model_routes(account, tenant, 1, &stale)
        .await
        .unwrap();
    assert_eq!(result.disabled, 0);
    assert_eq!(result.warnings, vec!["catalog_changed"]);
}

#[tokio::test]
async fn sqlite_concurrent_reconciliation_has_one_owned_route_per_stable_key() {
    let (state, _directory) = state().await;
    exercise_concurrency(&state, "managed-concurrent").await;
}

#[tokio::test]
async fn postgres_concurrent_reconciliation_has_one_owned_route_per_stable_key() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let state = AppState::initialize(Config::for_test(url)).await.unwrap();
    exercise_concurrency(&state, &format!("managed-concurrent-{}", Uuid::now_v7())).await;
}

#[tokio::test]
async fn endpoint_requires_both_scopes_and_enforces_tenant_boundaries() {
    let (state, _directory) = state().await;
    let account_a = account(&state, "tenant-a", "http://127.0.0.1:18081").await;
    let account_b = account(&state, "tenant-b", "http://127.0.0.1:18081").await;
    for scopes in [vec!["providers:write"], vec!["routes:write"]] {
        let token = state
            .db
            .create_service_token(
                CreateServiceTokenInput {
                    name: "single-scope".into(),
                    scopes: scopes.into_iter().map(str::to_owned).collect(),
                    tenant_external_id: Some("tenant-a".into()),
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        assert_eq!(
            request(&state, account_a, &token.token, "sync-routes")
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    let token = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "tenant-a-sync".into(),
                scopes: vec!["providers:write".into(), "routes:write".into()],
                tenant_external_id: Some("tenant-a".into()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(
        request(&state, account_b, &token.token, "sync-routes")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let models = publish(&state, account_b, "tenant-b", &["private"]).await;
    assert!(matches!(
        state
            .db
            .reconcile_managed_model_routes(account_b, "tenant-a", 1, &models)
            .await,
        Err(AppError::NotFound)
    ));
    assert!(
        state
            .db
            .list_model_routes(Some("tenant-a"))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn refresh_only_endpoint_keeps_its_existing_route_lifecycle_contract() {
    let (state, _directory) = state().await;
    let server = MockServer::start().await;
    let tenant = "managed-refresh-only";
    let account = account(&state, tenant, &server.uri()).await;
    let models = publish(&state, account, tenant, &["old"]).await;
    state
        .db
        .reconcile_managed_model_routes(account, tenant, 1, &models)
        .await
        .unwrap();
    serve(&server, json!({"data": []})).await;
    let (status, result) = request(&state, account, &state.config.service_token, "sync").await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert!(result.get("routes").is_none());
    assert_eq!(result["status"], "ready");
    assert!(state.db.list_model_routes(Some(tenant)).await.unwrap()[0].enabled);
}
