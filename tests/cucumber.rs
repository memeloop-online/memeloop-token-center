use std::{fmt, str::FromStr};

use cucumber::{World, given, then, when};
use memeloop_token_center::{AppState, api, config::Config};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[derive(World)]
struct TokenCenterWorld {
    client: Client,
    service_url: String,
    mock: Option<MockServer>,
    temp_dir: Option<TempDir>,
    server_task: Option<JoinHandle<()>>,
    current_key: String,
    old_key: String,
    stable_key_id: Option<Uuid>,
    status: Option<StatusCode>,
    response: Value,
}

impl Default for TokenCenterWorld {
    fn default() -> Self {
        Self {
            client: Client::new(),
            service_url: String::new(),
            mock: None,
            temp_dir: None,
            server_task: None,
            current_key: String::new(),
            old_key: String::new(),
            stable_key_id: None,
            status: None,
            response: Value::Null,
        }
    }
}

impl fmt::Debug for TokenCenterWorld {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenCenterWorld")
            .field("service_url", &self.service_url)
            .field("stable_key_id", &self.stable_key_id)
            .field("status", &self.status)
            .field("response", &self.response)
            .finish()
    }
}

impl Drop for TokenCenterWorld {
    fn drop(&mut self) {
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
    }
}

#[given("a token center backed by SQLite and memory object storage")]
async fn start_test_service(world: &mut TokenCenterWorld) {
    let temp_dir = tempfile::tempdir().expect("temporary directory");
    let database_path = temp_dir.path().join("token-center.db");
    let database_url = format!("sqlite://{}?mode=rwc", database_path.display());
    let mock = MockServer::start().await;
    let mut config = Config::for_test(database_url);
    config.upstream_openai_url = Some(mock.uri());
    config.upstream_anthropic_url = Some(mock.uri());
    let state = AppState::initialize(config)
        .await
        .expect("initialize test service");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let address = listener.local_addr().expect("test server address");
    let server_task = tokio::spawn(async move {
        axum::serve(listener, api::router(state))
            .await
            .expect("serve test application");
    });

    world.service_url = format!("http://{address}");
    world.mock = Some(mock);
    world.temp_dir = Some(temp_dir);
    world.server_task = Some(server_task);
}

#[given("the mock OpenAI upstream returns a successful completion")]
async fn mock_successful_openai(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-test",
            "choices": [{"message": {"role": "assistant", "content": "hello"}}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
        })))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[given("the mock Anthropic upstream returns a successful message")]
async fn mock_successful_anthropic(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg-test",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hello from Claude"}],
            "usage": {"input_tokens": 8, "output_tokens": 4}
        })))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[when(expr = "the service creates a key for principal {string} allowing model {string}")]
async fn create_key(world: &mut TokenCenterWorld, principal: String, model: String) {
    let price_response = world
        .client
        .post(format!(
            "{}/internal/v1/prices/USD/{model}",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({
            "input_per_million": "1.00",
            "output_per_million": "1.00"
        }))
        .send()
        .await
        .expect("create model price");
    assert_eq!(price_response.status(), StatusCode::OK);
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "principal_external_id": principal,
            "alias": "primary",
            "currency": "USD",
            "initial_balance": "10.00",
            "policy": {"allowed_models": [model]}
        }))
        .send()
        .await
        .expect("create key request");
    world.status = Some(response.status());
    world.response = response.json().await.expect("create key JSON");
    world.current_key = world.response["key"]
        .as_str()
        .expect("issued key")
        .to_owned();
    world.stable_key_id = Some(
        Uuid::from_str(world.response["key_id"].as_str().expect("key id")).expect("UUID key id"),
    );
}

#[when(expr = "the service creates an exhausted key allowing model {string}")]
async fn create_exhausted_key(world: &mut TokenCenterWorld, model: String) {
    let price_response = world
        .client
        .post(format!(
            "{}/internal/v1/prices/USD/{model}",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({
            "input_per_million": "1.00",
            "output_per_million": "1.00"
        }))
        .send()
        .await
        .expect("create model price");
    assert_eq!(price_response.status(), StatusCode::OK);
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "principal_external_id": "exhausted",
            "alias": "empty",
            "currency": "USD",
            "initial_balance": "0",
            "policy": {"allowed_models": [model]}
        }))
        .send()
        .await
        .expect("create exhausted key");
    world.response = response.json().await.expect("create key JSON");
    world.current_key = world.response["key"]
        .as_str()
        .expect("issued key")
        .to_owned();
}

#[when(expr = "the service creates a key with RPM 1 allowing model {string}")]
async fn create_rate_limited_key(world: &mut TokenCenterWorld, model: String) {
    let price_response = world
        .client
        .post(format!(
            "{}/internal/v1/prices/USD/{model}",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"input_per_million": "1", "output_per_million": "1"}))
        .send()
        .await
        .expect("create model price");
    assert_eq!(price_response.status(), StatusCode::OK);
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "principal_external_id": "rate-limited",
            "alias": "rpm-one",
            "currency": "USD",
            "initial_balance": "10",
            "policy": {
                "allowed_models": [model],
                "requests_per_minute": 1,
                "tokens_per_minute": 100000,
                "max_concurrency": 4
            }
        }))
        .send()
        .await
        .expect("create rate-limited key");
    world.response = response.json().await.expect("create key JSON");
    world.current_key = world.response["key"]
        .as_str()
        .expect("issued key")
        .to_owned();
}

#[when(expr = "the client calls model {string}")]
async fn call_model(world: &mut TokenCenterWorld, model: String) {
    let response = world
        .client
        .post(format!("{}/v1/chat/completions", world.service_url))
        .bearer_auth(&world.current_key)
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("model request");
    world.status = Some(response.status());
    world.response = response.json().await.unwrap_or(Value::Null);
}

#[when(expr = "the client calls model {string} twice")]
async fn call_model_twice(world: &mut TokenCenterWorld, model: String) {
    call_model(world, model.clone()).await;
    assert_eq!(world.status, Some(StatusCode::OK));
    call_model(world, model).await;
}

#[when(expr = "the Claude client calls model {string}")]
async fn call_anthropic_model(world: &mut TokenCenterWorld, model: String) {
    let response = world
        .client
        .post(format!("{}/v1/messages", world.service_url))
        .bearer_auth(&world.current_key)
        .header("anthropic-version", "2023-06-01")
        .header("x-claude-code-session-id", "claude-code-session")
        .json(&json!({
            "model": model,
            "max_tokens": 128,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("Anthropic model request");
    world.status = Some(response.status());
    world.response = response.json().await.unwrap_or(Value::Null);
}

#[when(expr = "the client sends two full-context requests for model {string} in one session")]
async fn send_full_context_session(world: &mut TokenCenterWorld, model: String) {
    let first = world
        .client
        .post(format!("{}/v1/chat/completions", world.service_url))
        .bearer_auth(&world.current_key)
        .header("x-claude-code-session-id", "claude-session-a")
        .json(&json!({
            "model": model,
            "messages": [{"role": "user", "content": "one"}]
        }))
        .send()
        .await
        .expect("first full-context request");
    assert_eq!(first.status(), StatusCode::OK);
    let second = world
        .client
        .post(format!("{}/v1/chat/completions", world.service_url))
        .bearer_auth(&world.current_key)
        .header("x-claude-code-session-id", "claude-session-a")
        .json(&json!({
            "model": model,
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "assistant", "content": "two"},
                {"role": "user", "content": "three"}
            ]
        }))
        .send()
        .await
        .expect("second full-context request");
    world.status = Some(second.status());
}

#[then(expr = "the response status is {int}")]
async fn response_status(world: &mut TokenCenterWorld, expected: u16) {
    assert_eq!(
        world.status,
        Some(StatusCode::from_u16(expected).expect("valid HTTP status"))
    );
}

#[when("the service rotates the key")]
async fn rotate_key(world: &mut TokenCenterWorld) {
    world.old_key = world.current_key.clone();
    let key_id = world.stable_key_id.expect("stable key id");
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/keys/{key_id}/rotate",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("rotate key request");
    world.status = Some(response.status());
    world.response = response.json().await.expect("rotate key JSON");
    world.current_key = world.response["key"]
        .as_str()
        .expect("rotated key")
        .to_owned();
}

#[then("the rotated credential retains the stable key id")]
async fn rotated_key_retains_id(world: &mut TokenCenterWorld) {
    assert_eq!(
        world.response["key_id"],
        world.stable_key_id.expect("stable id").to_string()
    );
    assert_eq!(world.response["credential_generation"], 2);
}

#[then("the old credential is rejected")]
async fn old_key_rejected(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .get(format!("{}/self/v1/stats", world.service_url))
        .bearer_auth(&world.old_key)
        .send()
        .await
        .expect("old key check");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[when("the client views its statistics with the rotated credential")]
async fn view_stats(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .get(format!("{}/self/v1/stats", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("self statistics request");
    world.status = Some(response.status());
    world.response = response.json().await.expect("statistics JSON");
}

#[then(expr = "the statistics contain {int} request and {int} tokens")]
async fn stats_contain_request(world: &mut TokenCenterWorld, requests: i64, tokens: i64) {
    assert_eq!(
        world.response["key_id"],
        world.stable_key_id.expect("key id").to_string()
    );
    assert_eq!(world.response["summary"]["total_requests"], requests);
    assert_eq!(
        world.response["summary"]["input_tokens"]
            .as_i64()
            .unwrap_or_default()
            + world.response["summary"]["output_tokens"]
                .as_i64()
                .unwrap_or_default(),
        tokens
    );
}

#[then("the request detail contains the archived prompt and response")]
async fn request_detail_contains_archive(world: &mut TokenCenterWorld) {
    let requests = world
        .client
        .get(format!("{}/self/v1/requests", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("request list")
        .json::<Value>()
        .await
        .expect("request list JSON");
    let request_id = requests[0]["request_id"].as_str().expect("request id");
    let response = world
        .client
        .get(format!(
            "{}/self/v1/requests/{request_id}",
            world.service_url
        ))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("request detail");
    assert_eq!(response.status(), StatusCode::OK);
    let detail = response.json::<Value>().await.expect("request detail JSON");
    assert_eq!(detail["archive_complete"], true);
    assert_eq!(detail["request_body"]["messages"][0]["content"], "hi");
    assert_eq!(
        detail["response_body"]["choices"][0]["message"]["content"],
        "hello"
    );
}

#[then("the requests form one logical conversation with a continuation edge")]
async fn logical_conversation_continues(world: &mut TokenCenterWorld) {
    let clusters = world
        .client
        .get(format!("{}/self/v1/conversations", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("conversation clusters")
        .json::<Value>()
        .await
        .expect("conversation clusters JSON");
    assert_eq!(clusters.as_array().map(Vec::len), Some(1));
    assert_eq!(clusters[0]["request_count"], 2);
    let cluster_id = clusters[0]["cluster_id"].as_str().expect("cluster id");
    let detail = world
        .client
        .get(format!(
            "{}/self/v1/conversations/{cluster_id}",
            world.service_url
        ))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("conversation detail")
        .json::<Value>()
        .await
        .expect("conversation detail JSON");
    assert_eq!(detail["edges"][0]["relation"], "continues");
    assert!(
        detail["edges"][0]["confidence"]
            .as_f64()
            .unwrap_or_default()
            >= 0.95
    );
}

#[then("the downstream key cannot create another key")]
async fn downstream_cannot_administer(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth(&world.current_key)
        .json(&json!({
            "principal_external_id": "intruder",
            "alias": "forbidden",
            "currency": "USD"
        }))
        .send()
        .await
        .expect("forbidden management request");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::main]
async fn main() {
    TokenCenterWorld::run("tests/features").await;
}
