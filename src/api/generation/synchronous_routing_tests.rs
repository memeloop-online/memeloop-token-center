use super::*;
use crate::{
    config::Config,
    db::{
        CreateGroupInput, GroupKind, GroupRoutingStrategy, ReplaceGroupMembersInput,
        UpdateGroupRoutingStrategyInput, UpstreamFailureKind,
    },
    plugin::PluginRuntime,
};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use sqlx::Row;
use std::fs;
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

struct Fixture {
    state: AppState,
    credential: String,
    key_id: Uuid,
    tenant: Uuid,
    account: Uuid,
    database_url: String,
    _directory: tempfile::TempDir,
}

fn request_json() -> Value {
    json!({"model":"image-replay-model","prompt":"draw a fox","n":1,"size":"1024x1024"})
}

#[tokio::test]
async fn native_image_parameter_validation_does_not_restrict_openai_forwarding() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created":1,"data":[{"b64_json":"bW9jay1wbmc="}]
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = fixture(&upstream).await;
    let mut payload = request_json();
    payload["quality"] = json!("high");
    payload["background"] = json!("transparent");
    payload["output_format"] = json!("png");
    payload["user"] = json!("fixture-user");
    let response = router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/images/generations")
                .header(header::CONTENT_TYPE, "application/json")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", fixture.credential),
                )
                .header("idempotency-key", "openai-parameter-forwarding")
                .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let requests = upstream.received_requests().await.unwrap();
    let forwarded: Value = requests[0].body_json().unwrap();
    for field in ["quality", "background", "output_format", "user"] {
        assert_eq!(forwarded[field], payload[field]);
    }
}

async fn post(state: AppState, credential: &str, idempotency: &str) -> Response {
    router_for_role(state, RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/images/generations")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {credential}"))
                .header("idempotency-key", idempotency)
                .body(Body::from(serde_json::to_vec(&request_json()).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

fn component(plan: &Value, observation: &Value) -> Vec<u8> {
    let plan = plan.to_string();
    let observation = observation.to_string();
    let escape = |value: &str| {
        value
            .bytes()
            .map(|byte| format!("\\{byte:02x}"))
            .collect::<String>()
    };
    let output = |offset, len| {
        format!(
            "i32.const 32 i32.const 0 i32.store i32.const 36 i32.const {offset} i32.store i32.const 40 i32.const {len} i32.store i32.const 32"
        )
    };
    let source = format!(
        r#"(module
      (memory (export "memory") 2)
      (global $heap (mut i32) (i32.const 8192))
      (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
        (local $result i32) global.get $heap local.set $result
        global.get $heap local.get 3 i32.add global.set $heap local.get $result)
      (data (i32.const 1024) "{}") (data (i32.const 4096) "{}")
      (func (export "memeloop:token-center/group-routing-v1@0.2.0#plan") (param i32 i32) (result i32) {})
      (func (export "memeloop:token-center/group-routing-v1@0.2.0#observe") (param i32 i32) (result i32) {})
    )"#,
        escape(&plan),
        escape(&observation),
        output(1024, plan.len()),
        output(4096, observation.len())
    );
    let mut module = wat::parse_str(source).unwrap();
    let mut resolve = Resolve::default();
    let (package, _) = resolve.push_path("wit/token-center.wit").unwrap();
    let world = resolve
        .select_world(&[package], Some("group-routing-plugin"))
        .unwrap();
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8).unwrap();
    ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

async fn fixture(upstream: &MockServer) -> Fixture {
    fixture_with_archive(upstream, crate::config::ArchiveBackend::Memory).await
}

async fn fixture_with_archive(
    upstream: &MockServer,
    archive_backend: crate::config::ArchiveBackend,
) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("images.db").display()
    );
    let mut config = Config::for_test(database_url.clone());
    config.archive_backend = archive_backend;
    config.archive_path = Some(directory.path().join("archive").display().to_string());
    let mut state = AppState::initialize(config).await.unwrap();
    let tenant_external = "synchronous-strategy";
    let account=state.db.create_upstream_account(CreateUpstreamAccountInput {
        tenant_external_id:tenant_external.into(), name:"image mock".into(), driver:"http-json".into(),
        config:json!({"base_url":format!("{}/v1",upstream.uri()),"network_scope":"private"}),
        credential:UpstreamCredential::ApiKey { value:"image-fixture-secret".into(),header:"authorization".into(),prefix:"Bearer ".into() },oauth_session_id:None,oauth_driver:None,oauth_refresh_url:None,
    },state.config.key_pepper.as_bytes()).await.unwrap();
    let route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant_external.into(),
            public_model: "image-replay-model".into(),
            upstream_account_id: account.id,
            upstream_model: "image-mock".into(),
            protocol: "generation".into(),
            priority: 0,
        })
        .await
        .unwrap();
    state
        .db
        .upsert_generation_price("image-replay-model", "USD", "image", Decimal::new(3, 1))
        .await
        .unwrap();
    let issued = state
        .db
        .create_key_with_routing(
            CreateKeyInput {
                tenant_external_id: tenant_external.into(),
                principal_external_id: "image-user".into(),
                alias: "image-test".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            &[route.id],
            &[],
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let group = state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: tenant_external.into(),
                name: "image routing".into(),
            },
        )
        .await
        .unwrap();
    let group = state
        .db
        .replace_group_members(
            GroupKind::Provider,
            group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: tenant_external.into(),
                member_ids: vec![account.id],
                expected_updated_at: group.updated_at,
            },
        )
        .await
        .unwrap();
    let pool = sqlx::AnyPool::connect(&database_url).await.unwrap();
    sqlx::query("INSERT INTO model_route_included_provider_groups (tenant_id,model_route_id,provider_group_id,created_at) VALUES ($1,$2,$3,1)")
        .bind(group.tenant_id.to_string())
        .bind(route.id.to_string())
        .bind(group.id.to_string())
        .execute(&pool).await.unwrap();
    pool.close().await;
    state
        .db
        .update_group_routing_strategy(
            GroupKind::Provider,
            group.id,
            UpdateGroupRoutingStrategyInput {
                tenant_external_id: tenant_external.into(),
                expected_updated_at: group.updated_at,
                expected_strategy_version: group.strategy_version,
                routing_strategy: Some(GroupRoutingStrategy {
                    plugin_id: "image-routing".into(),
                    config: json!({}),
                }),
                routing_priority: 0,
            },
        )
        .await
        .unwrap();
    let directive = |cooldown| {
        json!({"tenant_id":group.tenant_id.to_string(),"route_id":route.id.to_string(),"account_id":account.id.to_string(),"generation":1,
        "allow_transient_probe":false,"cooldown_ms":cooldown,"recovery_wait_ms":0,"recheck_ms":100,"stickiness":false})
    };
    let root = directory.path().join("plugins");
    let package = root.join("image-routing");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("plugin.json"),serde_json::to_vec(&json!({"id":"image-routing","version":"1.0.0","wit_version":"0.2.0","wasm":"plugin.wasm","capabilities":[],
        "contributions":{"group_routing":{"version":"group-routing-v1","schema":{"type":"object","additionalProperties":false,"properties":{"tag":{"type":"string","maxLength":32}}},"default":{}}}})).unwrap()).unwrap();
    fs::write(
        package.join("plugin.wasm"),
        component(&json!({"candidates":[directive(0)]}), &directive(12345)),
    )
    .unwrap();
    state.plugins = PluginRuntime::load(root.to_str(), state.db.clone()).unwrap();
    Fixture {
        state,
        credential: issued.key,
        key_id: issued.key_id,
        tenant: group.tenant_id,
        account: account.id,
        database_url,
        _directory: directory,
    }
}

#[tokio::test]
async fn image_group_hard_quota_never_sends() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let fixture = fixture(&upstream).await;
    fixture
        .state
        .db
        .record_upstream_account_failure(
            fixture.account,
            1,
            UpstreamFailureKind::RateLimitedUntil {
                until: unix_millis() + 60_000,
                exhausted: true,
            },
        )
        .await
        .unwrap();
    let response = post(fixture.state.clone(), &fixture.credential, "hard-image").await;
    assert!(!response.status().is_success());
    assert!(upstream.received_requests().await.unwrap().is_empty());
    let health = fixture
        .state
        .db
        .group_routing_health(fixture.tenant, fixture.account, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(health.last_failure_kind, "quota_exhausted");
    assert_eq!(health.probe_lease_until, 0);
}

#[tokio::test]
async fn image_invalid_body_observes_once_and_same_key_remains_uncertain_after_expiry() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("invalid JSON", "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = fixture(&upstream).await;
    let response = post(fixture.state.clone(), &fixture.credential, "invalid-image").await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "image_submission_uncertain");
    assert_eq!(body["error"]["retryable"], false);
    assert_eq!(body["error"]["reconciliation_available"], true);
    let health = fixture
        .state
        .db
        .group_routing_health(fixture.tenant, fixture.account, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(health.last_failure_kind, "invalid_response");
    assert_eq!(health.cooldown_until - health.updated_at, 12345);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE synchronous_image_idempotency SET lease_expires_at = 0 WHERE key_id = $1")
        .bind(fixture.key_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let reservation=sqlx::query("SELECT r.status, q.submission_started_at, q.routing_snapshot FROM usage_reservations r JOIN request_records q ON q.reservation_id = r.id WHERE q.key_id = $1").bind(fixture.key_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(reservation.get::<String, _>("status"), "reserved");
    assert!(
        reservation
            .get::<Option<i64>, _>("submission_started_at")
            .is_some()
    );
    assert!(
        reservation
            .get::<Option<String>, _>("routing_snapshot")
            .is_some()
    );
    pool.close().await;
    let repeated = post(fixture.state.clone(), &fixture.credential, "invalid-image").await;
    assert_eq!(repeated.status(), StatusCode::CONFLICT);
    let replay: Value =
        serde_json::from_slice(&to_bytes(repeated.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert!(
        replay["error"]["reconciliation_available"].is_null(),
        "read-only replay must not claim publication readiness"
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn image_claim_expired_before_send_can_be_taken_over_once() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"created":1,"data":[{"b64_json":"bW9jay1wbmc="}]})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = fixture(&upstream).await;
    let idempotency = GenerationJobIdempotency {
        key: "before-send".into(),
        request_hash: crate::generation::generation_request_hash(
            "image-replay-model",
            &request_json(),
        ),
    };
    assert!(matches!(
        fixture
            .state
            .db
            .claim_synchronous_image_idempotency(fixture.key_id, &idempotency, Uuid::now_v7())
            .await
            .unwrap(),
        SynchronousImageIdempotencyClaim::Claimed
    ));
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE synchronous_image_idempotency SET lease_expires_at = 0 WHERE key_id = $1")
        .bind(fixture.key_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let response = post(fixture.state.clone(), &fixture.credential, "before-send").await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn cancelling_after_image_send_keeps_durable_no_replay_fence() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_json(json!({"created":1,"data":[{"b64_json":"bW9jay1wbmc="}]})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = fixture(&upstream).await;
    let state = fixture.state.clone();
    let credential = fixture.credential.clone();
    let task = tokio::spawn(async move { post(state, &credential, "cancel-after-send").await });
    tokio::time::timeout(Duration::from_secs(10), async {
        while upstream.received_requests().await.unwrap().is_empty() {
            assert!(
                !task.is_finished(),
                "request ended before mock observed the send"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE synchronous_image_idempotency SET lease_expires_at = 0 WHERE key_id = $1")
        .bind(fixture.key_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let status: String = sqlx::query_scalar("SELECT r.status FROM usage_reservations r JOIN request_records q ON q.reservation_id = r.id WHERE q.key_id = $1")
        .bind(fixture.key_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "reserved");
    pool.close().await;
    let repeated = post(
        fixture.state.clone(),
        &fixture.credential,
        "cancel-after-send",
    )
    .await;
    assert_eq!(repeated.status(), StatusCode::CONFLICT);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn durable_media_restore_pins_config_and_code_and_never_refreshes_deadline() {
    use crate::generation::group_routing::{prepare_route, snapshot};
    use crate::group_routing::durable::{deadline, restore_selected};
    use crate::plugin::routing::GroupRoutingOutcome;
    let upstream = MockServer::start().await;
    let mut fixture = fixture(&upstream).await;
    let key = fixture
        .state
        .db
        .authenticate_key(
            &fixture.credential,
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let route = prepare_route(
        &mut fixture.state,
        &key,
        "image-replay-model",
        None,
        request_id,
        request_id,
    )
    .await
    .unwrap();
    let persisted = snapshot(&fixture.state)
        .unwrap()
        .expect("group inclusion must create a durable strategy snapshot");
    assert_eq!(persisted["policies"].as_array().unwrap().len(), 1);
    assert_eq!(persisted["policies"][0]["config"], json!({}));
    let original_directive = persisted["policies"][0]["directive"].clone();

    // A later group update must not retroactively become this job's config.
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE provider_groups SET routing_strategy = $1, strategy_version = strategy_version + 1 WHERE tenant_id = $2")
        .bind(json!({"plugin_id":"image-routing","config":{"tag":"new"}}).to_string())
        .bind(fixture.tenant.to_string()).execute(&pool).await.unwrap();
    pool.close().await;

    // Change the actual component bytes and reload a separate runtime, keeping
    // the original runtime alive like a retained historical application pin.
    let root = fixture._directory.path().join("plugins");
    let mut replacement = original_directive.clone();
    replacement["cooldown_ms"] = json!(24680);
    fs::write(
        root.join("image-routing/plugin.wasm"),
        component(&json!({"candidates":[replacement.clone()]}), &replacement),
    )
    .unwrap();
    let mut current = fixture.state.clone();
    current.plugins = PluginRuntime::load(root.to_str(), current.db.clone()).unwrap();
    prepare_route(
        &mut current,
        &key,
        "image-replay-model",
        None,
        request_id,
        request_id,
    )
    .await
    .unwrap();
    let new_plan = snapshot(&current).unwrap().unwrap();
    assert_eq!(new_plan["policies"][0]["config"], json!({"tag":"new"}));
    assert_eq!(new_plan["policies"][0]["directive"]["cooldown_ms"], 24680);

    let restored = restore_selected(
        &fixture.state,
        Some(&persisted),
        fixture.tenant,
        Some(route.route_id),
        fixture.account,
        request_id,
    )
    .await
    .unwrap();
    let retained = snapshot(&restored).unwrap().unwrap();
    assert_eq!(retained["policies"][0]["config"], json!({}));
    assert_eq!(retained["policies"][0]["directive"], original_directive);
    let observed = crate::group_routing::observe(
        &restored,
        request_id,
        route.route_id,
        fixture.account,
        route.credential_generation,
        GroupRoutingOutcome::TransientFailure,
    )
    .await
    .unwrap();
    assert_eq!(
        observed.cooldown_ms, 12345,
        "real old Wasm observe must remain pinned"
    );

    let fallback = restore_selected(
        &current,
        Some(&persisted),
        fixture.tenant,
        Some(route.route_id),
        fixture.account,
        request_id,
    )
    .await
    .unwrap();
    assert!(
        snapshot(&fallback).unwrap().unwrap()["policies"]
            .as_array()
            .unwrap()
            .is_empty(),
        "different component fingerprint must not substitute replacement code"
    );
    assert!(
        crate::group_routing::observe(
            &fallback,
            request_id,
            route.route_id,
            fixture.account,
            route.credential_generation,
            GroupRoutingOutcome::TransientFailure
        )
        .await
        .is_none()
    );
    assert!(
        restore_selected(
            &fixture.state,
            Some(&persisted),
            Uuid::now_v7(),
            Some(route.route_id),
            fixture.account,
            request_id
        )
        .await
        .is_err()
    );

    // Use a definitely expired persisted wall-clock budget rather than sleeps
    // or a fresh Tokio deadline; every worker restore must keep it expired.
    let mut expired = persisted.clone();
    expired["deadline_at"] = json!(unix_millis() - 100);
    expired["started_at"] = json!(unix_millis() - 1100);
    for _ in 0..2 {
        let restored = restore_selected(
            &fixture.state,
            Some(&expired),
            fixture.tenant,
            Some(route.route_id),
            fixture.account,
            request_id,
        )
        .await
        .unwrap();
        assert!(deadline(&restored).unwrap() <= tokio::time::Instant::now());
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn explicit_image_auth_rejection_settles_zero_and_never_replays() {
    for status in [401, 403] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            // Malformed body does not erase the definite rejection in headers.
            .respond_with(
                ResponseTemplate::new(status).set_body_raw("invalid JSON", "application/json"),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        let fixture = fixture(&upstream).await;
        let response = post(
            fixture.state.clone(),
            &fixture.credential,
            "definite-auth-rejection",
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        let row = sqlx::query("SELECT r.status, r.actual_micros, q.cost_micros, q.submission_uncertain_at, i.status AS idempotency_status FROM usage_reservations r JOIN request_records q ON q.reservation_id = r.id JOIN synchronous_image_idempotency i ON i.request_id = q.id WHERE q.key_id = $1")
            .bind(fixture.key_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(row.get::<String, _>("status"), "settled");
        assert_eq!(row.get::<i64, _>("actual_micros"), 0);
        assert_eq!(row.get::<i64, _>("cost_micros"), 0);
        assert_eq!(row.get::<String, _>("idempotency_status"), "failed");
        assert!(
            row.get::<Option<i64>, _>("submission_uncertain_at")
                .is_none()
        );
        pool.close().await;
        let health = fixture
            .state
            .db
            .group_routing_health(fixture.tenant, fixture.account, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(health.last_failure_kind, "authentication");
        assert_eq!(health.consecutive_failures, 1);
        assert_eq!(health.probe_lease_until, 0);
        let replay = post(
            fixture.state.clone(),
            &fixture.credential,
            "definite-auth-rejection",
        )
        .await;
        assert_eq!(replay.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn truncated_successful_image_body_keeps_reservation_and_never_replays() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let upstream = MockServer::start().await;
    let fixture = fixture(&upstream).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 8192];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n{").await.unwrap();
        socket.shutdown().await.unwrap();
        listener
    });
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE upstream_accounts SET config_json = $1 WHERE id = $2")
        .bind(
            json!({"base_url":format!("http://{address}/v1"),"network_scope":"private"})
                .to_string(),
        )
        .bind(fixture.account.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let response = post(
        fixture.state.clone(),
        &fixture.credential,
        "truncated-image",
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let listener = server.await.unwrap();
    let status: String = sqlx::query_scalar("SELECT r.status FROM usage_reservations r JOIN request_records q ON q.reservation_id = r.id WHERE q.key_id = $1")
        .bind(fixture.key_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "reserved");
    pool.close().await;
    let replay = post(
        fixture.state.clone(),
        &fixture.credential,
        "truncated-image",
    )
    .await;
    assert_eq!(replay.status(), StatusCode::CONFLICT);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "same-key replay must not establish another upstream connection"
    );
    let body = to_bytes(replay.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "image_submission_uncertain");
}

#[tokio::test]
async fn filesystem_image_request_keeps_metadata_only_and_never_resubmits_replay() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer image-fixture-secret",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"created":1,"data":[{"b64_json":"bW9jay1wbmc="}]})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = fixture_with_archive(&upstream, crate::config::ArchiveBackend::Filesystem).await;
    let response = post(
        fixture.state.clone(),
        &fixture.credential,
        "filesystem-image",
    )
    .await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let row = sqlx::query("SELECT q.request_object, q.response_object, q.submission_started_at, r.status, r.actual_micros FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.key_id = $1")
        .bind(fixture.key_id.to_string()).fetch_one(&pool).await.unwrap();
    let locator = row.get::<String, _>("request_object");
    assert!(locator.starts_with("metadata-only-json:"));
    assert!(
        row.get::<String, _>("response_object")
            .starts_with("metadata-only-json:")
    );
    assert!(row.get::<Option<i64>, _>("submission_started_at").is_some());
    assert_eq!(row.get::<String, _>("status"), "settled");
    assert_eq!(row.get::<i64, _>("actual_micros"), 300_000);
    let replay = post(
        fixture.state.clone(),
        &fixture.credential,
        "filesystem-image",
    )
    .await;
    assert_eq!(replay.status(), StatusCode::CONFLICT);
    let replay_body = to_bytes(replay.into_body(), 64 * 1024).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&replay_body).unwrap()["error"]["code"],
        "image_result_not_retained"
    );
    let sent = upstream.received_requests().await.unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0]
            .headers
            .get_all(header::AUTHORIZATION)
            .iter()
            .count(),
        1,
        "credential revalidation must not append a duplicate authorization header"
    );
    pool.close().await;
}

#[tokio::test]
async fn rejected_image_arm_cas_never_sends_and_settles_zero_without_uncertainty() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let fixture = fixture(&upstream).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_image_arm BEFORE UPDATE OF submission_started_at ON request_records WHEN NEW.submission_started_at IS NOT NULL BEGIN SELECT RAISE(IGNORE); END")
        .execute(&pool).await.unwrap();
    let response = post(
        fixture.state.clone(),
        &fixture.credential,
        "arm-cas-rejected",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert_ne!(body["error"]["code"], "image_submission_uncertain");
    assert!(upstream.received_requests().await.unwrap().is_empty());
    let row = sqlx::query("SELECT r.status, r.actual_micros, q.submission_started_at, q.submission_uncertain_at FROM usage_reservations r JOIN request_records q ON q.reservation_id=r.id WHERE q.key_id=$1")
        .bind(fixture.key_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(row.get::<String, _>("status"), "settled");
    assert_eq!(row.get::<i64, _>("actual_micros"), 0);
    assert!(row.get::<Option<i64>, _>("submission_started_at").is_none());
    assert!(
        row.get::<Option<i64>, _>("submission_uncertain_at")
            .is_none()
    );
    pool.close().await;
}

#[tokio::test]
async fn pending_image_arm_uses_durable_truth_and_preserves_unknown_query_failures() {
    use super::super::synchronous_image::{
        ARM_CONFIRMED, ARM_NOT_STARTED, ARM_PENDING, SyncImageRequest, submission_may_have_started,
    };
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    for scenario in [
        "not_started",
        "committed_ack_lost",
        "query_failed",
        "query_failed_after_commit",
    ] {
        let upstream = MockServer::start().await;
        let fixture = fixture(&upstream).await;
        let key = fixture
            .state
            .db
            .authenticate_key(
                &fixture.credential,
                fixture.state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let price = fixture
            .state
            .db
            .generation_price("image-replay-model", &key.currency)
            .await
            .unwrap()
            .reservation_price()
            .unwrap();
        let request_id = Uuid::now_v7();
        let reservation = match fixture
            .state
            .db
            .start_synchronous_image_request(StartSynchronousImageRequest {
                routing_snapshot: None,
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 0,
                output_token_ceiling: 1,
                idempotency: None,
                protocol: "openai-image",
                model: "image-replay-model",
                request_object: "objects/blake3/test-arm-proof",
                upstream_account_id: Some(fixture.account),
                model_route_id: None,
            })
            .await
            .unwrap()
        {
            StartSynchronousImageResult::Started(value) => value,
            _ => panic!("fresh request"),
        };
        if matches!(scenario, "committed_ack_lost" | "query_failed_after_commit") {
            // Leave local state Pending, simulating loss of the successful
            // transaction acknowledgement after the durable marker committed.
            fixture
                .state
                .db
                .arm_synchronous_image_submission(fixture.key_id, None, request_id, reservation.id)
                .await
                .unwrap();
        }
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        if scenario.starts_with("query_failed") {
            sqlx::query("CREATE TRIGGER fail_arm_confirmation BEFORE UPDATE OF completed_at ON request_records BEGIN SELECT RAISE(FAIL, 'confirmation unavailable'); END")
                .execute(&pool).await.unwrap();
        }
        let context = SyncImageRequest {
            state: &fixture.state,
            reservation: &reservation,
            request_id,
            started: Instant::now(),
            billed_units: 1,
            expected_image_count: 1,
            key_id: fixture.key_id,
            idempotency_key: None,
            tenant_id: fixture.tenant,
            arm_state: AtomicU8::new(ARM_PENDING),
            invalid_response: AtomicBool::new(false),
            confirmed_rejection: AtomicBool::new(false),
        };
        assert_eq!(
            submission_may_have_started(&context).await.unwrap(),
            scenario != "not_started"
        );
        let expected_state = match scenario {
            "not_started" => ARM_NOT_STARTED,
            "committed_ack_lost" => ARM_CONFIRMED,
            _ => ARM_PENDING,
        };
        assert_eq!(context.arm_state.load(Ordering::Acquire), expected_state);
        let response = fail_image_request(&context, "image_submission_failed")
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if scenario == "not_started" {
                StatusCode::BAD_GATEWAY
            } else {
                StatusCode::CONFLICT
            }
        );
        let response: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap())
                .unwrap();
        if scenario.starts_with("query_failed") {
            assert_eq!(
                response["error"]["code"],
                "image_submission_state_unavailable"
            );
            assert_eq!(response["error"]["reconciliation_available"], false);
            assert_eq!(response["error"]["retryable"], false);
            assert!(
                fixture
                    .state
                    .db
                    .list_image_generation_quarantine("synchronous-strategy", 20, None)
                    .await
                    .unwrap()
                    .is_empty()
            );
        } else if scenario == "committed_ack_lost" {
            assert_eq!(response["error"]["code"], "image_submission_uncertain");
            assert_eq!(response["error"]["reconciliation_available"], true);
        }
        let status: String =
            sqlx::query_scalar("SELECT status FROM usage_reservations WHERE id=$1")
                .bind(reservation.id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            status,
            if scenario == "not_started" {
                "settled"
            } else {
                "reserved"
            }
        );
        if scenario.starts_with("query_failed") {
            // Restore storage, then run the real maintenance recovery. No new
            // submission, fake receipt or status-only rewrite is involved.
            sqlx::query("DROP TRIGGER fail_arm_confirmation")
                .execute(&pool)
                .await
                .unwrap();
            let stale = unix_millis() - 31 * 60 * 1000;
            let mut transaction = pool.begin().await.unwrap();
            sqlx::query("UPDATE request_records SET created_at=$1 WHERE id=$2")
                .bind(stale)
                .bind(request_id.to_string())
                .execute(&mut *transaction)
                .await
                .unwrap();
            // The locator's immutable timestamp is part of the settlement CAS.
            // Age both representations together when simulating old requests.
            sqlx::query("UPDATE request_record_locators SET created_at=$1 WHERE id=$2")
                .bind(stale)
                .bind(request_id.to_string())
                .execute(&mut *transaction)
                .await
                .unwrap();
            sqlx::query("UPDATE usage_reservations SET created_at=$1 WHERE id=$2")
                .bind(stale)
                .bind(reservation.id.to_string())
                .execute(&mut *transaction)
                .await
                .unwrap();
            transaction.commit().await.unwrap();
            let released = fixture
                .state
                .db
                .release_orphaned_reservations(20)
                .await
                .unwrap();
            assert_eq!(
                released,
                if scenario == "query_failed_after_commit" {
                    0
                } else {
                    1
                }
            );
            let row = sqlx::query("SELECT r.status, r.actual_micros, q.completed_at, q.submission_uncertain_at FROM usage_reservations r JOIN request_records q ON q.reservation_id=r.id WHERE q.id=$1")
                .bind(request_id.to_string()).fetch_one(&pool).await.unwrap();
            let listed = fixture
                .state
                .db
                .list_image_generation_quarantine("synchronous-strategy", 20, None)
                .await
                .unwrap();
            if scenario == "query_failed_after_commit" {
                assert_eq!(row.get::<String, _>("status"), "reserved");
                assert!(row.get::<Option<i64>, _>("completed_at").is_none());
                assert!(
                    row.get::<Option<i64>, _>("submission_uncertain_at")
                        .is_some()
                );
                assert_eq!(
                    listed.len(),
                    1,
                    "committed fence must become actionable after recovery"
                );
            } else {
                assert_eq!(row.get::<String, _>("status"), "settled");
                assert_eq!(row.get::<i64, _>("actual_micros"), 0);
                assert!(row.get::<Option<i64>, _>("completed_at").is_some());
                assert!(
                    row.get::<Option<i64>, _>("submission_uncertain_at")
                        .is_none()
                );
                assert!(
                    listed.is_empty(),
                    "proven unsent request is safely closed, not quarantined"
                );
            }
        }
        assert!(upstream.received_requests().await.unwrap().is_empty());
        pool.close().await;
    }
}

#[tokio::test]
async fn non_connect_image_send_timeout_keeps_health_unchanged_and_reservation_uncertain() {
    use super::super::synchronous_image::{
        ARM_NOT_STARTED, ImageResponseFormat, SyncImageRequest, execute_synchronous_image_request,
    };
    use std::sync::atomic::{AtomicBool, AtomicU8};
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .expect(1)
        .mount(&upstream)
        .await;
    let mut fixture = fixture(&upstream).await;
    let key = fixture
        .state
        .db
        .authenticate_key(
            &fixture.credential,
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let route = crate::generation::group_routing::prepare_route(
        &mut fixture.state,
        &key,
        "image-replay-model",
        None,
        request_id,
        request_id,
    )
    .await
    .unwrap();
    let snapshot = crate::generation::group_routing::snapshot(&fixture.state).unwrap();
    let price = fixture
        .state
        .db
        .generation_price("image-replay-model", &key.currency)
        .await
        .unwrap()
        .reservation_price()
        .unwrap();
    let placeholder = crate::api::generation::synchronous_image::image_metadata_locator(json!({
        "kind": "synchronous_image",
        "media_archived": false,
        "model": "image-replay-model",
        "expected_image_count": 1
    }))
    .unwrap();
    let reservation = match fixture
        .state
        .db
        .start_synchronous_image_request(StartSynchronousImageRequest {
            routing_snapshot: snapshot.as_ref(),
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 0,
            output_token_ceiling: 1,
            idempotency: None,
            protocol: "openai-image",
            model: "image-replay-model",
            request_object: &placeholder,
            upstream_account_id: Some(route.account_id),
            model_route_id: Some(route.route_id),
        })
        .await
        .unwrap()
    {
        StartSynchronousImageResult::Started(value) => value,
        _ => panic!("fresh timeout request"),
    };
    let context = SyncImageRequest {
        state: &fixture.state,
        reservation: &reservation,
        request_id,
        started: Instant::now(),
        billed_units: 1,
        expected_image_count: 1,
        key_id: key.key_id,
        idempotency_key: None,
        tenant_id: key.tenant_id,
        arm_state: AtomicU8::new(ARM_NOT_STARTED),
        invalid_response: AtomicBool::new(false),
        confirmed_rejection: AtomicBool::new(false),
    };
    // Per-request timeout is intentional: network policy may construct a fresh
    // no-retry client, so changing AppState.http does not control its deadline.
    // Keep the durable metadata-only arm, health admission and the single POST.
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let request = route
        .credential
        .apply(
            client
                .post(format!("{}/v1/images/generations", upstream.uri()))
                .json(&request_json())
                .timeout(Duration::from_millis(100)),
            unix_millis(),
        )
        .unwrap();
    let response = execute_synchronous_image_request(
        &context,
        Bytes::from(serde_json::to_vec(&request_json()).unwrap()),
        &placeholder,
        &route,
        request,
        ImageResponseFormat::OpenAi,
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    let health = fixture
        .state
        .db
        .group_routing_health(fixture.tenant, fixture.account, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(health.consecutive_failures, 0);
    assert_eq!(health.last_failure_kind, "");
    assert_eq!(health.probe_lease_until, 0);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let row=sqlx::query("SELECT r.status, q.submission_started_at, q.submission_uncertain_at, q.completed_at FROM usage_reservations r JOIN request_records q ON q.reservation_id=r.id WHERE q.key_id=$1")
        .bind(fixture.key_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(row.get::<String, _>("status"), "reserved");
    assert!(row.get::<Option<i64>, _>("submission_started_at").is_some());
    assert!(
        row.get::<Option<i64>, _>("submission_uncertain_at")
            .is_some()
    );
    assert!(row.get::<Option<i64>, _>("completed_at").is_none());
    pool.close().await;
}
