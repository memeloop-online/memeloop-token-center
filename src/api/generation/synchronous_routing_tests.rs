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
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("images.db").display()
    );
    let mut state = AppState::initialize(Config::for_test(database_url.clone()))
        .await
        .unwrap();
    let tenant_external = "synchronous-strategy";
    let account=state.db.create_upstream_account(CreateUpstreamAccountInput {
        tenant_external_id:tenant_external.into(), name:"image mock".into(), driver:"http-json".into(),
        config:json!({"base_url":format!("{}/v1",upstream.uri()),"network_scope":"private"}),
        credential:UpstreamCredential::None,oauth_session_id:None,oauth_driver:None,oauth_refresh_url:None,
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
        "contributions":{"group_routing":{"version":"group-routing-v1","schema":{"type":"object","additionalProperties":false},"default":{}}}})).unwrap()).unwrap();
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
    assert_eq!(response.status(), StatusCode::OK);
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
