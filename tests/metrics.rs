use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::CreateServiceTokenInput,
};
use serde_json::Value;
use tower::ServiceExt;

async fn test_application() -> (tempfile::TempDir, Router) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("metrics.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .expect("application state");
    (directory, api::router(state))
}

async fn get(application: &Router, path: &str) -> axum::response::Response {
    application
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response")
}

async fn request(application: &Router, method: Method, path: &str) -> axum::response::Response {
    application
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .expect("request"),
        )
        .await
        .expect("response")
}

async fn get_authorized(application: &Router, path: &str) -> axum::response::Response {
    application
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::AUTHORIZATION, "Bearer test-service-token")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response")
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("bounded body");
    String::from_utf8(bytes.to_vec()).expect("UTF-8 response")
}

#[tokio::test]
async fn health_version_and_metrics_contract_is_operational() {
    let (_directory, application) = test_application().await;

    let liveness = get(&application, "/livez").await;
    assert_eq!(liveness.status(), StatusCode::OK);

    let readiness = get(&application, "/readyz").await;
    assert_eq!(readiness.status(), StatusCode::OK);
    let readiness: Value =
        serde_json::from_str(&body_text(readiness).await).expect("readiness JSON");
    assert_eq!(readiness["checks"]["database"], "ok");
    assert_eq!(readiness["checks"]["archive"], "ok");

    let compatibility = get(&application, "/healthz").await;
    assert_eq!(compatibility.status(), StatusCode::OK);
    assert_eq!(
        compatibility
            .headers()
            .get("deprecation")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        compatibility
            .headers()
            .get(header::LINK)
            .and_then(|value| value.to_str().ok()),
        Some("</livez>; rel=\"successor-version\"")
    );

    let unauthenticated_version = get(&application, "/version").await;
    assert_eq!(unauthenticated_version.status(), StatusCode::UNAUTHORIZED);
    let version = get_authorized(&application, "/version").await;
    assert_eq!(version.status(), StatusCode::OK);
    let version: Value = serde_json::from_str(&body_text(version).await).expect("version JSON");
    assert_eq!(version["service"], "memeloop-token-center");
    assert_eq!(version["api"]["current"], "v1");
    assert_eq!(version["api"]["deprecated"][0]["path"], "/healthz");

    // A concrete unknown URI must never become a Prometheus label.
    let missing = get(&application, "/private-user-value/credential-123").await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    let unauthenticated_metrics = get(&application, "/metrics").await;
    assert_eq!(unauthenticated_metrics.status(), StatusCode::UNAUTHORIZED);
    let metrics = get_authorized(&application, "/metrics").await;
    assert_eq!(metrics.status(), StatusCode::OK);
    assert_eq!(
        metrics
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    let metrics = body_text(metrics).await;
    assert!(metrics.contains("memeloop_token_center_build_info"));
    assert!(metrics.contains("memeloop_token_center_http_requests_total"));
    assert!(metrics.contains("route=\"/version\""));
    assert!(metrics.contains("route=\"unmatched\""));
    assert!(metrics.contains("memeloop_token_center_dependency_ready{dependency=\"database\"} 1"));
    assert!(metrics.contains("memeloop_token_center_db_pool_connections{state=\"idle\"}"));
    assert!(metrics.contains("memeloop_token_center_generation_jobs{status=\"queued\"} 0"));
    assert!(!metrics.contains("private-user-value"));
    assert!(!metrics.contains("credential-123"));
    assert!(!metrics.contains("test-service-token"));
}

#[tokio::test]
async fn public_gateway_role_does_not_register_operational_metadata_routes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("gateway-metrics.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .expect("application state");
    let application = api::router_for_role(state, RuntimeRole::Gateway);

    assert_eq!(get(&application, "/readyz").await.status(), StatusCode::OK);
    assert_eq!(
        get_authorized(&application, "/metrics").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&application, "/version").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn gateway_and_control_roles_return_404_for_every_opposite_operation_family() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("role-isolation.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .expect("application state");
    let gateway = api::router_for_role(state.clone(), RuntimeRole::Gateway);
    let control = api::router_for_role(state, RuntimeRole::Control);
    let id = "00000000-0000-7000-8000-000000000001";

    let control_operations = [
        (Method::GET, "/operator".to_owned()),
        (Method::GET, "/version".to_owned()),
        (Method::GET, "/metrics".to_owned()),
        (Method::GET, "/internal/v1/keys".to_owned()),
        (Method::POST, "/internal/v1/service-tokens".to_owned()),
        (Method::GET, "/internal/v1/provider-types".to_owned()),
        (Method::GET, "/internal/v1/plugins".to_owned()),
        (Method::GET, "/internal/v1/schemas".to_owned()),
        (Method::POST, "/internal/v1/oauth/cursor/start".to_owned()),
        (Method::GET, "/internal/v1/upstreams".to_owned()),
        (Method::GET, format!("/internal/v1/upstreams/{id}/models")),
        (
            Method::GET,
            "/internal/v1/imports/cpa/managed-oauth/capabilities".to_owned(),
        ),
        (
            Method::GET,
            "/internal/v1/imports/session-archive/quarantine?tenant_external_id=t".to_owned(),
        ),
        (Method::GET, "/internal/v1/requests".to_owned()),
        (Method::GET, "/internal/v1/stats".to_owned()),
        (Method::GET, "/internal/v1/request-events".to_owned()),
        (Method::GET, "/internal/v1/model-routes".to_owned()),
        (Method::GET, "/internal/v1/provider-groups".to_owned()),
        (Method::GET, "/internal/v1/route-groups".to_owned()),
        (Method::GET, "/internal/v1/credential-groups".to_owned()),
        (Method::GET, "/internal/v1/model-prices".to_owned()),
        (Method::GET, "/internal/v1/generation-prices".to_owned()),
        (Method::GET, format!("/internal/v1/accounts/{id}/ledger")),
        (Method::GET, "/internal/v1/entitlements".to_owned()),
        (
            Method::PUT,
            "/internal/v1/integrations/memeloop-cloud/subscription".to_owned(),
        ),
    ];
    for (method, path) in control_operations {
        let response = request(&gateway, method.clone(), &path).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "gateway unexpectedly registered control operation {method} {path}"
        );
    }

    let gateway_operations = [
        (Method::GET, "/portal"),
        (Method::GET, "/self/v1/key"),
        (Method::GET, "/self/v1/key/limits"),
        (Method::GET, "/self/v1/requests"),
        (Method::GET, "/self/v1/stats"),
        (Method::GET, "/self/v1/usage-analysis"),
        (Method::GET, "/self/v1/generations"),
        (Method::GET, "/self/v1/conversations"),
        (Method::GET, "/v1/models"),
        (Method::POST, "/v1/chat/completions"),
        (Method::POST, "/v1/responses"),
        (Method::POST, "/v1/embeddings"),
        (Method::POST, "/v1/messages"),
        (Method::POST, "/v1/generations"),
        (Method::POST, "/v1/videos/generations"),
        (Method::POST, "/v1/images/generations"),
    ];
    for (method, path) in gateway_operations {
        let response = request(&control, method.clone(), path).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "control unexpectedly registered gateway operation {method} {path}"
        );
    }
}

#[tokio::test]
async fn operational_metadata_requires_the_metrics_scope() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("metrics-scope.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .expect("application state");
    let issued = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "request-reader".to_owned(),
                scopes: vec!["requests:read".to_owned()],
                tenant_external_id: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .expect("scoped service credential");
    let application = api::router_for_role(state, RuntimeRole::Control);

    for path in ["/metrics", "/version"] {
        let response = application
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::AUTHORIZATION, format!("Bearer {}", issued.token))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
}
