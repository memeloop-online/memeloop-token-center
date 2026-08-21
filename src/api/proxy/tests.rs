use super::*;

use axum::{
    body::{Body, to_bytes},
    http::{Request, header},
};
use rust_decimal::Decimal;
use sqlx::Row;
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, header as header_matcher, method, path},
};

use crate::{
    api::router_for_role,
    config::{ArchiveBackend, Config, RuntimeRole},
    db::{CreateKeyInput, CreateModelRouteInput, CreateUpstreamAccountInput},
    model::KeyPolicy,
};

#[test]
fn later_billable_stream_chunks_upgrade_nonbillable_lifecycle_delivery() {
    let mut delivered_any = false;
    let mut delivered_billable = false;
    record_delivered_chunk(&mut delivered_any, &mut delivered_billable, false);
    assert!(delivered_any);
    assert!(!delivered_billable);
    record_delivered_chunk(&mut delivered_any, &mut delivered_billable, true);
    assert!(delivered_any);
    assert!(delivered_billable);
}

#[test]
fn buffered_usage_capture_only_accepts_plausible_json_content_types() {
    assert!(should_capture_buffered_usage(false, None));
    assert!(should_capture_buffered_usage(
        false,
        Some(&HeaderValue::from_static("application/json; charset=utf-8"))
    ));
    assert!(should_capture_buffered_usage(
        false,
        Some(&HeaderValue::from_static("application/problem+json"))
    ));
    assert!(!should_capture_buffered_usage(
        false,
        Some(&HeaderValue::from_static("application/octet-stream"))
    ));
    assert!(!should_capture_buffered_usage(
        true,
        Some(&HeaderValue::from_static("text/event-stream"))
    ));
}

#[test]
fn trusted_input_overhead_is_http_json_only_and_defaults_to_zero() {
    assert_eq!(
        trusted_input_token_overhead_ceiling(
            Some("http-json"),
            Some(&json!({"base_url": "https://example.com"})),
        )
        .unwrap(),
        0
    );
    assert_eq!(
        trusted_input_token_overhead_ceiling(
            Some(codex_transport::DRIVER),
            Some(&json!({"input_token_overhead_ceiling": 1_000_000})),
        )
        .unwrap(),
        0,
        "direct Codex transport must not double-count compatible-upstream overhead"
    );
    assert!(
        trusted_input_token_overhead_ceiling(
            Some("http-json"),
            Some(&json!({"input_token_overhead_ceiling": 1_000_001})),
        )
        .is_err()
    );
}

#[test]
fn json_and_sse_usage_parsing_contracts_are_preserved() {
    let json_body = br#"{"usage":{"input_tokens":12,"output_tokens":34}}"#;
    let ExtractedUsage::Valid(json_usage) = extract_usage_checked(json_body) else {
        panic!("JSON usage must remain parseable");
    };
    assert_eq!(
        json_usage,
        TokenUsage {
            input_tokens: 12,
            output_tokens: 34,
            ..TokenUsage::default()
        }
    );

    let mut sse = ResponsesSseCapture::default();
    sse.push(
        b"event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":34}}}\n\n",
    );
    let summary = sse.finish_summary();
    assert_eq!(
        summary.usage,
        Some(TokenUsage {
            input_tokens: 12,
            output_tokens: 34,
            ..TokenUsage::default()
        })
    );
    assert!(!summary.usage_invalid);
}

struct CodexRouteFixture {
    state: AppState,
    database_url: String,
    archive_path: std::path::PathBuf,
    key: String,
    key_id: Uuid,
    upstream_account_id: Uuid,
    route_id: Uuid,
    model: String,
    upstream_model: String,
    _directory: tempfile::TempDir,
}

async fn codex_route_fixture(label: &str) -> CodexRouteFixture {
    let directory = tempfile::tempdir().unwrap();
    let archive_path = directory.path().join("archive");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join(format!("codex-{label}.db")).display()
    );
    let mut config = Config::for_test(database_url.clone());
    config.archive_backend = ArchiveBackend::Filesystem;
    config.archive_path = Some(archive_path.display().to_string());
    let state = AppState::initialize(config).await.unwrap();
    let tenant = format!("codex-route-{label}");
    let model = format!("codex-public-{label}");
    let upstream_model = format!("gpt-codex-{label}");
    let upstream = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: format!("codex-{label}"),
                driver: codex_transport::DRIVER.to_owned(),
                config: json!({
                    "base_url": codex_transport::BASE_URL,
                    "network_scope": "public",
                    "reservation_token_bounds": {upstream_model.clone(): 64}
                }),
                credential: UpstreamCredential::OAuth {
                    access_token: "upstream-access-secret".to_owned(),
                    refresh_token: Some("upstream-refresh-secret".to_owned()),
                    expires_at: Some(i64::MAX),
                    header: "authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                    adapter_state: Some(json!({
                        "schema": "cpa-codex-oauth-v1",
                        "account_id": "account-123"
                    })),
                },
                oauth_session_id: None,
                oauth_driver: Some(codex_transport::DRIVER.to_owned()),
                oauth_refresh_url: Some(crate::oauth::managed::codex::TOKEN_ENDPOINT.to_owned()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.clone(),
            public_model: model.clone(),
            upstream_account_id: upstream.id,
            upstream_model: upstream_model.clone(),
            protocol: "openai".to_owned(),
            priority: 0,
        })
        .await
        .unwrap();
    let issued = state
        .db
        .create_key_with_routing(
            CreateKeyInput {
                tenant_external_id: tenant,
                principal_external_id: "member".to_owned(),
                alias: format!("codex-{label}"),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    allowed_models: vec![model.clone()],
                    max_concurrency: 4,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            &[route.id],
            &[],
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    state
        .db
        .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    CodexRouteFixture {
        state,
        database_url,
        archive_path,
        key: issued.key,
        key_id: issued.key_id,
        upstream_account_id: upstream.id,
        route_id: route.id,
        model,
        upstream_model,
        _directory: directory,
    }
}

async fn response_usage_fixture(
    label: &str,
    upstream: &MockServer,
    input_token_overhead_ceiling: i64,
) -> CodexRouteFixture {
    let directory = tempfile::tempdir().unwrap();
    let archive_path = directory.path().join("archive");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory
            .path()
            .join(format!("compatibility-{label}.db"))
            .display()
    );
    let mut config = Config::for_test(database_url.clone());
    config.archive_backend = ArchiveBackend::Filesystem;
    config.archive_path = Some(archive_path.display().to_string());
    let state = AppState::initialize(config).await.unwrap();
    let tenant = format!("compatibility-route-{label}");
    let model = "gpt-5.6-sol".to_owned();
    let upstream_account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: format!("compatibility-{label}"),
                driver: "http-json".to_owned(),
                config: json!({
                    "base_url": upstream.uri(),
                    "network_scope": "public",
                    "input_token_overhead_ceiling": input_token_overhead_ceiling
                }),
                credential: UpstreamCredential::ApiKey {
                    value: "compatibility-upstream-secret".to_owned(),
                    header: "authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                },
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.clone(),
            public_model: model.clone(),
            upstream_account_id: upstream_account.id,
            upstream_model: model.clone(),
            protocol: "openai".to_owned(),
            priority: 0,
        })
        .await
        .unwrap();
    let issued = state
        .db
        .create_key_with_routing(
            CreateKeyInput {
                tenant_external_id: tenant,
                principal_external_id: "member".to_owned(),
                alias: format!("compatibility-{label}"),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    allowed_models: vec![model.clone()],
                    max_concurrency: 4,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            &[route.id],
            &[],
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    state
        .db
        .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    CodexRouteFixture {
        state,
        database_url,
        archive_path,
        key: issued.key,
        key_id: issued.key_id,
        upstream_account_id: upstream_account.id,
        route_id: route.id,
        model: model.clone(),
        upstream_model: model,
        _directory: directory,
    }
}

async fn send_response_usage_request(fixture: &CodexRouteFixture, body: &Value) -> Response {
    router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

fn archive_file_count(root: &std::path::Path) -> usize {
    std::fs::read_dir(root)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| {
                    if entry.path().is_dir() {
                        archive_file_count(&entry.path())
                    } else {
                        1
                    }
                })
                .sum()
        })
        .unwrap_or_default()
}

async fn send_codex_route(
    fixture: &CodexRouteFixture,
    upstream: &MockServer,
    path: &str,
    body: Value,
) -> Response {
    let request = Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
        .header(header::ACCEPT, "application/json")
        .header("originator", "downstream-spoof")
        .header("session_id", "downstream-spoof")
        .header("chatgpt-account-id", "downstream-spoof")
        .header("anthropic-version", "downstream-spoof")
        .header("anthropic-beta", "downstream-spoof")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    codex_transport::with_test_endpoint(
        upstream.uri(),
        router_for_role(fixture.state.clone(), RuntimeRole::Gateway).oneshot(request),
    )
    .await
    .unwrap()
}

async fn wait_for_request_settlement(fixture: &CodexRouteFixture, expected: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let rows = fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap();
            if rows.len() == expected && rows.iter().all(|row| row.status_code.is_some()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn assert_exactly_once_side_effects(
    fixture: &CodexRouteFixture,
    request_id: Uuid,
    expected_response_id: Option<&str>,
) {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let row = sqlx::query(
        "SELECT q.upstream_account_id, q.model_route_id, r.status AS reservation_status, (SELECT COUNT(*) FROM usage_reservations x WHERE x.id = q.reservation_id) AS reservation_count, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = q.reservation_id) AS ledger_count, (SELECT COUNT(*) FROM request_stats_facts f WHERE f.request_id = q.id) AS fact_count, (SELECT COUNT(*) FROM request_events e WHERE e.request_id = q.id AND e.event_kind = 'finished') AS event_count, (SELECT COUNT(*) FROM conversation_observations o WHERE o.request_id = q.id) AS observation_count, (SELECT COUNT(*) FROM conversation_observations o WHERE o.request_id = q.id AND o.upstream_response_id = $2) AS response_observation_count FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.id = $1",
    )
    .bind(request_id.to_string())
    .bind(expected_response_id.unwrap_or(""))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row.get::<String, _>("upstream_account_id"),
        fixture.upstream_account_id.to_string()
    );
    assert_eq!(
        row.get::<String, _>("model_route_id"),
        fixture.route_id.to_string()
    );
    assert_eq!(row.get::<String, _>("reservation_status"), "settled");
    for field in [
        "reservation_count",
        "ledger_count",
        "fact_count",
        "event_count",
        "observation_count",
    ] {
        assert_eq!(row.get::<i64, _>(field), 1, "{field}");
    }
    assert_eq!(
        row.get::<i64, _>("response_observation_count"),
        i64::from(expected_response_id.is_some())
    );
    pool.close().await;
}

fn completed_codex_sse(output: &str) -> String {
    format!(
        concat!(
            "event: response.output_item.done\r\n",
            "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"id\":\"item-codex\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{output}\"}}]}}}}\r\n\r\n",
            "event: response.completed\n",
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp-codex\",\"object\":\"response\",\"output\":[{{\"id\":\"item-codex\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{output}\"}}]}}],\"usage\":{{\"input_tokens\":3,\"output_tokens\":2}}}}}}\n\n",
            "data: [DONE]\n\n"
        ),
        output = output
    )
}

fn assert_codex_wire(request: &wiremock::Request, upstream_model: &str) {
    assert_eq!(request.url.path(), codex_transport::RESPONSES_PATH);
    assert_eq!(request.headers[header::ACCEPT], "text/event-stream");
    assert_eq!(request.headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(
        request.headers[header::USER_AGENT],
        "codex-tui/0.146.0 (Mac OS 26.5.0; arm64) iTerm.app/3.6.10 (codex-tui; 0.146.0)"
    );
    assert_eq!(request.headers["originator"], "codex-tui");
    assert_eq!(request.headers["chatgpt-account-id"], "account-123");
    assert_eq!(
        request.headers[header::AUTHORIZATION],
        "Bearer upstream-access-secret"
    );
    assert_ne!(request.headers["session_id"], "downstream-spoof");
    assert!(request.headers.get("anthropic-version").is_none());
    assert!(request.headers.get("anthropic-beta").is_none());
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["model"], upstream_model);
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["instructions"], "");
    assert_eq!(body["input"][0]["role"], "developer");
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    for removed in [
        "temperature",
        "previous_response_id",
        "max_output_tokens",
        "stream_options",
    ] {
        assert!(body.get(removed).is_none(), "{removed}");
    }
}

#[tokio::test]
async fn codex_chat_and_embeddings_fail_before_reservation_archive_or_upstream() {
    let fixture = codex_route_fixture("preadmission").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    for (path, request) in [
        (
            "/v1/chat/completions",
            json!({"model": fixture.model, "messages": [{"role": "user", "content": "hello"}]}),
        ),
        (
            "/v1/embeddings",
            json!({"model": fixture.model, "input": "hello"}),
        ),
    ] {
        let response = send_codex_route(&fixture, &upstream, path, request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("Responses protocol only"), "{body}");
    }
    assert!(
        fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(archive_file_count(&fixture.archive_path), 0);
    assert!(upstream.received_requests().await.unwrap().is_empty());
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_reservations")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    pool.close().await;
}

#[tokio::test]
async fn malformed_codex_credential_and_config_fail_before_side_effects() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    for (label, corrupt_credential) in [("bad-credential", true), ("bad-config", false)] {
        let fixture = codex_route_fixture(label).await;
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        if corrupt_credential {
            let invalid = UpstreamCredential::OAuth {
                access_token: "access-secret".to_owned(),
                refresh_token: Some("refresh-secret".to_owned()),
                expires_at: Some(i64::MAX),
                header: "x-api-key".to_owned(),
                prefix: "Bearer ".to_owned(),
                adapter_state: Some(json!({
                    "schema": "cpa-codex-oauth-v1",
                    "account_id": "account-123"
                })),
            };
            let ciphertext = crate::provider::seal_credential(
                &invalid,
                fixture.state.config.key_pepper.as_bytes(),
            )
            .unwrap();
            sqlx::query(
                "UPDATE upstream_credentials SET credential_ciphertext = $1 WHERE upstream_account_id = $2",
            )
            .bind(ciphertext)
            .bind(fixture.upstream_account_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        } else {
            sqlx::query("UPDATE upstream_accounts SET config_json = $1 WHERE id = $2")
                .bind(
                    json!({
                        "base_url": codex_transport::BASE_URL,
                        "network_scope": "private",
                        "reservation_token_bounds": {fixture.upstream_model.clone(): 64}
                    })
                    .to_string(),
                )
                .bind(fixture.upstream_account_id.to_string())
                .execute(&pool)
                .await
                .unwrap();
        }
        let response = send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "hello"}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(archive_file_count(&fixture.archive_path), 0);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_reservations")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        pool.close().await;
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn codex_buffered_route_rewrites_wire_and_archives_final_json_once() {
    let fixture = codex_route_fixture("buffered").await;
    let upstream = MockServer::start().await;
    let sse = completed_codex_sse("buffered answer");
    codex_transport::parse_buffered_sse_for_test(sse.as_bytes()).unwrap();
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("accept", "text/event-stream"))
        .and(header_matcher("originator", "codex-tui"))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .and(body_partial_json(json!({
            "model": fixture.upstream_model,
            "stream": true,
            "store": false,
            "parallel_tool_calls": true
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse.into_bytes(), "text/event-stream; charset=utf-8"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let original = json!({
        "model": fixture.model,
        "input": [{"role": "system", "content": "be concise"}],
        "stream": false,
        "temperature": 0.7,
        "previous_response_id": "resp-parent",
        "include": ["reasoning.encrypted_content", "reasoning.encrypted_content"]
    });
    let response = send_codex_route(&fixture, &upstream, "/v1/responses", original.clone()).await;
    let status = response.status();
    if status != StatusCode::OK {
        let failure = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        panic!(
            "buffered status {status}, body={}, upstream_requests={}, rows={:?}",
            String::from_utf8_lossy(&failure),
            upstream.received_requests().await.unwrap().len(),
            fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap()
        );
    }
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["id"], "resp-codex");
    assert_eq!(body["output"][0]["content"][0]["text"], "buffered answer");

    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 2));
    assert_eq!(rows[0].cost, "0.000005");
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex")).await;
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    let archived_request: Value = serde_json::from_slice(
        &fixture
            .state
            .archive
            .get(&refs.request_object)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(archived_request, original);
    let archived_response: Value = serde_json::from_slice(
        &fixture
            .state
            .archive
            .get(refs.response_object.as_deref().unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(archived_response, body);
    assert!(!archived_response.to_string().contains("response.completed"));
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_codex_wire(&requests[0], &fixture.upstream_model);
}

#[tokio::test]
async fn codex_streaming_route_preserves_sse_and_settles_usage_once() {
    let fixture = codex_route_fixture("streaming").await;
    let upstream = MockServer::start().await;
    let sse = completed_codex_sse("streamed answer");
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse.clone().into_bytes(), "text/event-stream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let original = json!({
        "model": fixture.model,
        "input": [{"role": "system", "content": "stream"}],
        "stream": true
    });
    let response = send_codex_route(&fixture, &upstream, "/v1/responses", original.clone()).await;
    let status = response.status();
    if status != StatusCode::OK {
        let failure = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        panic!(
            "streaming status {status}, body={}, upstream_requests={}, rows={:?}",
            String::from_utf8_lossy(&failure),
            upstream.received_requests().await.unwrap().len(),
            fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap()
        );
    }
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), sse.as_bytes());
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 2));
    assert_eq!(rows[0].cost, "0.000005");
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex")).await;
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    let archived_request = fixture
        .state
        .archive
        .get(&refs.request_object)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&archived_request).unwrap(),
        original
    );
    let archived_response = fixture
        .state
        .archive
        .get(refs.response_object.as_deref().unwrap())
        .await
        .unwrap();
    assert_eq!(archived_response.as_ref(), sse.as_bytes());
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_codex_wire(&requests[0], &fixture.upstream_model);
}

#[tokio::test]
async fn codex_streaming_failure_is_redacted_for_client_and_archive() {
    let fixture = codex_route_fixture("stream-failure").await;
    let upstream = MockServer::start().await;
    let failed = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-failed\"}}\n\n",
        "event: response.failed\n",
        "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"provider-secret\",\"token\":\"secret-token\"}}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"post-terminal-secret\"}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(failed.as_bytes().to_vec(), "text/event-stream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "fail safely", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let rendered = String::from_utf8(body.to_vec()).unwrap();
    assert!(rendered.contains("upstream request failed"));
    for secret in ["provider-secret", "secret-token", "post-terminal-secret"] {
        assert!(!rendered.contains(secret));
    }
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (0, 0));
    assert_eq!(rows[0].cost, "0");
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_failed_response")
    );
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    let archived = fixture
        .state
        .archive
        .get(refs.response_object.as_deref().unwrap())
        .await
        .unwrap();
    let archived = String::from_utf8(archived.to_vec()).unwrap();
    assert_eq!(archived, rendered);
    for secret in ["provider-secret", "secret-token", "post-terminal-secret"] {
        assert!(!archived.contains(secret));
    }
}

#[tokio::test]
async fn codex_streaming_output_then_failure_charges_the_contract_ceiling_once() {
    let fixture = codex_route_fixture("stream-output-failure").await;
    let upstream = MockServer::start().await;
    let failed = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-failed\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"billable output\"}\n\n",
        "event: response.failed\n",
        "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"provider-secret\"}}}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(failed.as_bytes().to_vec(), "text/event-stream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "partially delivered", "stream": true}),
    )
    .await;
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let rendered = String::from_utf8(body.to_vec()).unwrap();
    assert!(rendered.contains("billable output"));
    assert!(!rendered.contains("provider-secret"));
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(502));
    assert!(rows[0].input_tokens > 0);
    assert_eq!(rows[0].output_tokens, 64);
    assert_ne!(rows[0].cost, "0");
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
}

fn completed_response_with_usage(input_tokens: i64, output_tokens: i64) -> Value {
    json!({
        "id": "resp-usage-contract",
        "object": "response",
        "status": "completed",
        "error": null,
        "incomplete_details": null,
        "output": [{
            "id": "msg-usage-contract",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "cutover-ok", "annotations": []}]
        }],
        "usage": {
            "input_tokens": input_tokens,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": output_tokens,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": input_tokens + output_tokens
        }
    })
}

#[tokio::test]
async fn buffered_response_usage_uses_trusted_overhead_and_settles_http_200() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(completed_response_with_usage(309, 7)),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("buffered-response-usage", &upstream, 256).await;
    let request = json!({
        "model": fixture.model,
        "input": "xxxxxxxxxxxxxxxxxxxxxxxxxx",
        "stream": false,
        "max_output_tokens": 16
    });
    assert_eq!(serde_json::to_vec(&request).unwrap().len(), 98);
    let response = send_response_usage_request(&fixture, &request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        completed_response_with_usage(309, 7)
    );
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (309, 7));
    assert_eq!(rows[0].error_code, None);
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-usage-contract"))
        .await;
}

#[tokio::test]
async fn buffered_response_usage_rejects_usage_above_total_reservation() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(completed_response_with_usage(400, 7)),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("buffered-over-reservation", &upstream, 256).await;
    let request = json!({
        "model": fixture.model,
        "input": "xxxxxxxxxxxxxxxxxxxxxxxxxx",
        "stream": false,
        "max_output_tokens": 16
    });
    assert_eq!(serde_json::to_vec(&request).unwrap().len(), 98);
    let response = send_response_usage_request(&fixture, &request).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_invalid_usage")
    );
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (0, 0));
}

#[tokio::test]
async fn buffered_failed_response_is_redacted_and_settled_as_502() {
    let upstream = MockServer::start().await;
    let mut failed = completed_response_with_usage(309, 7);
    failed["status"] = json!("failed");
    failed["error"] = json!({"message": "provider-secret", "token": "secret-token"});
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(failed))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("buffered-failure", &upstream, 256).await;
    let request = json!({
        "model": fixture.model,
        "input": "xxxxxxxxxxxxxxxxxxxxxxxxxx",
        "stream": false,
        "max_output_tokens": 16
    });
    let response = send_response_usage_request(&fixture, &request).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let rendered = String::from_utf8(body.to_vec()).unwrap();
    assert!(rendered.contains("upstream request failed"));
    assert!(!rendered.contains("provider-secret"));
    assert!(!rendered.contains("secret-token"));
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_failed_response")
    );
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (0, 0));
}

#[tokio::test]
async fn streaming_response_error_null_completes_and_non_null_error_fails() {
    for (label, input_tokens, response_error, expected_status, expected_error) in [
        ("stream-null-error", 309, Value::Null, 200, None),
        (
            "stream-non-null-error",
            309,
            json!({"message": "provider-secret"}),
            502,
            Some("upstream_failed_response"),
        ),
        (
            "stream-over-reservation",
            400,
            Value::Null,
            502,
            Some("upstream_invalid_usage"),
        ),
    ] {
        let upstream = MockServer::start().await;
        let completed = completed_response_with_usage(input_tokens, 7);
        let response_error_is_null = response_error.is_null();
        let sse = format!(
            concat!(
                "event: response.created\n",
                "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-usage-contract\",\"error\":null}}}}\n\n",
                "event: response.completed\n",
                "data: {{\"type\":\"response.completed\",\"response\":{completed}}}\n\n",
                "data: [DONE]\n\n"
            ),
            completed = {
                let mut value = completed;
                value["error"] = response_error;
                value
            }
        );
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;
        let fixture = response_usage_fixture(label, &upstream, 256).await;
        let request = json!({
            "model": fixture.model,
            "input": "xxxxxxxxxxxxxxxxxxxxxxxxxx",
            "stream": true,
            "max_output_tokens": 16
        });
        let response = send_response_usage_request(&fixture, &request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        let rendered = String::from_utf8(body.to_vec()).unwrap();
        assert!(rendered.contains("response.created"));
        if response_error_is_null {
            assert!(rendered.contains("response.completed"));
            assert!(rendered.contains("\"error\":null"));
        } else {
            assert!(rendered.contains("upstream request failed"));
            assert!(!rendered.contains("provider-secret"));
        }
        wait_for_request_settlement(&fixture, 1).await;
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows[0].status_code, Some(expected_status));
        assert_eq!(rows[0].error_code.as_deref(), expected_error);
        if expected_status == 200 {
            assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (309, 7));
        } else {
            assert_ne!(rows[0].cost, "0", "delivered failed streams are billed");
            assert_eq!(rows[0].output_tokens, 16);
        }
    }
}

#[test]
fn responses_sse_requires_terminal_event_and_payload_to_match() {
    let mut capture = ResponsesSseCapture::for_responses();
    capture.push(
        b"event: response.failed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-mismatch\",\"error\":null}}\n\n",
    );
    assert_eq!(capture.finish(), ResponsesSseOutcome::Failed);
}

#[test]
fn upstream_usage_rejects_negative_and_extreme_token_counts() {
    assert!(matches!(
        extract_usage_checked(
            b"data: {\"type\":\"response.output_text.delta\",\"usage\":null}\n\ndata: [DONE]\n\n"
        ),
        ExtractedUsage::Missing
    ));
    assert!(matches!(
        extract_usage_checked(
            b"data: {\"usage\":null}\n\ndata: {\"usage\":{\"input_tokens\":12,\"output_tokens\":34}}\n\ndata: [DONE]\n\n"
        ),
        ExtractedUsage::Valid(TokenUsage {
            input_tokens: 12,
            output_tokens: 34,
            ..
        })
    ));
    assert_eq!(
        usage_from_value(&json!({"usage":{"input_tokens":12,"output_tokens":34}})),
        Some(TokenUsage {
            input_tokens: 12,
            output_tokens: 34,
            ..TokenUsage::default()
        })
    );
    assert_eq!(
        usage_from_value(&json!({"usage":{"input_tokens":-1,"output_tokens":34}})),
        None
    );
    assert_eq!(
        usage_from_value(&json!({"usage":{"input_tokens":12,"output_tokens":1000000001_i64}})),
        None
    );
    assert_eq!(
        usage_from_value(&json!({
            "service_tier": "priority",
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "prompt_tokens_details": {"cached_tokens": 40}
            }
        })),
        Some(TokenUsage {
            input_tokens: 60,
            cached_input_tokens: 40,
            cache_write_tokens: 0,
            output_tokens: 20,
            service_tier: Some("priority".to_owned()),
        })
    );
    assert_eq!(
        usage_from_value(&json!({
            "type": "message_start",
            "message": {"usage": {
                "input_tokens": 60,
                "cache_read_input_tokens": 30,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 7,
                    "ephemeral_1h_input_tokens": 3
                },
                "output_tokens": 2
            }}
        })),
        Some(TokenUsage {
            input_tokens: 60,
            cached_input_tokens: 30,
            cache_write_tokens: 10,
            output_tokens: 2,
            service_tier: None,
        })
    );
    assert_eq!(
        usage_from_value(&json!({
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 1,
                "prompt_tokens_details": {"cached_tokens": 11}
            }
        })),
        None,
        "an inclusive OpenAI input count cannot be smaller than cached input"
    );
}

#[test]
fn response_archive_cleanup_requires_conclusive_database_ownership() {
    let stored = "staging/proxy/00000000-0000-0000-0000-000000000000/response/00000000-0000-0000-0000-000000000001/body";
    assert!(!response_archive_requires_cleanup(
        &Err(AppError::Internal),
        stored
    ));
    assert!(response_archive_requires_cleanup(
        &Err(AppError::Conflict("fenced".to_owned())),
        stored
    ));
    assert!(!response_archive_requires_cleanup(
        &Ok(FinishProxyRequestResult::Finished {
            cost_micros: 1,
            usage_invalid: false,
        }),
        stored
    ));
    assert!(!response_archive_requires_cleanup(
        &Ok(FinishProxyRequestResult::AlreadyFinished {
            status_code: 200,
            cost_micros: 1,
            error_code: None,
            response_object: stored.to_owned(),
        }),
        stored
    ));
    assert!(response_archive_requires_cleanup(
        &Ok(FinishProxyRequestResult::AlreadyFinished {
            status_code: 200,
            cost_micros: 1,
            error_code: None,
            response_object: "staging/proxy/00000000-0000-0000-0000-000000000000/response/00000000-0000-0000-0000-000000000002/body".to_owned(),
        }),
        stored
    ));
}

#[test]
fn response_id_supports_buffered_and_sse_responses_without_scanning_plain_text() {
    assert_eq!(
        extract_response_id(br#"{"id":"resp-buffered","object":"response"}"#).as_deref(),
        Some("resp-buffered")
    );
    assert_eq!(
        extract_response_id(
            b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-streamed\"}}\n\ndata: [DONE]\n"
        )
        .as_deref(),
        Some("resp-streamed")
    );
    assert_eq!(
        extract_response_id(
            b"data: {\"type\":\"response.output_item.added\",\"id\":\"item-id\"}\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-preferred\"}}\n"
        )
        .as_deref(),
        Some("resp-preferred")
    );
    assert_eq!(
        extract_response_id(
            b"ordinary text data: {\"response\":{\"id\":\"forged\"}}\ndata: [DONE]\n"
        ),
        None
    );
    let mut outside_scan_limit = vec![b'x'; 2 * 1024 * 1024];
    outside_scan_limit
        .extend_from_slice(b"\ndata: {\"response\":{\"id\":\"outside-scan-limit\"}}\n");
    assert_eq!(extract_response_id(&outside_scan_limit), None);
}

#[test]
fn responses_sse_capture_handles_split_crlf_lines_and_multiple_events() {
    let stream = concat!(
        "event: response.created\r\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-split\"}}\r\n",
        "\r\n",
        "data: {\"type\":\"response.output_item.added\",\"id\":\"item-not-response\"}\n\n",
        "event: response.completed\r\n",
        "data: {\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\r\n",
        "\r\n"
    );
    let mut capture = ResponsesSseCapture::default();
    for chunk in stream.as_bytes().chunks(1) {
        capture.push(chunk);
    }
    assert_eq!(
        capture.finish(),
        ResponsesSseOutcome::Completed {
            response_id: Some("resp-split".to_owned())
        }
    );
}

#[test]
fn responses_sse_capture_preserves_early_usage_across_a_large_stream() {
    let mut capture = ResponsesSseCapture::default();
    capture.push(
        b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":60,\"output_tokens\":0}}}\n\n",
    );
    let padding = "x".repeat(1024);
    for _ in 0..2_100 {
        capture.push(
            format!(
                "data: {{\"type\":\"content_block_delta\",\"delta\":{{\"text\":\"{padding}\"}},\"usage\":null}}\n\n"
            )
            .as_bytes(),
        );
    }
    capture.push(
        b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":20}}\n\ndata: [DONE]\n\n",
    );
    let summary = capture.finish_summary();
    assert_eq!(
        summary.outcome,
        ResponsesSseOutcome::Completed { response_id: None }
    );
    assert_eq!(
        summary.usage,
        Some(TokenUsage {
            input_tokens: 60,
            output_tokens: 20,
            ..TokenUsage::default()
        })
    );
    assert!(!summary.usage_invalid);
}

#[test]
fn responses_sse_capture_requires_an_unambiguous_success_terminal() {
    let created =
        b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-created\"}}\n\n";
    let mut truncated = ResponsesSseCapture::default();
    truncated.push(created);
    assert_eq!(truncated.finish(), ResponsesSseOutcome::Incomplete);

    let mut failed = ResponsesSseCapture::default();
    failed.push(created);
    failed.push(b"data: {\"type\":\"response.failed\"}\n\ndata: [DONE]\n\n");
    assert_eq!(failed.finish(), ResponsesSseOutcome::Failed);

    let mut conflicting = ResponsesSseCapture::default();
    conflicting.push(created);
    conflicting
        .push(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-other\"}}\n\n");
    assert_eq!(conflicting.finish(), ResponsesSseOutcome::Incomplete);
}

#[test]
fn response_id_is_not_observed_after_http_or_transport_failure() {
    let response_id = Some("resp-never-observed".to_owned());
    assert_eq!(
        completed_response_id(
            StatusCode::BAD_GATEWAY,
            true,
            true,
            response_id.clone(),
            &[],
        ),
        None
    );
    assert_eq!(
        completed_response_id(StatusCode::OK, false, true, response_id, &[]),
        None
    );
}

#[test]
fn responses_sse_capture_bounds_and_skips_an_oversized_single_event() {
    let mut capture = ResponsesSseCapture::default();
    capture.push(
        b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-before-large\"}}\n\n",
    );
    capture.push(b"data: ");
    let oversized_event = vec![b'x'; MAX_RESPONSES_SSE_EVENT_BYTES + 1];
    capture.push(&oversized_event);
    assert!(capture.line.len() <= MAX_RESPONSES_SSE_EVENT_BYTES);
    assert!(capture.data.len() <= MAX_RESPONSES_SSE_EVENT_BYTES);
    capture.push(b"\n\ndata: {\"type\":\"response.completed\"}\n\n");
    assert_eq!(capture.finish(), ResponsesSseOutcome::Incomplete);

    let mut failed = ResponsesSseCapture::default();
    failed.push(
        b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-before-failure\"}}\n\nevent: response.failed\ndata: ",
    );
    failed.push(&oversized_event);
    failed.push(b"\n\ndata: [DONE]\n\n");
    assert_eq!(failed.finish(), ResponsesSseOutcome::Failed);
}
