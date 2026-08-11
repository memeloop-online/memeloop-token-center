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
    matchers::{body_partial_json, header, method, path},
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
    oauth_upstream_id: Option<Uuid>,
    oauth_generation: i64,
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
            oauth_upstream_id: None,
            oauth_generation: 0,
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

#[given("the mock routed upstream accepts API key and OAuth credentials")]
async fn mock_routed_upstream(world: &mut TokenCenterWorld) {
    for (authorization, model) in [
        ("ApiKey direct-secret", "api-upstream"),
        ("Bearer oauth-access-1", "oauth-upstream"),
    ] {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", authorization))
            .and(body_partial_json(json!({"model": model})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": format!("chatcmpl-{model}"),
                "choices": [{"message": {"role": "assistant", "content": "routed"}}],
                "usage": {"prompt_tokens": 4, "completion_tokens": 2}
            })))
            .mount(world.mock.as_ref().expect("mock server"))
            .await;
    }
}

#[when("the service creates API and OAuth routes")]
async fn create_upstream_routes(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let api_account = create_upstream_account(
        world,
        "direct-api",
        json!({
            "type": "api_key",
            "value": "direct-secret",
            "header": "authorization",
            "prefix": "ApiKey "
        }),
        &mock_url,
    )
    .await;
    assert!(api_account.get("credential").is_none());
    let oauth_account = create_upstream_account(
        world,
        "oauth-account",
        json!({
            "type": "oauth",
            "access_token": "oauth-access-1",
            "refresh_token": "oauth-refresh-1",
            "expires_at": 4102444800000_i64
        }),
        &mock_url,
    )
    .await;
    assert!(oauth_account.get("credential").is_none());
    let api_id = api_account["id"].as_str().expect("API account id");
    let oauth_id = oauth_account["id"].as_str().expect("OAuth account id");
    world.oauth_upstream_id = Some(Uuid::from_str(oauth_id).expect("OAuth UUID"));
    world.oauth_generation = oauth_account["credential_generation"]
        .as_i64()
        .expect("credential generation");
    create_route(world, "api-public", api_id, "api-upstream").await;
    create_route(world, "oauth-public", oauth_id, "oauth-upstream").await;
}

async fn create_upstream_account(
    world: &TokenCenterWorld,
    name: &str,
    credential: Value,
    base_url: &str,
) -> Value {
    let response = world
        .client
        .post(format!("{}/internal/v1/upstreams", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": name,
            "driver": "http-json",
            "config": {"base_url": base_url},
            "credential": credential
        }))
        .send()
        .await
        .expect("create upstream account");
    let status = response.status();
    let body = response.text().await.expect("upstream account response");
    assert_eq!(status, StatusCode::CREATED, "{body}");
    serde_json::from_str(&body).expect("upstream account JSON")
}

async fn create_route(
    world: &TokenCenterWorld,
    public_model: &str,
    upstream_account_id: &str,
    upstream_model: &str,
) {
    let response = world
        .client
        .post(format!("{}/internal/v1/model-routes", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "public_model": public_model,
            "upstream_account_id": upstream_account_id,
            "upstream_model": upstream_model,
            "protocol": "openai"
        }))
        .send()
        .await
        .expect("create model route");
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[when("the service creates a key allowing both routed models")]
async fn create_key_for_routed_models(world: &mut TokenCenterWorld) {
    for model in ["api-public", "oauth-public"] {
        let response = world
            .client
            .post(format!(
                "{}/internal/v1/prices/USD/{model}",
                world.service_url
            ))
            .bearer_auth("test-service-token")
            .json(&json!({"input_per_million": "1", "output_per_million": "1"}))
            .send()
            .await
            .expect("create routed model price");
        assert_eq!(response.status(), StatusCode::OK);
    }
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "principal_external_id": "routed-user",
            "alias": "routed",
            "currency": "USD",
            "initial_balance": "10",
            "policy": {"allowed_models": ["api-public", "oauth-public"]}
        }))
        .send()
        .await
        .expect("create routed key");
    assert_eq!(response.status(), StatusCode::CREATED);
    let value: Value = response.json().await.expect("routed key JSON");
    world.current_key = value["key"].as_str().expect("issued key").to_owned();
}

#[when("the client calls both routed models")]
async fn call_both_routed_models(world: &mut TokenCenterWorld) {
    call_model(world, "api-public".to_owned()).await;
    assert_eq!(world.status, Some(StatusCode::OK));
    call_model(world, "oauth-public".to_owned()).await;
}

#[then("both upstream authentication types used the same routing pipeline")]
async fn both_auth_types_were_routed(world: &mut TokenCenterWorld) {
    let requests = world
        .mock
        .as_ref()
        .expect("mock server")
        .received_requests()
        .await
        .expect("received requests");
    let authorizations = requests
        .iter()
        .filter_map(|request| request.headers.get("authorization"))
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>();
    assert!(authorizations.contains(&"ApiKey direct-secret"));
    assert!(authorizations.contains(&"Bearer oauth-access-1"));
}

#[when("the service rotates the OAuth upstream credential")]
async fn rotate_oauth_upstream(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer oauth-access-2"))
        .and(body_partial_json(json!({"model": "oauth-upstream"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-oauth-rotated",
            "choices": [{"message": {"role": "assistant", "content": "rotated"}}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2}
        })))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    let account_id = world.oauth_upstream_id.expect("OAuth upstream id");
    let response = world
        .client
        .put(format!(
            "{}/internal/v1/upstreams/{account_id}/credential",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({
            "credential": {
                "type": "oauth",
                "access_token": "oauth-access-2",
                "refresh_token": "oauth-refresh-2",
                "expires_at": 4102444800000_i64
            }
        }))
        .send()
        .await
        .expect("rotate OAuth upstream credential");
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response.json().await.expect("rotated upstream JSON");
    assert_eq!(value["id"], account_id.to_string());
    world.oauth_generation = value["credential_generation"]
        .as_i64()
        .expect("rotated generation");
}

#[then("the OAuth upstream account retains its stable id and uses generation 2")]
async fn oauth_upstream_retains_id(world: &mut TokenCenterWorld) {
    assert!(world.oauth_upstream_id.is_some());
    assert_eq!(world.oauth_generation, 2);
    let requests = world
        .mock
        .as_ref()
        .expect("mock server")
        .received_requests()
        .await
        .expect("received requests");
    assert!(requests.iter().any(|request| {
        request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some("Bearer oauth-access-2")
    }));
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
