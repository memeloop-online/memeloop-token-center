use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::IntoResponse,
};
use memeloop_token_center::{
    AppState, api,
    archive::ArchiveStagingObjectStore,
    archive_reaper::ArchiveReaper,
    archive_staging::{
        ArchiveStagingIntentDigest, ArchiveStagingKey, ArchiveStagingLeaseOwner,
        ArchiveStagingOwner, ArchiveStagingPurpose, BeginArchiveStagingInput,
        BeginArchiveStagingResult,
    },
    config::{Config, RuntimeRole},
    db::{
        CreateKeyInput, CreateRoutedModelRouteInput, CreateUpstreamAccountInput,
        DiscoveredUpstreamModel, ReplaceModelCatalogResult,
    },
    error::AppError,
    model::KeyPolicy,
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const WORKER_CANARY: &str = "MTC_CANARY_WORKER_OBJECT_ERROR_2b7a36";

struct CanaryWorkerStore;

#[async_trait::async_trait]
impl ArchiveStagingObjectStore for CanaryWorkerStore {
    async fn delete_archive_staging_segment(
        &self,
        _key: ArchiveStagingKey,
    ) -> Result<(), AppError> {
        Err(AppError::Storage(WORKER_CANARY.to_owned()))
    }

    async fn archive_staging_segment_is_empty(
        &self,
        _key: ArchiveStagingKey,
    ) -> Result<bool, AppError> {
        unreachable!("the worker canary store always fails deletion")
    }
}

#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("log capture lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogCapture {
    type Writer = LogWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        LogWriter(self.0.clone())
    }
}

impl LogCapture {
    fn rendered(&self) -> String {
        String::from_utf8(self.0.lock().expect("log capture lock").clone())
            .expect("UTF-8 tracing output")
    }
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .expect("bounded security response");
    String::from_utf8(bytes.to_vec()).expect("UTF-8 security response")
}

#[tokio::test]
async fn proxy_oauth_import_database_and_object_store_never_log_or_return_canaries() {
    const PROXY_CANARY: &str = "MTC_CANARY_PROXY_41c3b7";
    const OAUTH_CANARY: &str = "MTC_CANARY_OAUTH_8fca22";
    const IMPORT_CANARY: &str = "MTC_CANARY_IMPORT_a65049";
    const DATABASE_CANARY: &str = "MTC_CANARY_DATABASE_65f33c";
    const OBJECT_CANARY: &str = "MTC_CANARY_OBJECT_STORE_d1c29a";

    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);

    let response_bodies = async {
        let directory = tempfile::tempdir().expect("security log directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("security-log.db").display()
        );
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string(PROXY_CANARY))
            .mount(&upstream)
            .await;
        Mock::given(method("GET"))
            .and(path("/oauth/poll"))
            .respond_with(ResponseTemplate::new(401).set_body_string(OAUTH_CANARY))
            .mount(&upstream)
            .await;

        let mut config = Config::for_test(database_url);
        config.upstream_openai_url = Some(upstream.uri());
        let state = AppState::initialize(config).await.expect("security log state");
        let staging_key = ArchiveStagingKey::new(
            ArchiveStagingOwner::ProxyRequest(uuid::Uuid::now_v7()),
            ArchiveStagingPurpose::Request,
            uuid::Uuid::now_v7(),
        )
        .expect("worker canary staging key");
        let staging_lease = match state
            .db
            .begin_archive_staging_attempt(BeginArchiveStagingInput {
                key: staging_key,
                intent_digest: ArchiveStagingIntentDigest::new("a".repeat(64))
                    .expect("worker canary digest"),
                lease_token: uuid::Uuid::now_v7(),
                lease_owner: ArchiveStagingLeaseOwner::new("security-log-writer")
                    .expect("worker canary writer"),
            })
            .await
            .expect("begin worker canary staging")
        {
            BeginArchiveStagingResult::Created(lease) => lease,
            _ => panic!("worker canary staging attempt was not created"),
        };
        state
            .db
            .abandon_archive_staging_attempt(&staging_lease)
            .await
            .expect("abandon worker canary staging");
        let reaper = ArchiveReaper::with_store(
            state.db.clone(),
            Arc::new(CanaryWorkerStore),
            ArchiveStagingLeaseOwner::new("security-log-reaper")
                .expect("worker canary reaper"),
        );
        let pass = reaper.reap_once().await.expect("worker canary reaper pass");
        assert_eq!(pass.claimed, 1);
        assert_eq!(pass.cleaned, 0);
        state
            .db
            .upsert_model_price(
                "security-log-model",
                "USD",
                Decimal::ZERO,
                Decimal::ZERO,
            )
            .await
            .expect("security log price");
        let issued = state
            .db
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "security-log-tenant".to_owned(),
                    principal_external_id: "security-log-principal".to_owned(),
                    alias: "security-log-key".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy {
                        allowed_models: vec!["security-log-model".to_owned()],
                        ..KeyPolicy::default()
                    },
                    initial_balance: Decimal::TEN,
                    idempotency_key: None,
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .expect("security log key");
        let account = state
            .db
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: "security-log-tenant".to_owned(),
                    name: "security-log-upstream".to_owned(),
                    driver: "http-json".to_owned(),
                    config: json!({
                        "base_url": upstream.uri(),
                        "network_scope": "private"
                    }),
                    credential: UpstreamCredential::None,
                    oauth_session_id: None,
                    oauth_driver: None,
                    oauth_refresh_url: None,
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .expect("security log upstream");
        let catalog_lease = uuid::Uuid::now_v7();
        assert!(
            state
                .db
                .claim_upstream_model_catalog_sync(
                    account.id,
                    "security-log-tenant",
                    account.credential_generation,
                    catalog_lease,
                )
                .await
                .expect("claim security log model catalog")
        );
        assert_eq!(
            state
                .db
                .replace_upstream_model_catalog(
                    account.id,
                    "security-log-tenant",
                    account.credential_generation,
                    catalog_lease,
                    "openai_v1",
                    &[DiscoveredUpstreamModel {
                        model_id: "security-log-model".to_owned(),
                        protocol: "openai".to_owned(),
                        context_window: None,
                        reservation_token_bound: None,
                        reservation_bound_source: None,
                    }],
                )
                .await
                .expect("replace security log model catalog"),
            ReplaceModelCatalogResult::Replaced
        );
        state
            .db
            .create_routed_model_route(CreateRoutedModelRouteInput {
                tenant_external_id: "security-log-tenant".to_owned(),
                public_model: "security-log-model".to_owned(),
                upstream_model: "security-log-model".to_owned(),
                protocol: "openai".to_owned(),
                priority: 0,
                upstream_account_ids: vec![account.id],
                included_provider_group_ids: Vec::new(),
                excluded_provider_group_ids: Vec::new(),
                route_group_ids: Vec::new(),
                route_group_names: Vec::new(),
                granted_credential_ids: vec![issued.key_id],
                custom_model_confirmed: false,
            })
            .await
            .expect("security log authorized route");

        let proxy_response = api::router_for_role(state.clone(), RuntimeRole::Gateway)
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header(header::AUTHORIZATION, format!("Bearer {}", issued.key))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "model": "security-log-model",
                            "messages": [{"role": "user", "content": "redaction probe"}]
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .expect("proxy canary response");
        assert_eq!(proxy_response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let proxy_body = body_text(proxy_response).await;

        let start_response = api::router_for_role(state.clone(), RuntimeRole::Control)
            .oneshot(
                Request::post("/internal/v1/oauth/cursor/start")
                    .header(header::AUTHORIZATION, "Bearer test-service-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "tenant_external_id": "security-log-tenant",
                            "account_name": "security-log-oauth",
                            "provider_driver": "http-json",
                            "provider_config": {
                                "base_url": upstream.uri(),
                                "network_scope": "private"
                            },
                            "endpoints": {
                                "login_url": format!("{}/oauth/login", upstream.uri()),
                                "poll_url": format!("{}/oauth/poll", upstream.uri()),
                                "refresh_url": format!("{}/oauth/refresh", upstream.uri())
                            }
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .expect("OAuth start response");
        assert_eq!(start_response.status(), StatusCode::OK);
        let started: Value = serde_json::from_str(&body_text(start_response).await)
            .expect("OAuth start JSON");
        let oauth_response = api::router_for_role(state.clone(), RuntimeRole::Control)
            .oneshot(
                Request::post("/internal/v1/oauth/cursor/poll")
                    .header(header::AUTHORIZATION, "Bearer test-service-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "session_token": started["session_token"]
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .expect("OAuth poll response");
        assert_eq!(oauth_response.status(), StatusCode::BAD_GATEWAY);
        let oauth_body = body_text(oauth_response).await;

        let import_response = api::router_for_role(state, RuntimeRole::Control)
            .oneshot(
                Request::post("/internal/v1/imports/cpa/managed-oauth")
                    .header(header::AUTHORIZATION, "Bearer test-service-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "contract_version": 1,
                            "tenant_external_id": "security-log-tenant",
                            "source": {"kind": "auth_file", "relative_path": "accounts/redaction.json"},
                            "source_type": "unsupported-redaction-source",
                            "document": {"access_token": IMPORT_CANARY}
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .expect("import canary response");
        assert_eq!(import_response.status(), StatusCode::BAD_REQUEST);
        let import_body = body_text(import_response).await;

        let upstream_requests = upstream
            .received_requests()
            .await
            .expect("redaction mock request recording");
        assert_eq!(
            upstream_requests
                .iter()
                .filter(|request| {
                    request.method.as_str() == "POST"
                        && request.url.path() == "/v1/chat/completions"
                })
                .count(),
            1,
            "proxy redaction probe did not reach its mock upstream"
        );
        assert_eq!(
            upstream_requests
                .iter()
                .filter(|request| {
                    request.method.as_str() == "GET" && request.url.path() == "/oauth/poll"
                })
                .count(),
            1,
            "OAuth redaction probe did not reach its mock upstream"
        );

        let database_body = body_text(
            AppError::from(sqlx::Error::Protocol(DATABASE_CANARY.to_owned())).into_response(),
        )
        .await;
        let object_body = body_text(
            AppError::from(object_store::Error::Generic {
                store: "security-log-store",
                source: Box::new(std::io::Error::other(OBJECT_CANARY)),
            })
            .into_response(),
        )
        .await;

        vec![proxy_body, oauth_body, import_body, database_body, object_body]
    }
    .with_subscriber(dispatch)
    .await;

    let logs = capture.rendered();
    assert!(
        logs.contains("database operation failed"),
        "database redaction probe did not reach the tracing collector"
    );
    assert!(
        logs.contains("object storage operation failed"),
        "object-store redaction probe did not reach the tracing collector"
    );
    assert!(
        logs.contains("archive staging cleanup will retry"),
        "worker redaction probe did not reach the tracing collector"
    );
    for canary in [
        PROXY_CANARY,
        OAUTH_CANARY,
        IMPORT_CANARY,
        DATABASE_CANARY,
        OBJECT_CANARY,
        WORKER_CANARY,
    ] {
        assert!(
            response_bodies.iter().all(|body| !body.contains(canary)),
            "a response exposed a secret canary"
        );
        assert!(!logs.contains(canary), "tracing exposed a secret canary");
    }
}
