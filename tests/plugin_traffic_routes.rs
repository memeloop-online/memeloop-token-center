use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{
        CreateKeyInput, CreateModelRouteInput, CreateRoutedModelRouteInput,
        CreateUpstreamAccountInput, ReplaceCredentialRoutingInput, RouteSelectionOptions,
        StatsFilter, unix_millis,
    },
    model::{IssuedKey, KeyPolicy},
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, header as matches_header, method, path},
};
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

struct LogWriter(LogCapture);

impl std::io::Write for LogWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.0.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogCapture {
    type Writer = LogWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        LogWriter(self.clone())
    }
}

impl LogCapture {
    fn contents(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

fn component_from_core_wat(source: &str) -> Vec<u8> {
    let mut module = wat::parse_str(source).expect("parse test core Wasm");
    let mut resolve = Resolve::default();
    let (package, _) = resolve
        .push_path("wit/token-center.wit")
        .expect("parse plugin WIT");
    let world = resolve
        .select_world(&[package], Some("plugin"))
        .expect("select plugin world");
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8)
        .expect("embed plugin metadata");
    ComponentEncoder::default()
        .module(&module)
        .expect("read test core module")
        .validate(true)
        .encode()
        .expect("encode test component")
}

fn policy_wat(body: &str) -> String {
    let source = include_str!("../examples/plugins/policy-rewrite/plugin.wat");
    let (prefix, tail) = source
        .split_once(";; BEGIN POST-AUTH BODY")
        .expect("post-auth start marker");
    let (_, suffix) = tail
        .split_once(";; END POST-AUTH BODY")
        .expect("post-auth end marker");
    format!("{prefix};; BEGIN POST-AUTH BODY\n{body}\n;; END POST-AUTH BODY{suffix}")
}

fn write_policy_package(root: &Path, body: &str, capabilities: Value) {
    fs::write(
        root.join("plugin.json"),
        serde_json::to_vec(&json!({
            "id": "gateway-policy",
            "version": "1.0.0",
            "wit_version": "0.2.0",
            "wasm": "plugin.wasm",
            "capabilities": capabilities,
            "contributions": {"traffic_policy": true, "providers": []}
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        root.join("plugin.wasm"),
        component_from_core_wat(&policy_wat(body)),
    )
    .unwrap();
}

async fn policy_state(
    body: &str,
    label: &str,
    allowed_models: Vec<String>,
) -> (tempfile::TempDir, AppState, IssuedKey) {
    policy_state_with_capabilities(body, label, allowed_models, json!([])).await
}

async fn policy_state_with_capabilities(
    body: &str,
    label: &str,
    allowed_models: Vec<String>,
    capabilities: Value,
) -> (tempfile::TempDir, AppState, IssuedKey) {
    let directory = tempfile::tempdir().unwrap();
    write_policy_package(directory.path(), body, capabilities);
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("policy.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some(directory.path().display().to_string());
    let state = AppState::initialize(config).await.unwrap();
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("policy-{label}"),
                principal_external_id: "policy-user".into(),
                alias: "policy-key".into(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    allowed_models,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    (directory, state, issued)
}

const DENY_BODY: &str = r#"
    i32.const 256 i32.const 0 i32.store
    i32.const 260 i32.const 0 i32.store
    i32.const 264 i32.const 0 i32.store
    i32.const 276 i32.const 0 i32.store
    i32.const 288 i32.const 0 i32.store
    i32.const 300 i32.const 0 i32.store
    i32.const 256
"#;

const REWRITE_MODEL_BODY: &str = r#"
    i32.const 256 i32.const 0 i32.store
    i32.const 260 i32.const 1 i32.store
    i32.const 264 i32.const 0 i32.store
    i32.const 276 i32.const 1 i32.store
    i32.const 280 i32.const 400 i32.store
    i32.const 284 i32.const 17 i32.store
    i32.const 288 i32.const 0 i32.store
    i32.const 300 i32.const 0 i32.store
    i32.const 256
"#;

fn deny_body_with_reason(reason: &str) -> String {
    let stores = stores_for_string(4096, reason);
    format!(
        r#"
        {stores}
        i32.const 256 i32.const 0 i32.store
        i32.const 260 i32.const 0 i32.store
        i32.const 264 i32.const 1 i32.store
        i32.const 268 i32.const 4096 i32.store
        i32.const 272 i32.const {reason_length} i32.store
        i32.const 276 i32.const 0 i32.store
        i32.const 288 i32.const 0 i32.store
        i32.const 300 i32.const 0 i32.store
        i32.const 256
        "#,
        reason_length = reason.len()
    )
}

fn stores_for_string(pointer: usize, value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .enumerate()
        .map(|(index, byte)| format!("i32.const {} i32.const {byte} i32.store8", pointer + index))
        .collect::<Vec<_>>()
        .join("\n")
}

fn allow_body_with_upstream_hint(account_id: uuid::Uuid) -> String {
    let account_id = account_id.to_string();
    let stores = stores_for_string(4096, &account_id);
    format!(
        r#"
        {stores}
        i32.const 256 i32.const 0 i32.store
        i32.const 260 i32.const 1 i32.store
        i32.const 264 i32.const 0 i32.store
        i32.const 276 i32.const 0 i32.store
        i32.const 288 i32.const 1 i32.store
        i32.const 292 i32.const 4096 i32.store
        i32.const 296 i32.const {account_id_length} i32.store
        i32.const 300 i32.const 0 i32.store
        i32.const 256
        "#,
        account_id_length = account_id.len()
    )
}

fn deny_body_with_log_message(message: &str) -> String {
    let level_stores = stores_for_string(4080, "warn");
    let message_stores = stores_for_string(4096, message);
    format!(
        r#"
        {level_stores}
        {message_stores}
        i32.const 4080 i32.const 4
        i32.const 4096 i32.const {message_length}
        call $log
        i32.const 256 i32.const 0 i32.store
        i32.const 260 i32.const 0 i32.store
        i32.const 264 i32.const 0 i32.store
        i32.const 276 i32.const 0 i32.store
        i32.const 288 i32.const 0 i32.store
        i32.const 300 i32.const 0 i32.store
        i32.const 256
        "#,
        message_length = message.len()
    )
}

struct HintRoutingFixture {
    _directory: tempfile::TempDir,
    state: AppState,
    database_url: String,
    issued: IssuedKey,
    model: String,
    preferred_account_id: uuid::Uuid,
    standby_account_id: uuid::Uuid,
    unauthorized_account_id: uuid::Uuid,
}

async fn hint_routing_fixture(
    label: &str,
    preferred_uri: String,
    standby_uri: String,
    hint_unauthorized_account: bool,
) -> HintRoutingFixture {
    hint_routing_fixture_with_database(
        label,
        preferred_uri,
        standby_uri,
        hint_unauthorized_account,
        None,
    )
    .await
}

async fn hint_routing_fixture_with_database(
    label: &str,
    preferred_uri: String,
    standby_uri: String,
    hint_unauthorized_account: bool,
    database_url: Option<String>,
) -> HintRoutingFixture {
    let directory = tempfile::tempdir().unwrap();
    let database_url = database_url.unwrap_or_else(|| {
        format!(
            "sqlite://{}?mode=rwc",
            directory
                .path()
                .join(format!("hint-routing-{label}.db"))
                .display()
        )
    });
    let initial_state = AppState::initialize(Config::for_test(database_url.clone()))
        .await
        .unwrap();
    let tenant = format!("hint-routing-{label}");
    let model = format!("hint-routing-model-{label}");
    let mut route_ids = Vec::new();
    let mut account_ids = Vec::new();
    for (name, uri, priority) in [
        ("preferred", preferred_uri, 10_i64),
        ("standby", standby_uri, 0_i64),
        (
            "unauthorized",
            "https://unauthorized.invalid".to_owned(),
            -10_i64,
        ),
    ] {
        let account = initial_state
            .db
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: tenant.clone(),
                    name: format!("{label}-{name}"),
                    driver: "http-json".to_owned(),
                    config: json!({
                        "base_url": uri,
                        "network_scope": "public",
                        "input_token_overhead_ceiling": if name == "standby" { 4096 } else { 0 }
                    }),
                    credential: UpstreamCredential::None,
                    oauth_session_id: None,
                    oauth_driver: None,
                    oauth_refresh_url: None,
                },
                initial_state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let route = initial_state
            .db
            .create_model_route(CreateModelRouteInput {
                tenant_external_id: tenant.clone(),
                public_model: model.clone(),
                upstream_account_id: account.id,
                upstream_model: format!("upstream-{name}"),
                protocol: "openai".to_owned(),
                priority,
            })
            .await
            .unwrap();
        account_ids.push(account.id);
        route_ids.push(route.id);
    }
    let issued = initial_state
        .db
        .create_key_with_routing(
            CreateKeyInput {
                tenant_external_id: tenant,
                principal_external_id: "hint-routing-user".to_owned(),
                alias: format!("hint-routing-{label}"),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    allowed_models: vec![model.clone()],
                    max_concurrency: 4,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            &route_ids[..2],
            &[],
            initial_state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    initial_state
        .db
        .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let preferred_account_id = account_ids[0];
    let standby_account_id = account_ids[1];
    let unauthorized_account_id = account_ids[2];
    let hint = if hint_unauthorized_account {
        unauthorized_account_id
    } else {
        preferred_account_id
    };
    write_policy_package(
        directory.path(),
        &allow_body_with_upstream_hint(hint),
        json!([]),
    );
    drop(initial_state);
    let mut runtime_config = Config::for_test(database_url.clone());
    runtime_config.plugin_dir = Some(directory.path().display().to_string());
    let state = AppState::initialize(runtime_config).await.unwrap();
    HintRoutingFixture {
        _directory: directory,
        state,
        database_url,
        issued,
        model,
        preferred_account_id,
        standby_account_id,
        unauthorized_account_id,
    }
}

async fn put_accounts_in_cooldown(fixture: &HintRoutingFixture, account_ids: &[uuid::Uuid]) {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let now = unix_millis();
    for account_id in account_ids {
        let credential_generation: i64 =
            sqlx::query_scalar("SELECT credential_generation FROM upstream_accounts WHERE id = $1")
                .bind(account_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO upstream_account_health (
                 upstream_account_id, consecutive_failures, cooldown_until,
                 probe_lease_until, probe_lease_token, credential_generation,
                 last_failure_kind, updated_at
             ) VALUES ($1, 1, $2, 0, '', $3, 'rate_limited', $4)",
        )
        .bind(account_id.to_string())
        .bind(now.saturating_add(60_000))
        .bind(credential_generation)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool.close().await;
}

async fn corrupt_upstream_credential(fixture: &HintRoutingFixture, account_id: uuid::Uuid) {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "UPDATE upstream_credentials SET credential_ciphertext = 'malformed-selected-candidate'
         WHERE upstream_account_id = $1",
    )
    .bind(account_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

async fn call_hint_routing_fixture(fixture: &HintRoutingFixture) -> (StatusCode, Value) {
    call(
        &fixture.state,
        &fixture.issued.key,
        "/v1/chat/completions",
        json!({
            "model": fixture.model,
            "messages": [{"role": "user", "content": "route safely"}]
        }),
    )
    .await
}

fn successful_hint_response(account: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": format!("chatcmpl-{account}"),
        "choices": [{"message": {"role": "assistant", "content": account}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
}

#[tokio::test]
async fn authorized_traffic_hint_is_preferred_ahead_of_route_priority() {
    let preferred = MockServer::start().await;
    let standby = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(successful_hint_response("preferred"))
        .expect(1)
        .mount(&preferred)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(successful_hint_response("standby"))
        .expect(0)
        .mount(&standby)
        .await;
    let fixture = hint_routing_fixture("healthy", preferred.uri(), standby.uri(), false).await;

    let (status, body) = call_hint_routing_fixture(&fixture).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["choices"][0]["message"]["content"], "preferred");
    preferred.verify().await;
    standby.verify().await;
}

async fn assert_malformed_standby_is_lazy(database_url: Option<String>, label: &str) {
    let preferred = MockServer::start().await;
    let standby = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(successful_hint_response("preferred"))
        .expect(1)
        .mount(&preferred)
        .await;
    Mock::given(method("POST"))
        .respond_with(successful_hint_response("unexpected-standby"))
        .expect(0)
        .mount(&standby)
        .await;
    let fixture = hint_routing_fixture_with_database(
        label,
        preferred.uri(),
        standby.uri(),
        false,
        database_url,
    )
    .await;
    corrupt_upstream_credential(&fixture, fixture.standby_account_id).await;

    let (status, body) = call_hint_routing_fixture(&fixture).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["choices"][0]["message"]["content"], "preferred");
    preferred.verify().await;
    standby.verify().await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let reservation_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1")
            .bind(fixture.issued.key_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    pool.close().await;
    assert_eq!(reservation_count, 1, "standby must not reserve capacity");
}

async fn assert_selected_malformed_candidate_fails_closed(
    database_url: Option<String>,
    label: &str,
) {
    let preferred = MockServer::start().await;
    let standby = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(successful_hint_response("unexpected-preferred"))
        .expect(0)
        .mount(&preferred)
        .await;
    Mock::given(method("POST"))
        .respond_with(successful_hint_response("unexpected-standby"))
        .expect(0)
        .mount(&standby)
        .await;
    let fixture = hint_routing_fixture_with_database(
        label,
        preferred.uri(),
        standby.uri(),
        false,
        database_url,
    )
    .await;
    put_accounts_in_cooldown(&fixture, &[fixture.preferred_account_id]).await;
    corrupt_upstream_credential(&fixture, fixture.standby_account_id).await;

    let (status, _) = call_hint_routing_fixture(&fixture).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    preferred.verify().await;
    standby.verify().await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let terminal: (i64, String, i64) = sqlx::query_as(
        "SELECT request.status_code, request.error_code,
                (SELECT COUNT(*) FROM usage_reservations reservation
                 WHERE reservation.key_id = request.key_id)
         FROM request_records request
         WHERE request.key_id = $1 ORDER BY request.created_at DESC LIMIT 1",
    )
    .bind(fixture.issued.key_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(terminal, (502, "upstream_candidate_invalid".to_owned(), 1));
}

#[tokio::test]
async fn sqlite_malformed_standby_does_not_poison_a_healthy_hinted_primary() {
    assert_malformed_standby_is_lazy(None, "sqlite-lazy-malformed-standby").await;
}

#[tokio::test]
async fn sqlite_selected_malformed_candidate_fails_closed() {
    assert_selected_malformed_candidate_fails_closed(None, "sqlite-selected-malformed").await;
}

#[tokio::test]
async fn postgres_malformed_standby_does_not_poison_a_healthy_hinted_primary() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let label = format!("postgres-lazy-malformed-{}", uuid::Uuid::now_v7());
    assert_malformed_standby_is_lazy(Some(database_url), &label).await;
}

#[tokio::test]
async fn postgres_selected_malformed_candidate_fails_closed() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let label = format!("postgres-selected-malformed-{}", uuid::Uuid::now_v7());
    assert_selected_malformed_candidate_fails_closed(Some(database_url), &label).await;
}

#[tokio::test]
async fn cooled_down_traffic_hint_fails_over_to_healthy_authorized_candidate() {
    assert_cooldown_failover_keeps_one_reservation(None, "cooldown").await;
}

async fn assert_cooldown_failover_keeps_one_reservation(database_url: Option<String>, label: &str) {
    let preferred = MockServer::start().await;
    let standby = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(successful_hint_response("unexpected-preferred"))
        .expect(0)
        .mount(&preferred)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(successful_hint_response("standby"))
        .expect(1)
        .mount(&standby)
        .await;
    let fixture = hint_routing_fixture_with_database(
        label,
        preferred.uri(),
        standby.uri(),
        false,
        database_url,
    )
    .await;
    put_accounts_in_cooldown(&fixture, &[fixture.preferred_account_id]).await;

    let (status, body) = call_hint_routing_fixture(&fixture).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["choices"][0]["message"]["content"], "standby");
    preferred.verify().await;
    standby.verify().await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let persisted: (String, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT request.upstream_account_id, reservation.reserved_tokens, rate_window.tokens,
                request.input_tokens, request.output_tokens,
                (SELECT COUNT(*) FROM usage_reservations reservation
                 WHERE reservation.key_id = request.key_id)
         FROM request_records request
         JOIN usage_reservations reservation ON reservation.id = request.reservation_id
         JOIN rate_limit_windows rate_window ON rate_window.key_id = reservation.key_id
              AND rate_window.window_start = reservation.rate_window_start
         WHERE request.key_id = $1 ORDER BY request.created_at DESC LIMIT 1",
    )
    .bind(fixture.issued.key_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(persisted.0, fixture.standby_account_id.to_string());
    assert!(
        persisted.1 > 8_000,
        "standby-specific overhead must replace the primary reservation bound"
    );
    assert_eq!(
        persisted.2,
        persisted.3 + persisted.4,
        "terminal settlement must release unused candidate capacity"
    );
    assert_eq!(persisted.5, 1);
}

#[tokio::test]
async fn postgres_cooldown_failover_resizes_the_same_reservation() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let label = format!("postgres-cooldown-resize-{}", uuid::Uuid::now_v7());
    assert_cooldown_failover_keeps_one_reservation(Some(database_url), &label).await;
}

#[tokio::test]
async fn rate_limited_traffic_hint_fails_over_to_healthy_authorized_candidate() {
    let preferred = MockServer::start().await;
    let standby = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&preferred)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(successful_hint_response("standby"))
        .expect(1)
        .mount(&standby)
        .await;
    let fixture = hint_routing_fixture("rate-limit", preferred.uri(), standby.uri(), false).await;

    let (status, body) = call_hint_routing_fixture(&fixture).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["choices"][0]["message"]["content"], "standby");
    preferred.verify().await;
    standby.verify().await;
}

#[tokio::test]
async fn unauthorized_traffic_hint_cannot_expand_the_granted_candidate_set() {
    let preferred = MockServer::start().await;
    let standby = MockServer::start().await;
    for upstream in [&preferred, &standby] {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(successful_hint_response("authorized"))
            .mount(upstream)
            .await;
    }
    let fixture = hint_routing_fixture("unauthorized", preferred.uri(), standby.uri(), true).await;

    let authenticated = fixture
        .state
        .db
        .authenticate_key(
            &fixture.issued.key,
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let candidates = fixture
        .state
        .db
        .resolve_authorized_upstream_candidates_with_hint(
            authenticated.key_id,
            authenticated.tenant_id,
            &fixture.model,
            "openai",
            RouteSelectionOptions {
                upstream_account_hint: Some(fixture.unauthorized_account_id),
                selection_seed: uuid::Uuid::now_v7(),
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(candidates.len(), 2);
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.account_id != fixture.unauthorized_account_id),
        "the resolver may only sort candidates produced by existing grants"
    );

    let (status, body) = call_hint_routing_fixture(&fixture).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["choices"][0]["message"]["content"], "authorized");
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let selected_account: String = sqlx::query_scalar(
        "SELECT upstream_account_id FROM request_records
         WHERE key_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(fixture.issued.key_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_ne!(
        selected_account,
        fixture.unauthorized_account_id.to_string(),
        "a plugin hint must not create a routing grant"
    );
}

#[tokio::test]
async fn all_authorized_candidates_in_cooldown_return_unavailable_without_upstream_calls() {
    let preferred = MockServer::start().await;
    let standby = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(successful_hint_response("unexpected-preferred"))
        .expect(0)
        .mount(&preferred)
        .await;
    Mock::given(method("POST"))
        .respond_with(successful_hint_response("unexpected-standby"))
        .expect(0)
        .mount(&standby)
        .await;
    let fixture = hint_routing_fixture("all-cooldown", preferred.uri(), standby.uri(), false).await;
    put_accounts_in_cooldown(
        &fixture,
        &[fixture.preferred_account_id, fixture.standby_account_id],
    )
    .await;

    let (status, _) = call_hint_routing_fixture(&fixture).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    preferred.verify().await;
    standby.verify().await;
}

#[tokio::test]
async fn deny_policy_covers_text_images_seedance_and_comfyui_before_any_upstream_or_charge() {
    let (_directory, state, issued) = policy_state(
        DENY_BODY,
        "deny",
        vec!["requested-model".into(), "example-rewritten".into()],
    )
    .await;
    create_account_and_routes(
        &state,
        "policy-deny",
        &issued,
        RouteFixture {
            driver: "http-json",
            config: json!({"base_url": "https://denied.example.test", "network_scope": "public"}),
            credential: UpstreamCredential::None,
            upstream_model: "denied-upstream",
            protocols: &["openai", "generation"],
            granted_public_models: &["requested-model", "example-rewritten"],
        },
    )
    .await;
    for (path, body) in [
        (
            "/v1/chat/completions",
            json!({"model": "requested-model", "messages": []}),
        ),
        (
            "/v1/images/generations",
            json!({"model": "requested-model", "prompt": "blocked image"}),
        ),
        (
            "/v1/videos/generations",
            json!({"model": "requested-model", "input": {"duration": 5}}),
        ),
        (
            "/v1/generations",
            json!({"model": "requested-model", "input": {"parameters": {}}}),
        ),
    ] {
        let response = call(&state, &issued.key, path, body).await;
        assert_eq!(response.0, StatusCode::FORBIDDEN, "{path}");
    }
    let key = state
        .db
        .authenticate_key(&issued.key, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert_eq!(
        state
            .db
            .stats_filtered(
                key.key_id,
                StatsFilter {
                    from_created_at: Some(unix_millis().saturating_sub(60_000)),
                    to_created_at: Some(unix_millis().saturating_add(1)),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .summary
            .total_requests,
        0
    );
}

#[tokio::test]
async fn malicious_denial_reason_is_absent_from_logs_and_http_response() {
    const CANARY: &str = "CANARY_TRAFFIC_REASON_CUSTOMER_SECRET";
    let reason = format!("{CANARY}{}", "\0\u{1b}\n".repeat(1_024));
    let body = deny_body_with_reason(&reason);
    let (_directory, state, issued) =
        policy_state(&body, "malicious-reason", vec!["requested-model".into()]).await;
    create_account_and_routes(
        &state,
        "policy-malicious-reason",
        &issued,
        RouteFixture {
            driver: "http-json",
            config: json!({"base_url": "https://denied.example.test", "network_scope": "public"}),
            credential: UpstreamCredential::None,
            upstream_model: "denied-upstream",
            protocols: &["openai"],
            granted_public_models: &["requested-model", "example-rewritten"],
        },
    )
    .await;
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();

    let response = call(
        &state,
        &issued.key,
        "/v1/chat/completions",
        json!({"model": "requested-model", "messages": []}),
    )
    .with_subscriber(subscriber)
    .await;

    assert_eq!(response.0, StatusCode::FORBIDDEN);
    let response_body = serde_json::to_string(&response.1).unwrap();
    let logs = capture.contents();
    assert!(!response_body.contains(CANARY), "{response_body}");
    assert!(!logs.contains(CANARY), "{logs}");
    assert!(logs.contains("gateway-policy"), "{logs}");
    assert!(logs.contains("policy_denied_invalid_metadata"), "{logs}");
    assert!(logs.len() < 4_096, "guest reason amplified log output");
}

#[tokio::test]
async fn log_capability_emits_only_bounded_host_owned_fields() {
    const CANARY: &str = "CANARY_PLUGIN_LOG_CUSTOMER_SECRET";
    let message = format!("{CANARY}{}", "\0\u{1b}\n".repeat(1_024));
    let body = deny_body_with_log_message(&message);
    let (_directory, state, issued) = policy_state_with_capabilities(
        &body,
        "malicious-log",
        vec!["requested-model".into()],
        json!([{"kind": "log"}]),
    )
    .await;
    create_account_and_routes(
        &state,
        "policy-malicious-log",
        &issued,
        RouteFixture {
            driver: "http-json",
            config: json!({"base_url": "https://denied.example.test", "network_scope": "public"}),
            credential: UpstreamCredential::None,
            upstream_model: "denied-upstream",
            protocols: &["openai"],
            granted_public_models: &["requested-model", "example-rewritten"],
        },
    )
    .await;
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);

    let decision = tracing::dispatcher::with_default(&dispatch, || {
        state.plugins.apply_traffic(
            memeloop_token_center::plugin::memeloop::token_center::types::RequestContext {
                tenant_id: "tenant".into(),
                principal_id: "principal".into(),
                key_id: "key".into(),
                protocol: "openai".into(),
                model: "requested-model".into(),
                config_json: "{}".into(),
            },
            &json!({"model": "requested-model", "messages": []}),
        )
    })
    .expect("execute policy with Log capability");
    assert!(!decision.allow);

    let response = call(
        &state,
        &issued.key,
        "/v1/chat/completions",
        json!({"model": "requested-model", "messages": []}),
    )
    .await;

    assert_eq!(response.0, StatusCode::FORBIDDEN);
    let response_body = serde_json::to_string(&response.1).unwrap();
    let logs = capture.contents();
    assert!(!response_body.contains(CANARY), "{response_body}");
    assert!(!logs.contains(CANARY), "{logs}");
    assert!(logs.contains("gateway-policy"), "{logs}");
    assert!(logs.contains("plugin_log_emitted"), "{logs}");
    assert!(logs.len() < 4_096, "guest message amplified log output");
}

#[tokio::test]
async fn image_rewrite_rechecks_effective_permission_route_price_and_archives_charge() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(matches_header("authorization", "Bearer image-secret"))
        .and(body_partial_json(json!({
            "model": "image-upstream",
            "prompt": "rewritten model image"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 1,
            "data": [{"b64_json": "aW1hZ2U="}]
        })))
        .expect(1)
        .mount(&mock)
        .await;
    let (_directory, state, issued) = policy_state(
        REWRITE_MODEL_BODY,
        "image-rewrite",
        vec!["requested-model".into(), "example-rewritten".into()],
    )
    .await;
    let account = create_account_and_routes(
        &state,
        "policy-image-rewrite",
        &issued,
        RouteFixture {
            driver: "http-json",
            config: json!({"base_url": mock.uri(), "network_scope": "public"}),
            credential: UpstreamCredential::ApiKey {
                value: "image-secret".into(),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
            },
            upstream_model: "image-upstream",
            protocols: &["generation"],
            granted_public_models: &["requested-model", "example-rewritten"],
        },
    )
    .await;
    state
        .db
        .upsert_generation_price("example-rewritten", "USD", "image", Decimal::new(5, 2))
        .await
        .unwrap();
    let response = call(
        &state,
        &issued.key,
        "/v1/images/generations",
        json!({"model": "requested-model", "prompt": "rewritten model image"}),
    )
    .await;
    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(response.1["data"][0]["b64_json"], "aW1hZ2U=");

    let key = state
        .db
        .authenticate_key(&issued.key, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    for _ in 0..50 {
        let stats = state
            .db
            .stats_filtered(
                key.key_id,
                memeloop_token_center::db::StatsFilter {
                    from_created_at: Some(unix_millis().saturating_sub(60_000)),
                    to_created_at: Some(unix_millis().saturating_add(1)),
                    upstream_account_id: Some(account.id),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        if stats.summary.total_requests == 1 {
            assert_eq!(stats.summary.total_cost.as_deref(), Some("0.05"));
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("rewritten image request was not finalized");
}

#[tokio::test]
async fn async_rewrite_rechecks_seedance_and_comfyui_routes_and_billing_units() {
    for (label, driver, billing_unit, config, path, input) in [
        (
            "seedance-rewrite",
            "volcengine-seedance",
            "second",
            json!({"base_url": "https://seedance.example.test"}),
            "/v1/videos/generations",
            json!({"duration": 5, "content": [{"type": "text", "text": "video"}]}),
        ),
        (
            "comfy-rewrite",
            "comfyui",
            "job",
            json!({
                "base_url": "https://comfy.example.test",
                "workflow_id": "fixture-v1",
                "workflow_template": {"1": {"inputs": {"text": {"$mtc_param": "prompt"}}}}
            }),
            "/v1/generations",
            json!({"parameters": {"prompt": "image"}}),
        ),
    ] {
        let (_directory, state, issued) = policy_state(
            REWRITE_MODEL_BODY,
            label,
            vec!["requested-model".into(), "example-rewritten".into()],
        )
        .await;
        create_account_and_routes(
            &state,
            &format!("policy-{label}"),
            &issued,
            RouteFixture {
                driver,
                config,
                credential: UpstreamCredential::None,
                upstream_model: "provider-model",
                protocols: &["generation"],
                granted_public_models: &["requested-model", "example-rewritten"],
            },
        )
        .await;
        state
            .db
            .upsert_generation_price("example-rewritten", "USD", billing_unit, Decimal::new(1, 2))
            .await
            .unwrap();
        let response = call(
            &state,
            &issued.key,
            path,
            json!({"model": "requested-model", "input": input}),
        )
        .await;
        assert_eq!(response.0, StatusCode::ACCEPTED, "{driver}");
        assert_eq!(response.1["model"], "example-rewritten");
        assert_eq!(response.1["driver"], driver);
    }
}

#[tokio::test]
async fn rewritten_model_is_checked_again_against_the_stable_key_policy() {
    let (_directory, state, issued) = policy_state(
        REWRITE_MODEL_BODY,
        "permission",
        vec!["requested-model".into()],
    )
    .await;
    create_account_and_routes(
        &state,
        "policy-permission",
        &issued,
        RouteFixture {
            driver: "http-json",
            config: json!({"base_url": "https://denied.example.test", "network_scope": "public"}),
            credential: UpstreamCredential::None,
            upstream_model: "denied-upstream",
            protocols: &["generation"],
            granted_public_models: &["requested-model"],
        },
    )
    .await;
    let response = call(
        &state,
        &issued.key,
        "/v1/videos/generations",
        json!({"model": "requested-model", "input": {"duration": 5}}),
    )
    .await;
    assert_eq!(response.0, StatusCode::FORBIDDEN);
}

struct RouteFixture<'a> {
    driver: &'a str,
    config: Value,
    credential: UpstreamCredential,
    upstream_model: &'a str,
    protocols: &'a [&'a str],
    granted_public_models: &'a [&'a str],
}

async fn create_account_and_routes(
    state: &AppState,
    tenant: &str,
    issued: &IssuedKey,
    fixture: RouteFixture<'_>,
) -> memeloop_token_center::provider::UpstreamAccountView {
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.into(),
                name: format!("{}-account", fixture.driver),
                driver: fixture.driver.into(),
                config: fixture.config,
                credential: fixture.credential,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let mut new_route_ids = Vec::new();
    for protocol in fixture.protocols {
        for public_model in ["requested-model", "example-rewritten"] {
            let (route, _) = state
                .db
                .create_routed_model_route(CreateRoutedModelRouteInput {
                    tenant_external_id: tenant.into(),
                    public_model: public_model.into(),
                    upstream_model: fixture.upstream_model.into(),
                    protocol: (*protocol).into(),
                    priority: 0,
                    enabled: true,
                    upstream_account_ids: vec![account.id],
                    included_provider_group_ids: Vec::new(),
                    excluded_provider_group_ids: Vec::new(),
                    route_group_ids: Vec::new(),
                    route_group_names: Vec::new(),
                    granted_credential_ids: Vec::new(),
                    custom_model_confirmed: true,
                })
                .await
                .unwrap();
            if fixture.granted_public_models.contains(&public_model) {
                new_route_ids.push(route.id);
            }
        }
    }
    let current = state
        .db
        .credential_routing(issued.key_id, tenant)
        .await
        .unwrap();
    let mut route_ids = current.route_ids;
    route_ids.extend(new_route_ids);
    route_ids.sort_unstable();
    route_ids.dedup();
    state
        .db
        .replace_credential_routing(
            issued.key_id,
            ReplaceCredentialRoutingInput {
                tenant_external_id: tenant.into(),
                route_ids,
                route_group_ids: current.route_group_ids,
                expected_grant_revision: current.grant_revision,
            },
        )
        .await
        .unwrap();
    account
}

async fn call(state: &AppState, key: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = api::router_for_role(state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post(path)
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, body)
}
