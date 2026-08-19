use memeloop_token_center::{
    AppState, api,
    config::Config,
    db::{CreateKeyInput, CreateServiceTokenInput},
    model::KeyPolicy,
};
use reqwest::{Client, StatusCode};
use rust_decimal::Decimal;
use serde_json::json;
use tempfile::TempDir;
use tokio::task::JoinHandle;
use uuid::Uuid;

const READ_SCOPE: &str = "imports:session_archive:quarantine:read";
const RESOLVE_SCOPE: &str = "imports:session_archive:quarantine:resolve";

struct Fixture {
    _directory: TempDir,
    client: Client,
    base_url: String,
    state: AppState,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("archive-quarantine-api.db").display()
        );
        let state = AppState::initialize(Config::for_test(database_url))
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let served_state = state.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, api::router(served_state))
                .await
                .unwrap();
        });
        Self {
            _directory: directory,
            client: Client::new(),
            base_url: format!("http://{address}"),
            state,
            server,
        }
    }

    async fn service_token(&self, name: &str, scope: &str, tenant: Option<&str>) -> String {
        self.state
            .db
            .create_service_token(
                CreateServiceTokenInput {
                    name: name.to_owned(),
                    scopes: vec![scope.to_owned()],
                    tenant_external_id: tenant.map(str::to_owned),
                },
                self.state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap()
            .token
    }

    async fn client_key(&self) -> String {
        self.state
            .db
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "quarantine-auth-tenant".to_owned(),
                    principal_external_id: "quarantine-auth-user".to_owned(),
                    alias: "quarantine-auth-user".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ZERO,
                    idempotency_key: None,
                },
                self.state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap()
            .key
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn quarantine_operator_routes_require_exact_global_persistent_credentials() {
    let fixture = Fixture::new().await;
    let read = fixture
        .service_token("quarantine-reader", READ_SCOPE, None)
        .await;
    let resolve = fixture
        .service_token("quarantine-resolver", RESOLVE_SCOPE, None)
        .await;
    let wrong = fixture
        .service_token("ordinary-reader", "requests:read", None)
        .await;
    let tenant_read = fixture
        .service_token(
            "tenant-quarantine-reader",
            READ_SCOPE,
            Some("quarantine-auth-tenant"),
        )
        .await;
    let tenant_resolve = fixture
        .service_token(
            "tenant-quarantine-resolver",
            RESOLVE_SCOPE,
            Some("quarantine-auth-tenant"),
        )
        .await;
    let client_key = fixture.client_key().await;
    let list_url = format!(
        "{}/internal/v1/imports/session-archive/quarantine?tenant_external_id=quarantine-auth-tenant",
        fixture.base_url
    );

    let unauthenticated = fixture.client.get(&list_url).send().await.unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    for rejected in ["test-service-token", wrong.as_str(), tenant_read.as_str()] {
        let response = fixture
            .client
            .get(&list_url)
            .bearer_auth(rejected)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let client_response = fixture
        .client
        .get(&list_url)
        .bearer_auth(&client_key)
        .send()
        .await
        .unwrap();
    assert_eq!(client_response.status(), StatusCode::UNAUTHORIZED);
    let resolver_cannot_read = fixture
        .client
        .get(&list_url)
        .bearer_auth(&resolve)
        .send()
        .await
        .unwrap();
    assert_eq!(resolver_cannot_read.status(), StatusCode::FORBIDDEN);

    let listed = fixture
        .client
        .get(&list_url)
        .bearer_auth(&read)
        .send()
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    assert_eq!(listed.json::<serde_json::Value>().await.unwrap(), json!([]));

    let quarantine_id = Uuid::now_v7();
    let detail_url = format!(
        "{}/internal/v1/imports/session-archive/quarantine/{quarantine_id}?tenant_external_id=quarantine-auth-tenant",
        fixture.base_url
    );
    let detail = fixture
        .client
        .get(detail_url)
        .bearer_auth(&read)
        .send()
        .await
        .unwrap();
    assert_eq!(detail.status(), StatusCode::NOT_FOUND);

    let resolution_url = format!(
        "{}/internal/v1/imports/session-archive/quarantine/{quarantine_id}/resolutions",
        fixture.base_url
    );
    let resolution = json!({
        "tenant_external_id": "quarantine-auth-tenant",
        "action": "dismiss",
        "key_id": null,
        "expected_record_digest": "a".repeat(64),
        "evidence_digest": "b".repeat(64),
        "note": "operator verified the source identity was not recoverable"
    });

    for rejected in ["test-service-token", read.as_str(), tenant_resolve.as_str()] {
        let response = fixture
            .client
            .post(&resolution_url)
            .bearer_auth(rejected)
            .header("idempotency-key", "quarantine-resolution-auth-test")
            .json(&resolution)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    let client_resolution = fixture
        .client
        .post(&resolution_url)
        .bearer_auth(&client_key)
        .header("idempotency-key", "quarantine-resolution-client-key")
        .json(&resolution)
        .send()
        .await
        .unwrap();
    assert_eq!(client_resolution.status(), StatusCode::UNAUTHORIZED);

    let missing_idempotency = fixture
        .client
        .post(&resolution_url)
        .bearer_auth(&resolve)
        .json(&resolution)
        .send()
        .await
        .unwrap();
    assert_eq!(missing_idempotency.status(), StatusCode::BAD_REQUEST);

    let invalid_proof = fixture
        .client
        .post(&resolution_url)
        .bearer_auth(&resolve)
        .header("idempotency-key", "quarantine-resolution-invalid-proof")
        .json(&json!({
            "tenant_external_id": "quarantine-auth-tenant",
            "action": "dismiss",
            "key_id": null,
            "expected_record_digest": "not-a-digest",
            "evidence_digest": "b".repeat(64)
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_proof.status(), StatusCode::BAD_REQUEST);

    let resolver_reaches_tenant_bound_database_lookup = fixture
        .client
        .post(&resolution_url)
        .bearer_auth(&resolve)
        .header("idempotency-key", "quarantine-resolution-auth-test")
        .json(&resolution)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resolver_reaches_tenant_bound_database_lookup.status(),
        StatusCode::NOT_FOUND
    );
}
