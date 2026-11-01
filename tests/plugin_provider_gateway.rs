use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use futures_util::StreamExt;
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{
        CreateKeyInput, CreateModelRouteInput, CreateUpstreamAccountInput,
        ReplaceCredentialRoutingInput, StatsFilter, unix_millis,
    },
    model::KeyPolicy,
    provider::UpstreamCredential,
    worker,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::Row;
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch},
};
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, header as matches_header, method, path},
};

mod http1_request {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/http1_request.rs"
    ));
}

#[tokio::test]
async fn plugin_provider_onboards_through_the_real_management_api() {
    let mock = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("plugin-onboarding.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some(".plugin-examples/examples/plugins".to_owned());
    let state = AppState::initialize(config).await.unwrap();
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::post("/internal/v1/upstreams")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", state.config.service_token),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "tenant_external_id": "plugin-onboarding",
                        "name": "plugin-api-primary",
                        "driver": "example-oauth-http",
                        "config": {
                            "base_url": mock.uri(),
                            "network_scope": "public"
                        },
                        "credential": {
                            "type": "api_key",
                            "value": "raw-provider-secret"
                        }
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let account: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::CREATED, "{account}");
    assert_eq!(account["driver"], "example-oauth-http");
    assert_eq!(account["connection_method"], "api_key");
    assert_eq!(account["can_rotate"], true);
    assert_eq!(account["can_refresh"], false);
    assert!(!account.to_string().contains("raw-provider-secret"));

    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::post("/internal/v1/model-routes")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", state.config.service_token),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "tenant_external_id": "plugin-onboarding",
                        "public_model": "plugin-public-model",
                        "upstream_account_id": account["id"],
                        "upstream_model": "plugin-private-model",
                        "protocol": "openai",
                        "priority": 0,
                        "custom_model_confirmed": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn oauth_only_provider_rejects_raw_oauth_on_the_direct_creation_api() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("oauth-only-provider.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap();
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::post("/internal/v1/upstreams")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", state.config.service_token),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "tenant_external_id": "oauth-only-provider",
                        "name": "raw-oauth-must-not-be-accepted",
                        "driver": "openai-codex",
                        "config": {
                            "base_url": "https://chatgpt.com/backend-api/codex",
                            "network_scope": "public",
                            "reservation_token_bounds": {}
                        },
                        "credential": {
                            "type": "oauth",
                            "access_token": "raw-access-secret",
                            "refresh_token": "raw-refresh-secret",
                            "expires_at": unix_millis() + 3_600_000,
                            "adapter_state": {}
                        }
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["error"]["code"], "invalid_request");
    assert!(
        error["error"]["message_en"]
            .as_str()
            .is_some_and(|message| message.contains("authorization flow"))
    );
    assert!(!error.to_string().contains("raw-access-secret"));
    assert!(!error.to_string().contains("raw-refresh-secret"));
}

#[tokio::test]
async fn direct_only_provider_accepts_its_schema_declared_api_key() {
    let mock = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("direct-only-provider.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap();
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::post("/internal/v1/upstreams")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", state.config.service_token),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "tenant_external_id": "direct-only-provider",
                        "name": "direct-api-key",
                        "driver": "http-json",
                        "config": {
                            "base_url": mock.uri(),
                            "network_scope": "public"
                        },
                        "credential": {
                            "type": "api_key",
                            "value": "direct-provider-secret"
                        }
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let account: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::CREATED, "{account}");
    assert_eq!(account["driver"], "http-json");
    assert_eq!(account["connection_method"], "api_key");
    assert!(!account.to_string().contains("direct-provider-secret"));
}

struct ComponentProviderFixture {
    _directory: tempfile::TempDir,
    state: AppState,
    key: String,
    account_id: Uuid,
}

async fn component_provider_fixture(
    base_url: &str,
    database_name: &str,
) -> ComponentProviderFixture {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join(database_name).display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some(".plugin-examples/examples/plugins".to_owned());
    let state = AppState::initialize(config).await.unwrap();
    let pepper = state.config.key_pepper.as_bytes();
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "component-provider-tenant".into(),
                principal_external_id: "component-provider-user".into(),
                alias: "stable-component-key".into(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    allowed_models: vec!["requested-model".into(), "example-rewritten".into()],
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
        .upsert_model_price("vendor-route-model", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "component-provider-tenant".into(),
                name: "same-account-api-or-oauth".into(),
                driver: "example-oauth-http".into(),
                config: json!({"base_url": base_url, "network_scope": "public"}),
                credential: UpstreamCredential::ApiKey {
                    value: "component-api-secret".into(),
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
    let requested_route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: "component-provider-tenant".into(),
            public_model: "requested-model".into(),
            upstream_account_id: account.id,
            upstream_model: "vendor-route-model".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    let rewritten_route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: "component-provider-tenant".into(),
            public_model: "example-rewritten".into(),
            upstream_account_id: account.id,
            upstream_model: "vendor-route-model".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    let current_routing = state
        .db
        .credential_routing(issued.key_id, "component-provider-tenant")
        .await
        .unwrap();
    let mut route_ids = current_routing.route_ids;
    route_ids.extend([requested_route.id, rewritten_route.id]);
    state
        .db
        .replace_credential_routing(
            issued.key_id,
            ReplaceCredentialRoutingInput {
                tenant_external_id: "component-provider-tenant".into(),
                route_ids,
                route_group_ids: current_routing.route_group_ids,
                expected_grant_revision: current_routing.grant_revision,
            },
        )
        .await
        .unwrap();
    ComponentProviderFixture {
        _directory: directory,
        state,
        key: issued.key,
        account_id: account.id,
    }
}

#[tokio::test]
async fn real_component_provider_normalizes_non_openai_upstream_and_core_owns_secrets_billing_and_archive()
 {
    let _logs = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_env_filter("memeloop_token_center::response_archive_spool=warn")
            .with_test_writer()
            .finish(),
    );
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/vendor/infer"))
        .and(matches_header("x-plugin-shape", "buffered-v1"))
        .and(body_json(json!({
            "prompt": "from-component",
            "model": "vendor-model"
        })))
        .respond_with(ResponseTemplate::new(207).set_body_json(json!({
            "vendor_answer": "non-openai-shape"
        })))
        .expect(2)
        .mount(&mock)
        .await;

    let ComponentProviderFixture {
        _directory,
        state,
        key: issued_key,
        account_id,
    } = component_provider_fixture(&mock.uri(), "component-provider.db").await;
    let pepper = state.config.key_pepper.as_bytes();

    let first = call_component_provider(&state, &issued_key).await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    // The real component response expands its tiny upstream body. Its adapter
    // maximum, not the upstream Content-Length, must cover normalization.
    assert!(
        serde_json::to_vec(&first.1).unwrap().len()
            > serde_json::to_vec(&json!({"vendor_answer":"non-openai-shape"}))
                .unwrap()
                .len()
    );
    assert_eq!(
        first.1["choices"][0]["message"]["content"],
        "normalized by component"
    );

    let rotated = state
        .db
        .rotate_upstream_credential(
            account_id,
            UpstreamCredential::ApiKey {
                value: "component-api-secret-rotated".into(),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
            },
            "component-provider-api-rotation",
            pepper,
        )
        .await
        .unwrap();
    assert_eq!(rotated.id, account_id);
    assert_eq!(rotated.connection_method, "api_key");

    let second = call_component_provider(&state, &issued_key).await;
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(second.1["usage"]["prompt_tokens"], 7);

    let received = mock.received_requests().await.unwrap();
    let credentials = received
        .iter()
        .map(|request| {
            request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        credentials,
        [
            "Bearer component-api-secret",
            "Bearer component-api-secret-rotated"
        ]
    );
    for request in &received {
        let body = String::from_utf8_lossy(&request.body);
        assert!(!body.contains("component-api-secret"));
        assert!(!body.contains("component-api-secret-rotated"));
    }

    let key = state
        .db
        .authenticate_key(&issued_key, pepper)
        .await
        .unwrap();
    let requests = state.db.list_requests(key.key_id, 10).await.unwrap();
    assert_eq!(requests.len(), 2);
    let request_ids = requests
        .iter()
        .map(|request| request.request_id)
        .collect::<Vec<_>>();
    wait_for_bound_plugin_archives(&state, &request_ids).await;
    let requests = state.db.list_requests(key.key_id, 10).await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| {
        request.model == "example-rewritten"
            && request.status_code == Some(200)
            && request.input_tokens == 7
            && request.output_tokens == 3
            && request.cost != "0"
    }));
    let exact_account = state
        .db
        .stats_filtered(
            key.key_id,
            StatsFilter {
                from_created_at: Some(unix_millis().saturating_sub(60_000)),
                to_created_at: Some(unix_millis().saturating_add(1)),
                upstream_account_id: Some(account_id),
                ..StatsFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(exact_account.summary.total_requests, 2);

    let inspection = sqlx::AnyPool::connect(&state.config.database_url)
        .await
        .unwrap();
    for request in requests {
        let refs = state
            .db
            .request_archive_refs(key.key_id, request.request_id)
            .await
            .unwrap();
        let cas_prefix = format!("tenants/{}/cas/v1/blake3/", key.tenant_id);
        assert_eq!(
            refs.request_object,
            format!(
                "staging/proxy/{0}/request/{0}/body.json",
                request.request_id
            )
        );
        let response_object = refs.response_object.as_deref().unwrap();
        assert!(
            response_object.starts_with(&cas_prefix)
                || response_object
                    .starts_with(&format!("staging/proxy/{}/response/", request.request_id))
        );
        let bound: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM response_archive_spools s JOIN request_records r ON r.id = s.request_id AND r.tenant_id = s.tenant_id AND r.reservation_id = s.reservation_id WHERE r.id = $1 AND r.key_id = $2 AND r.tenant_id = $3 AND s.state = 'bound' AND s.bound_locator = $4 AND r.response_object = $4",
        )
        .bind(request.request_id.to_string())
        .bind(key.key_id.to_string())
        .bind(key.tenant_id.to_string())
        .bind(response_object)
        .fetch_one(&inspection)
        .await
        .unwrap();
        assert_eq!(bound, 1);
        let archived_request = state
            .archive
            .get_bounded(&refs.request_object, 1024 * 1024)
            .await
            .unwrap();
        let archived_response = state
            .archive
            .get_bounded(response_object, 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&archived_request).unwrap(),
            json!({
                "model": "requested-model",
                "messages": [{"role": "user", "content": "canonical"}],
                "stream": false,
                "max_tokens": 32
            })
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&archived_response).unwrap()["choices"][0]["message"]["content"],
            "normalized by component"
        );
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&archived_request),
            String::from_utf8_lossy(&archived_response)
        );
        assert!(!combined.contains("component-api-secret"));
        assert!(!combined.contains("component-api-secret-rotated"));
    }
    inspection.close().await;
}

#[tokio::test]
async fn component_provider_does_not_replay_after_the_server_accepts_the_prepared_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = attempts.clone();
    let (shutdown, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        tokio::pin!(shutdown_rx);
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.unwrap();
                    let request = http1_request::read_bounded_http1_request(&mut stream)
                        .await
                        .unwrap();
                    assert_eq!(request.method, "POST");
                    assert_eq!(request.path, "/vendor/infer");
                    assert_eq!(
                        request.body.as_slice(),
                        b"{\"prompt\":\"from-component\",\"model\":\"vendor-model\"}"
                    );
                    server_attempts.fetch_add(1, Ordering::SeqCst);
                    drop(stream);
                }
            }
        }
    });
    let ComponentProviderFixture {
        _directory,
        state,
        key,
        account_id: _,
    } = component_provider_fixture(&base_url, "component-provider-disconnect.db").await;
    let response = call_component_provider(&state, &key).await;
    assert_eq!(response.0, StatusCode::BAD_GATEWAY, "{}", response.1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "a component-prepared method is not replayable after the server accepts it"
    );

    let authenticated = state
        .db
        .authenticate_key(&key, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let requests = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let requests = state
                .db
                .list_requests(authenticated.key_id, 10)
                .await
                .unwrap();
            if requests.len() == 1 && requests[0].operational_terminal.is_some() {
                break requests;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("plugin disconnect reached its operational terminal");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].status_code, Some(502));
    assert_eq!(
        requests[0].error_code.as_deref(),
        Some("upstream_connection")
    );
}

async fn wait_for_bound_plugin_archives(state: &AppState, request_ids: &[Uuid]) {
    assert!(!request_ids.is_empty());
    let (shutdown, receiver) = watch::channel(false);
    let worker = tokio::spawn(worker::run_until_shutdown(state.clone(), receiver));
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::get("/internal/v1/request-events?after_event_at=0")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", state.config.service_token),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let mut pending = request_ids.to_vec();
    let mut archive_events_pending = request_ids.to_vec();
    let mut financial_pending = request_ids.to_vec();
    let mut observed = Vec::new();
    let mut stream = response.into_body().into_data_stream();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut buffered = String::new();
        while let Some(chunk) = stream.next().await {
            buffered.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            while let Some(end) = buffered.find("\n\n") {
                let frame = buffered[..end].to_owned();
                buffered.drain(..end + 2);
                let Some(data) = frame
                    .lines()
                    .find_map(|line| line.strip_prefix("data:").map(str::trim_start))
                else {
                    continue;
                };
                let Ok(event) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                let Some(request_id) = event["request_id"]
                    .as_str()
                    .and_then(|value| Uuid::parse_str(value).ok())
                else {
                    continue;
                };
                if request_ids.contains(&request_id) {
                    observed.push(json!({
                        "request_id": request_id,
                        "event_id": event["event_id"],
                        "event_at": event["event_at"],
                        "event_kind": event["event_kind"],
                        "archive_state": event["archive_state"],
                    }));
                }
                if event["event_kind"] == "archive_bound" {
                    archive_events_pending.retain(|candidate| *candidate != request_id);
                }
                if event["archive_state"] == "bound" {
                    pending.retain(|candidate| *candidate != request_id);
                }
                if event["event_kind"] == "finished" {
                    financial_pending.retain(|candidate| *candidate != request_id);
                }
                if pending.is_empty()
                    && archive_events_pending.is_empty()
                    && financial_pending.is_empty()
                {
                    return;
                }
            }
        }
    })
    .await;
    if result.is_err() {
        eprintln!("plugin completion observed events: {observed:?}");
        let diagnostic = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let pool = sqlx::AnyPool::connect(&state.config.database_url)
                .await
                .unwrap();
            let mut spools = Vec::new();
            for request_id in request_ids {
                let rows = sqlx::query(
                    "SELECT 'request' AS purpose, state, attempts, next_attempt_at, last_error_code, byte_count FROM request_archive_spools WHERE request_id = $1 UNION ALL SELECT 'response' AS purpose, state, attempts, next_attempt_at, last_error_code, byte_count FROM response_archive_spools WHERE request_id = $1",
                )
                .bind(request_id.to_string())
                .fetch_all(&pool)
                .await
                .unwrap();
                for row in rows {
                    spools.push(json!({
                        "request_id": request_id,
                        "observed_at": unix_millis(),
                        "purpose": row.get::<String, _>("purpose"),
                        "state": row.get::<String, _>("state"),
                        "attempts": row.get::<i64, _>("attempts"),
                        "next_attempt_at": row.get::<i64, _>("next_attempt_at"),
                        "last_error_code": row.get::<Option<String>, _>("last_error_code"),
                        "byte_count": row.get::<i64, _>("byte_count"),
                    }));
                }
            }
            pool.close().await;
            spools
        })
        .await;
        eprintln!("plugin completion spools after timeout: {diagnostic:?}");
    }
    assert!(
        result.is_ok(),
        "plugin completion barrier: unbound={pending:?}, missing_archive_events={archive_events_pending:?}, missing_financial_events={financial_pending:?}"
    );
    assert!(pending.is_empty(), "missing archive bindings: {pending:?}");
    assert!(
        archive_events_pending.is_empty(),
        "missing archive events: {archive_events_pending:?}"
    );
    assert!(
        financial_pending.is_empty(),
        "missing financial completions: {financial_pending:?}"
    );
    shutdown.send(true).unwrap();
    worker.await.unwrap();
}

async fn call_component_provider(state: &AppState, key: &str) -> (StatusCode, Value) {
    let response = api::router_for_role(state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/chat/completions")
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "model": "requested-model",
                        "messages": [{"role": "user", "content": "canonical"}],
                        "stream": false,
                        "max_tokens": 32
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}
