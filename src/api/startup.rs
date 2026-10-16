//! Serve the existing static shell before any durable dependency initializes.
//! The only route installed during failure is a static/health router; no
//! authentication, data, billing or forwarding handler can run in this state.
use super::*;
use std::sync::{Arc, RwLock};
use tower::Service;

#[derive(Clone)]
pub struct StartupRouter {
    app: Arc<RwLock<Router>>,
    root: PathBuf,
}

pub fn router(role: RuntimeRole) -> (Router, StartupRouter) {
    router_at(role, super::web::web_root())
}

fn router_at(role: RuntimeRole, root: PathBuf) -> (Router, StartupRouter) {
    let handle = StartupRouter {
        app: Arc::new(RwLock::new(degraded_router(
            role,
            "service_initializing",
            "The service is starting. Data and authorization are not yet available.",
            root.clone(),
        ))),
        root,
    };
    let request_id = header::HeaderName::from_static(REQUEST_ID_HEADER);
    let app = Router::new()
        .fallback(dispatch)
        .layer(middleware::from_fn(security_headers))
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid))
        .with_state(handle.clone());
    (app, handle)
}

impl StartupRouter {
    pub fn install(&self, app: Router) {
        *self.app.write().expect("startup router lock") = app;
    }

    pub fn failed(&self, role: RuntimeRole, error: &crate::InitializationError) {
        use crate::{InitializationError, error::AppError};
        let (code, message) = match error {
            InitializationError::Database(AppError::DatabaseUnavailable(failure)) =>
                (failure.code(), failure.to_string()),
            InitializationError::Database(_) => (
                "database_initialization_failed",
                "Database initialization failed. Data and authorization cannot be verified. Service credentials do not need to be re-entered.".to_owned(),
            ),
            _ => (
                "application_initialization_failed",
                "Service initialization failed. Data and authorization are unavailable; contact the service operator. Service credentials do not need to be re-entered.".to_owned(),
            ),
        };
        tracing::error!(
            error_code = code,
            "application initialization unavailable; static shell retained"
        );
        self.install(degraded_router(role, code, &message, self.root.clone()));
    }
}

async fn dispatch(State(state): State<StartupRouter>, request: Request) -> Response {
    let mut app = state.app.read().expect("startup router lock").clone();
    match app.call(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

fn degraded_router(role: RuntimeRole, code: &'static str, message: &str, root: PathBuf) -> Router {
    let body = json!({
        "status": "not_ready",
        "checks": {
            "application": if code == "service_initializing" { "initializing" } else { "failed" },
            "database": "unknown", "database_schema": {"status": "unknown"}, "archive": "unknown"
        },
        "error": {"code": code, "message": message},
    });
    let unavailable = move || {
        let body = body.clone();
        async move {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CACHE_CONTROL, "no-store")],
                Json(body),
            )
                .into_response()
        }
    };
    let index = {
        let root = root.clone();
        move || super::web::index_at(root.clone())
    };
    let asset = move |Path(path): Path<String>| static_asset(root.clone(), path);
    let mut app = Router::new()
        .route("/livez", get(liveness))
        .route("/healthz", get(deprecated_health))
        .route("/readyz", get(unavailable.clone()))
        .route("/ui-assets/service-status", get(unavailable.clone()))
        .route("/ui-assets/{*path}", get(asset))
        .fallback(unavailable);
    if role.serves_control() {
        app = app.route("/operator", get(index.clone()));
    }
    if role.serves_gateway() {
        app = app.route("/portal", get(index));
    }
    app
}

async fn static_asset(root: PathBuf, path: String) -> Response {
    // Plugin modules require a fully validated runtime and are never served
    // from disk as a substitute for the existing plugin verification path.
    if path.starts_with("plugins/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    super::web::disk_asset_at(root, path).await
}

/// Public, fixed health metadata through the already routed asset prefix.
/// The existing readiness cache coalesces probes; this adds no write probe.
pub(super) async fn service_status(State(state): State<AppState>) -> Response {
    let (status, mut body) = super::health::readiness_snapshot(state).await;
    body["last_database_error"] =
        crate::error::DatabaseFailure::last_observed().unwrap_or(Value::Null);
    (status, [(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use tower::ServiceExt;

    async fn call(app: &Router, method: &str, path: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::AUTHORIZATION, "Bearer invalid-private-credential")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn database_failure_keeps_existing_shell_assets_and_truthful_health() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("index.html"),
            "<html>existing shell</html>",
        )
        .unwrap();
        std::fs::write(root.path().join("app.js"), "/* existing bundle */").unwrap();
        for (role, path) in [
            (RuntimeRole::Control, "/operator"),
            (RuntimeRole::Gateway, "/portal"),
            (RuntimeRole::All, "/operator"),
        ] {
            let (app, startup) = router_at(role, root.path().to_owned());
            startup.failed(
                role,
                &crate::InitializationError::Database(AppError::DatabaseUnavailable(
                    crate::error::DatabaseFailure::StorageFull,
                )),
            );
            let shell = call(&app, "GET", path).await;
            assert_eq!(shell.status(), StatusCode::OK);
            assert_eq!(shell.headers()[header::CACHE_CONTROL], "no-store");
            assert!(
                shell
                    .headers()
                    .contains_key(header::CONTENT_SECURITY_POLICY)
            );
            assert_eq!(
                to_bytes(shell.into_body(), 4096).await.unwrap(),
                "<html>existing shell</html>"
            );
            assert_eq!(
                call(&app, "GET", "/ui-assets/app.js").await.status(),
                StatusCode::OK
            );
            assert_eq!(call(&app, "GET", "/livez").await.status(), StatusCode::OK);
            for path in [
                "/readyz",
                "/ui-assets/service-status",
                "/internal/v1/keys",
                "/self/v1/key",
                "/v1/responses",
            ] {
                let response = call(&app, "GET", path).await;
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                let body = to_bytes(response.into_body(), 8192).await.unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["error"]["code"], "database_storage_full");
                assert!(!body.to_string().contains("invalid-private-credential"));
            }
            for path in [
                "/v1/responses",
                "/internal/v1/keys",
                "/internal/v1/integrations/memeloop-cloud/subscription",
            ] {
                assert_eq!(
                    call(&app, "POST", path).await.status(),
                    StatusCode::SERVICE_UNAVAILABLE
                );
            }
            assert_eq!(
                call(&app, "GET", "/ui-assets/plugins/private/module.js")
                    .await
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
    }

    #[tokio::test]
    async fn recovery_installs_original_authentication_without_replacing_http_listener() {
        let root = tempfile::tempdir().unwrap();
        let (app, startup) = router_at(RuntimeRole::Control, root.path().to_owned());
        assert_eq!(
            call(&app, "GET", "/internal/v1/keys").await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let (state, _directory) = super::super::tests::test_state().await;
        startup.install(super::super::router_for_role(state, RuntimeRole::Control));
        assert_eq!(
            call(&app, "GET", "/internal/v1/keys").await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(call(&app, "GET", "/readyz").await.status(), StatusCode::OK);
    }
}
