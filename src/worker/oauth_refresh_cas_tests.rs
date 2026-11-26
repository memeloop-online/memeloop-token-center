use super::*;
use crate::{config::Config, db::CreateUpstreamAccountInput, provider::UpstreamCredential};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

struct Fixture {
    state: AppState,
    server: MockServer,
    account: Uuid,
    pool: sqlx::AnyPool,
    _directory: tempfile::TempDir,
}
impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let mut config = Config::for_test(format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("cas.db").display()
        ));
        config.pricing_models_dev_url = format!("{}/prices", server.uri());
        config.pricing_litellm_url = config.pricing_models_dev_url.clone();
        config.pricing_openrouter_url = config.pricing_models_dev_url.clone();
        let database_url = config.database_url.clone();
        let state = Box::pin(AppState::initialize(config)).await.unwrap();
        let pool = sqlx::AnyPool::connect(&database_url).await.unwrap();
        let account = state
            .db
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: "cas-contract".into(),
                    name: "synthetic-cursor".into(),
                    driver: "http-json".into(),
                    config: json!({"base_url":"https://api.example.test"}),
                    credential: Self::credential(),
                    oauth_session_id: Some(Uuid::new_v4()),
                    oauth_driver: Some("cursor".into()),
                    oauth_refresh_url: Some(format!("{}/refresh", server.uri())),
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap()
            .id;
        Self {
            state,
            server,
            account,
            pool,
            _directory: directory,
        }
    }
    fn credential() -> UpstreamCredential {
        UpstreamCredential::OAuth {
            access_token: "synthetic-access".into(),
            refresh_token: Some("synthetic-refresh".into()),
            expires_at: Some(4_102_444_800_000),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: None,
            proxy_network_scope: None,
        }
    }
    async fn post(&self, key: &str, query: &str, authorized: bool) -> (StatusCode, Value) {
        self.post_token(
            key,
            query,
            if authorized {
                "test-service-token"
            } else {
                "invalid"
            },
        )
        .await
    }
    async fn post_token(&self, key: &str, query: &str, token: &str) -> (StatusCode, Value) {
        let request = Request::post(format!(
            "/internal/v1/upstreams/{}/oauth/refresh{}",
            self.account, query
        ))
        .header("authorization", format!("Bearer {token}"))
        .header("idempotency-key", key)
        .body(Body::empty())
        .unwrap();
        let response =
            api::router_for_role(self.state.clone(), crate::config::RuntimeRole::Control)
                .oneshot(request)
                .await
                .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn exchanges(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/refresh")
            .count()
    }
    async fn supplier(&self) {
        Mock::given(method("POST"))
            .and(path("/refresh"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"accessToken":"synthetic-next"})),
            )
            .mount(&self.server)
            .await;
    }
}

#[tokio::test]
async fn oauth_control_cas_rejects_invalid_stale_and_unauthorized_without_exchange() {
    let f = Box::pin(Fixture::new()).await;
    for query in [
        "?expected_credential_generation=0",
        "?expected_credential_generation=-1",
        "?expected_credential_generation=9223372036854775808",
        "?expected_credential_generation=1&expected_credential_generation=1",
    ] {
        assert_eq!(
            f.post("invalid", query, true).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        f.post("stale", "?expected_credential_generation=2", true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.post("unauthorized", "?expected_credential_generation=1", false)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let limited = f
        .state
        .db
        .create_service_token(
            crate::db::CreateServiceTokenInput {
                name: "synthetic-read-only".into(),
                scopes: vec!["providers:read".into()],
                tenant_external_id: None,
            },
            f.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(
        f.post_token(
            "forbidden",
            "?expected_credential_generation=1",
            &limited.token
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(f.exchanges().await, 0);
}

#[tokio::test]
async fn oauth_control_cas_preserves_legacy_history_and_disabled_fence() {
    let f = Box::pin(Fixture::new()).await;
    f.supplier().await;
    // Legacy manual request and worker-style receipt retain the original hash.
    assert_eq!(
        f.post("legacy", "", true).await.1["credential_generation"],
        2
    );
    let blocking = BlockingTasks::new();
    let result = api::refresh_managed_upstream_oauth_for_worker(
        &f.state,
        f.account,
        "worker-generation-2",
        2,
        &blocking,
    )
    .await
    .unwrap();
    assert_eq!(result.credential_generation, 3);
    let old = f
        .post("legacy", "?expected_credential_generation=1", true)
        .await;
    assert_eq!(old.0, StatusCode::OK);
    assert_eq!(old.1["credential_generation"], 2);
    assert_eq!(
        f.post("legacy", "?expected_credential_generation=2", true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.post(
            "worker-generation-2",
            "?expected_credential_generation=2",
            true
        )
        .await
        .1["credential_generation"],
        3
    );
    sqlx::query("UPDATE upstream_accounts SET status='disabled' WHERE id=$1")
        .bind(f.account.to_string())
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        f.post("legacy", "?expected_credential_generation=1", true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.post("disabled-new", "?expected_credential_generation=3", true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(f.exchanges().await, 2);
}

#[tokio::test]
async fn oauth_control_cas_pending_is_local_and_started_unknown_is_not_replayed() {
    let f = Box::pin(Fixture::new()).await;
    f.state
        .db
        .claim_upstream_oauth_refresh(f.account, "pending", f.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    f.state
        .db
        .mark_upstream_oauth_refresh_request_started(f.account, "pending")
        .await
        .unwrap();
    assert_eq!(
        f.post("pending", "?expected_credential_generation=2", true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.post("pending", "?expected_credential_generation=1", true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.post("another-key", "?expected_credential_generation=1", true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let saved = Fixture::credential();
    let sealed =
        crate::provider::seal_credential(&saved, f.state.config.key_pepper.as_bytes()).unwrap();
    sqlx::query("UPDATE upstream_oauth_refresh_leases SET pending_credential_ciphertext=$1, pending_expires_at=$2 WHERE account_id=$3").bind(sealed).bind(saved.expires_at()).bind(f.account.to_string()).execute(&f.pool).await.unwrap();
    assert_eq!(
        f.post("pending", "?expected_credential_generation=2", true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let recovered = f
        .post("pending", "?expected_credential_generation=1", true)
        .await;
    assert_eq!(recovered.0, StatusCode::OK);
    assert_eq!(recovered.1["credential_generation"], 2);
    assert_eq!(f.exchanges().await, 0);
}

#[tokio::test]
async fn oauth_control_cas_and_worker_share_one_generation_claim() {
    let f = Box::pin(Fixture::new()).await;
    f.supplier().await;
    let blocking = BlockingTasks::new();
    let (control, worker) = tokio::join!(
        f.post(
            "control-concurrent",
            "?expected_credential_generation=1",
            true
        ),
        api::refresh_managed_upstream_oauth_for_worker(
            &f.state,
            f.account,
            "worker-concurrent",
            1,
            &blocking
        )
    );
    assert!(control.0 == StatusCode::OK || worker.is_ok());
    assert!(matches!(control.0, StatusCode::OK | StatusCode::CONFLICT));
    if let Err(error) = worker {
        assert!(
            matches!(error, crate::error::AppError::Conflict(_)),
            "{error}"
        );
    }
    assert_eq!(f.exchanges().await, 1);
    let generation: i64 =
        sqlx::query_scalar("SELECT credential_generation FROM upstream_accounts WHERE id=$1")
            .bind(f.account.to_string())
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(generation, 2);
}
