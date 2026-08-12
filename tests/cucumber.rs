use std::{fmt, str::FromStr};

use cucumber::{World, given, then, when};
use futures_util::StreamExt;
use memeloop_token_center::{AppState, api, config::Config, worker};
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
    worker_task: Option<JoinHandle<()>>,
    current_key: String,
    old_key: String,
    stable_key_id: Option<Uuid>,
    stable_account_id: Option<Uuid>,
    oauth_upstream_id: Option<Uuid>,
    oauth_generation: i64,
    cursor_session_token: String,
    cursor_login_url: String,
    cursor_account_id: Option<Uuid>,
    cursor_generation: i64,
    current_service_token: String,
    old_service_token: String,
    stable_service_id: Option<Uuid>,
    generation_job_id: Option<Uuid>,
    subscription_session_token: String,
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
            worker_task: None,
            current_key: String::new(),
            old_key: String::new(),
            stable_key_id: None,
            stable_account_id: None,
            oauth_upstream_id: None,
            oauth_generation: 0,
            cursor_session_token: String::new(),
            cursor_login_url: String::new(),
            cursor_account_id: None,
            cursor_generation: 0,
            current_service_token: String::new(),
            old_service_token: String::new(),
            stable_service_id: None,
            generation_job_id: None,
            subscription_session_token: String::new(),
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
        if let Some(task) = self.worker_task.take() {
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
    let worker_task = tokio::spawn(worker::run(state.clone()));
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
    world.worker_task = Some(worker_task);
}

#[given("the mock Seedance upstream completes a five second video")]
async fn mock_seedance_generation(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/api/v3/contents/generations/tasks"))
        .and(header("authorization", "Bearer seedance-secret"))
        .and(body_partial_json(json!({"model": "seedance-upstream"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "cgt-test"})))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    Mock::given(method("GET"))
        .and(path("/api/v3/contents/generations/tasks/cgt-test"))
        .and(header("authorization", "Bearer seedance-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "cgt-test",
            "status": "succeeded",
            "duration": "5",
            "content": {"video_url": format!("{mock_url}/assets/video.mp4")},
            "usage": {"completion_tokens": 1234, "total_tokens": 1234}
        })))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    Mock::given(method("GET"))
        .and(path("/assets/video.mp4"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "video/mp4")
                .set_body_bytes(b"mock-video-content"),
        )
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[when("the service creates a metered Seedance route and key")]
async fn create_seedance_route_and_key(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let response = world
        .client
        .post(format!("{}/internal/v1/upstreams", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": "seedance",
            "driver": "volcengine-seedance",
            "config": {"base_url": mock_url},
            "credential": {"type": "api_key", "value": "seedance-secret"}
        }))
        .send()
        .await
        .expect("create Seedance upstream");
    let status = response.status();
    let response_body = response.text().await.expect("Seedance account response");
    assert!(
        status == StatusCode::CREATED,
        "Seedance account failed with {status}: {response_body}"
    );
    let account: Value = serde_json::from_str(&response_body).expect("Seedance account JSON");
    let response = world
        .client
        .post(format!("{}/internal/v1/model-routes", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "public_model": "seedance-public",
            "upstream_account_id": account["id"],
            "upstream_model": "seedance-upstream",
            "protocol": "generation"
        }))
        .send()
        .await
        .expect("create Seedance route");
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/generation-prices/USD/seedance-public",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"billing_unit": "second", "price_per_unit": "0.1"}))
        .send()
        .await
        .expect("create Seedance price");
    assert_eq!(response.status(), StatusCode::OK);
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "principal_external_id": "video-user",
            "alias": "video",
            "currency": "USD",
            "initial_balance": "10",
            "policy": {"allowed_models": ["seedance-public"]}
        }))
        .send()
        .await
        .expect("create Seedance key");
    assert_eq!(response.status(), StatusCode::CREATED);
    let key: Value = response.json().await.expect("Seedance key JSON");
    world.current_key = key["key"].as_str().expect("Seedance key").to_owned();
}

#[when("the client creates a five second Seedance generation")]
async fn create_seedance_generation(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/v1/videos/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .json(&json!({
            "model": "seedance-public",
            "input": {
                "duration": 5,
                "content": [{"type": "text", "text": "a fox in the wind"}]
            }
        }))
        .send()
        .await
        .expect("create Seedance generation");
    world.status = Some(response.status());
    world.response = response.json().await.expect("generation response JSON");
    world.generation_job_id = world.response["job_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok());
}

#[then("the generation eventually succeeds with an archived video costing 0.5")]
async fn generation_succeeds(world: &mut TokenCenterWorld) {
    let job_id = world.generation_job_id.expect("generation job id");
    for _ in 0..30 {
        let response = world
            .client
            .get(format!(
                "{}/self/v1/generations/{job_id}",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .expect("generation status");
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value = response.json().await.expect("generation status JSON");
        if value["status"] == "succeeded" {
            assert_eq!(value["billed_units"], 5);
            assert_eq!(value["cost"], "0.5");
            assert!(
                value["result"]["archive_objects"][0]
                    .as_str()
                    .is_some_and(|location| location.starts_with("objects/blake3/"))
            );
            assert_generation_stats(world, "seedance-public", "0.5").await;
            world.response = value;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("generation did not complete: {}", world.response);
}

#[given("the mock ComfyUI upstream completes an image workflow")]
async fn mock_comfyui_generation(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/prompt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"prompt_id": "comfy-test"})))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    Mock::given(method("GET"))
        .and(path("/history/comfy-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "comfy-test": {
                "status": {"status_str": "success", "completed": true},
                "outputs": {
                    "9": {"images": [{"filename": "result.png", "subfolder": "", "type": "output"}]}
                }
            }
        })))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    Mock::given(method("GET"))
        .and(path("/view"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(b"mock-png-content"),
        )
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[when("the service creates a metered ComfyUI route and key")]
async fn create_comfyui_route_and_key(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let response = world
        .client
        .post(format!("{}/internal/v1/upstreams", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": "comfyui",
            "driver": "comfyui",
            "config": {"base_url": mock_url, "api_prefix": ""},
            "credential": {"type": "none"}
        }))
        .send()
        .await
        .expect("create ComfyUI upstream");
    assert_eq!(response.status(), StatusCode::CREATED);
    let account: Value = response.json().await.expect("ComfyUI account JSON");
    let response = world
        .client
        .post(format!("{}/internal/v1/model-routes", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "public_model": "comfy-public",
            "upstream_account_id": account["id"],
            "upstream_model": "workflow-v1",
            "protocol": "generation"
        }))
        .send()
        .await
        .expect("create ComfyUI route");
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/generation-prices/USD/comfy-public",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"billing_unit": "job", "price_per_unit": "0.2"}))
        .send()
        .await
        .expect("create ComfyUI price");
    assert_eq!(response.status(), StatusCode::OK);
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "principal_external_id": "image-user",
            "alias": "image",
            "currency": "USD",
            "initial_balance": "10",
            "policy": {"allowed_models": ["comfy-public"]}
        }))
        .send()
        .await
        .expect("create ComfyUI key");
    assert_eq!(response.status(), StatusCode::CREATED);
    let key: Value = response.json().await.expect("ComfyUI key JSON");
    world.current_key = key["key"].as_str().expect("ComfyUI key").to_owned();
}

#[when("the client creates a ComfyUI image generation")]
async fn create_comfyui_generation(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/v1/images/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .json(&json!({
            "model": "comfy-public",
            "input": {
                "3": {"class_type": "KSampler", "inputs": {"seed": 42}},
                "9": {"class_type": "SaveImage", "inputs": {"filename_prefix": "MTC"}}
            }
        }))
        .send()
        .await
        .expect("create ComfyUI generation");
    world.status = Some(response.status());
    world.response = response.json().await.expect("ComfyUI generation JSON");
    world.generation_job_id = world.response["job_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok());
}

#[then("the ComfyUI generation eventually succeeds with an archived image costing 0.2")]
async fn comfyui_generation_succeeds(world: &mut TokenCenterWorld) {
    let job_id = world.generation_job_id.expect("ComfyUI generation job id");
    for _ in 0..30 {
        let value = world
            .client
            .get(format!(
                "{}/self/v1/generations/{job_id}",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .expect("ComfyUI generation status")
            .json::<Value>()
            .await
            .expect("ComfyUI generation status JSON");
        if value["status"] == "succeeded" {
            assert_eq!(value["billed_units"], 1);
            assert_eq!(value["cost"], "0.2");
            assert!(
                value["result"]["archive_objects"][0]
                    .as_str()
                    .is_some_and(|location| location.starts_with("objects/blake3/"))
            );
            assert_generation_stats(world, "comfy-public", "0.2").await;
            world.response = value;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("ComfyUI generation did not complete: {}", world.response);
}

async fn assert_generation_stats(world: &TokenCenterWorld, model: &str, cost: &str) {
    let stats = world
        .client
        .get(format!("{}/self/v1/stats", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("generation statistics")
        .json::<Value>()
        .await
        .expect("generation statistics JSON");
    assert_eq!(stats["summary"]["total_requests"], 1);
    assert_eq!(stats["summary"]["successful_requests"], 1);
    assert_eq!(stats["summary"]["total_cost"], cost);
    assert!(stats["by_model"].as_array().is_some_and(|rows| {
        rows.iter()
            .any(|row| row["name"] == model && row["requests"] == 1 && row["cost"] == cost)
    }));
    let operator_requests = world
        .client
        .get(format!(
            "{}/internal/v1/requests?limit=10",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("operator generation requests")
        .json::<Value>()
        .await
        .expect("operator generation requests JSON");
    assert!(operator_requests.as_array().is_some_and(|rows| {
        rows.iter().any(|row| {
            row["protocol"] == "generation" && row["model"] == model && row["cost"] == cost
        })
    }));

    let response = world
        .client
        .get(format!(
            "{}/internal/v1/request-events?after_event_at=0",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("generation event stream");
    let mut stream = response.bytes_stream();
    let lifecycle = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let mut body = String::new();
        while let Some(chunk) = stream.next().await {
            body.push_str(&String::from_utf8_lossy(
                &chunk.expect("generation SSE chunk"),
            ));
            if body.contains("\"protocol\":\"generation\"")
                && body.contains("\"event_kind\":\"started\"")
                && body.contains("\"event_kind\":\"finished\"")
            {
                return body;
            }
        }
        body
    })
    .await
    .expect("generation lifecycle events before timeout");
    assert!(lifecycle.contains("\"protocol\":\"generation\""));
}

#[given("the mock CPA subscription bridge completes Copilot OAuth and inference")]
async fn mock_copilot_subscription_bridge(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/oauth/start"))
        .and(header("authorization", "Bearer bridge-secret"))
        .and(body_partial_json(json!({"provider": "copilot"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "url": "https://github.com/login/device?user_code=TEST-CODE",
            "state": "bridge-login-state",
            "expires_at": "2099-01-01T00:00:00Z",
            "metadata": {"user_code": "TEST-CODE"}
        })))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/poll"))
        .and(header("authorization", "Bearer bridge-secret"))
        .and(body_partial_json(json!({
            "provider": "copilot",
            "state": "bridge-login-state"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "success",
            "message": "login completed",
            "auth": {
                "type": "subscription-bridge",
                "upstream": "copilot",
                "handle": "opaquehandle123",
                "label": "Copilot subscription"
            }
        })))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/execute"))
        .and(header("authorization", "Bearer bridge-secret"))
        .and(body_partial_json(json!({
            "provider": "copilot",
            "handle": "opaquehandle123",
            "model": "copilot-upstream"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "payload": {
                "id": "chatcmpl-copilot",
                "object": "chat.completion",
                "choices": [{"message": {"role": "assistant", "content": "hello from Copilot"}}],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
            }
        })))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[when("the service imports CPA Copilot and unsupported Codex auth documents twice")]
async fn import_cpa_accounts_twice(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let payload = json!({
        "bridge_base_url": mock_url,
        "bridge_secret": "bridge-import-secret",
        "auth_files": [
            {
                "filename": "github-copilot-test.json",
                "document": {
                    "type": "subscription-bridge",
                    "upstream": "copilot",
                    "handle": "opaqueimporthandle123",
                    "label": "Imported Copilot"
                }
            },
            {
                "filename": "codex-test.json",
                "document": {
                    "type": "codex",
                    "access_token": "codex-import-secret",
                    "refresh_token": "codex-refresh-secret"
                }
            }
        ]
    });
    for attempt in 0..2 {
        let response = world
            .client
            .post(format!(
                "{}/internal/v1/imports/cpa/subscription-accounts",
                world.service_url
            ))
            .bearer_auth("test-service-token")
            .json(&payload)
            .send()
            .await
            .expect("import CPA auth documents");
        world.status = Some(response.status());
        world.response = response.json().await.expect("CPA import JSON");
        let account_id = world.response["imported"][0]["account"]["id"]
            .as_str()
            .and_then(|value| Uuid::parse_str(value).ok())
            .expect("imported account id");
        if attempt == 0 {
            world.oauth_upstream_id = Some(account_id);
        } else {
            assert_eq!(world.oauth_upstream_id, Some(account_id));
        }
    }
}

#[then(
    "one opaque CPA account is imported and unsupported OAuth is skipped without echoing secrets"
)]
async fn cpa_import_is_safe(world: &mut TokenCenterWorld) {
    assert_eq!(world.status, Some(StatusCode::CREATED));
    assert_eq!(world.response["imported"].as_array().map(Vec::len), Some(1));
    assert_eq!(world.response["imported"][0]["provider"], "copilot");
    assert_eq!(world.response["skipped"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        world.response["skipped"][0]["reason"],
        "requires_provider_adapter"
    );
    let serialized = world.response.to_string();
    assert!(!serialized.contains("opaqueimporthandle123"));
    assert!(!serialized.contains("bridge-import-secret"));
    assert!(!serialized.contains("codex-import-secret"));
    assert!(!serialized.contains("codex-refresh-secret"));
}

#[when("the service creates a Copilot bridge account route and key")]
async fn create_copilot_bridge_route_and_key(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let start = world
        .client
        .post(format!(
            "{}/internal/v1/oauth/subscription-bridge/start",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({
            "account_name": "copilot-subscription",
            "provider": "copilot",
            "base_url": mock_url,
            "bridge_secret": "bridge-secret"
        }))
        .send()
        .await
        .expect("start Copilot bridge OAuth");
    assert_eq!(start.status(), StatusCode::OK);
    let started: Value = start.json().await.expect("Copilot start JSON");
    assert!(
        started["login_url"]
            .as_str()
            .is_some_and(|url| url.contains("user_code=TEST-CODE"))
    );
    world.subscription_session_token = started["session_token"]
        .as_str()
        .expect("subscription session token")
        .to_owned();
    let poll = world
        .client
        .post(format!(
            "{}/internal/v1/oauth/subscription-bridge/poll",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"session_token": world.subscription_session_token}))
        .send()
        .await
        .expect("poll Copilot bridge OAuth");
    assert_eq!(poll.status(), StatusCode::CREATED);
    let account: Value = poll.json().await.expect("Copilot account JSON");
    assert_eq!(account["auth_kind"], "oauth");
    let route = world
        .client
        .post(format!("{}/internal/v1/model-routes", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "public_model": "copilot-public",
            "upstream_account_id": account["id"],
            "upstream_model": "copilot-upstream",
            "protocol": "openai"
        }))
        .send()
        .await
        .expect("create Copilot route");
    assert_eq!(route.status(), StatusCode::CREATED);
    let price = world
        .client
        .post(format!(
            "{}/internal/v1/prices/USD/copilot-public",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"input_per_million": "1", "output_per_million": "1"}))
        .send()
        .await
        .expect("create Copilot price");
    assert_eq!(price.status(), StatusCode::OK);
    let key = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "principal_external_id": "copilot-user",
            "alias": "copilot",
            "currency": "USD",
            "initial_balance": "10",
            "policy": {"allowed_models": ["copilot-public"]}
        }))
        .send()
        .await
        .expect("create Copilot key");
    assert_eq!(key.status(), StatusCode::CREATED);
    let key: Value = key.json().await.expect("Copilot key JSON");
    world.current_key = key["key"].as_str().expect("Copilot key").to_owned();
}

#[then("the Copilot response is unwrapped without exposing the bridge handle")]
async fn copilot_response_is_unwrapped(world: &mut TokenCenterWorld) {
    assert_eq!(
        world.response["choices"][0]["message"]["content"],
        "hello from Copilot"
    );
    assert!(world.response.get("payload").is_none());
    assert!(!world.response.to_string().contains("opaquehandle123"));
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

#[given("the mock Cursor OAuth server and compatible upstream are ready")]
async fn mock_cursor_oauth(world: &mut TokenCenterWorld) {
    Mock::given(method("GET"))
        .and(path("/cursor/auth/poll"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accessToken": "cursor-access-1",
            "refreshToken": "cursor-refresh-1"
        })))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    Mock::given(method("POST"))
        .and(path("/cursor/auth/exchange_user_api_key"))
        .and(header("authorization", "Bearer cursor-refresh-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accessToken": "cursor-access-2"
        })))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    for access_token in ["cursor-access-1", "cursor-access-2"] {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header(
                "authorization",
                format!("Bearer {access_token}").as_str(),
            ))
            .and(body_partial_json(json!({"model": "cursor-upstream"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": format!("chatcmpl-{access_token}"),
                "choices": [{"message": {"role": "assistant", "content": "cursor"}}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 2}
            })))
            .mount(world.mock.as_ref().expect("mock server"))
            .await;
    }
}

#[when("the service starts a Cursor OAuth login")]
async fn start_cursor_oauth(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/oauth/cursor/start",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({
            "account_name": "cursor-oauth",
            "provider_driver": "http-json",
            "provider_config": {"base_url": mock_url},
            "endpoints": {
                "login_url": format!("{mock_url}/cursor/loginDeepControl"),
                "poll_url": format!("{mock_url}/cursor/auth/poll"),
                "refresh_url": format!("{mock_url}/cursor/auth/exchange_user_api_key")
            }
        }))
        .send()
        .await
        .expect("start Cursor OAuth");
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response.json().await.expect("Cursor OAuth start JSON");
    world.cursor_login_url = value["login_url"]
        .as_str()
        .expect("Cursor login URL")
        .to_owned();
    world.cursor_session_token = value["session_token"]
        .as_str()
        .expect("Cursor session token")
        .to_owned();
}

#[then("the Cursor login URL contains a PKCE challenge without exposing the verifier")]
async fn cursor_login_contains_pkce(world: &mut TokenCenterWorld) {
    let login_url = url::Url::parse(&world.cursor_login_url).expect("valid Cursor login URL");
    let query = login_url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    assert!(query.contains_key("challenge"));
    assert!(query.contains_key("uuid"));
    assert!(!query.contains_key("verifier"));
    assert!(!world.cursor_session_token.contains("cursor-oauth"));
}

#[when("the service polls the completed Cursor OAuth login")]
async fn poll_cursor_oauth(world: &mut TokenCenterWorld) {
    let poll = || {
        world
            .client
            .post(format!(
                "{}/internal/v1/oauth/cursor/poll",
                world.service_url
            ))
            .bearer_auth("test-service-token")
            .json(&json!({"session_token": world.cursor_session_token}))
            .send()
    };
    let response = poll().await.expect("poll Cursor OAuth");
    assert_eq!(response.status(), StatusCode::CREATED);
    let value: Value = response.json().await.expect("Cursor account JSON");
    assert_eq!(value["auth_kind"], "oauth");
    assert!(value.get("credential").is_none());
    world.cursor_account_id = Some(
        Uuid::from_str(value["id"].as_str().expect("Cursor account id"))
            .expect("Cursor account UUID"),
    );
    world.cursor_generation = value["credential_generation"]
        .as_i64()
        .expect("Cursor generation");
    let retry = poll().await.expect("retry completed Cursor OAuth poll");
    assert_eq!(retry.status(), StatusCode::CREATED);
    let retry_value: Value = retry.json().await.expect("retried Cursor account JSON");
    assert_eq!(
        retry_value["id"],
        world
            .cursor_account_id
            .expect("Cursor account id")
            .to_string()
    );
}

#[when(expr = "the service routes model {string} through the Cursor OAuth account")]
async fn route_cursor_model(world: &mut TokenCenterWorld, model: String) {
    create_route(
        world,
        &model,
        &world
            .cursor_account_id
            .expect("Cursor account id")
            .to_string(),
        "cursor-upstream",
    )
    .await;
}

#[when("the service refreshes the Cursor OAuth account")]
async fn refresh_cursor_oauth(world: &mut TokenCenterWorld) {
    let account_id = world.cursor_account_id.expect("Cursor account id");
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/upstreams/{account_id}/oauth/refresh",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("refresh Cursor OAuth");
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response.json().await.expect("refreshed Cursor account");
    assert_eq!(value["id"], account_id.to_string());
    world.cursor_generation = value["credential_generation"]
        .as_i64()
        .expect("refreshed generation");
}

#[then("the refreshed Cursor account keeps its id and uses generation 2")]
async fn refreshed_cursor_account_is_stable(world: &mut TokenCenterWorld) {
    assert!(world.cursor_account_id.is_some());
    assert_eq!(world.cursor_generation, 2);
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
            == Some("Bearer cursor-access-2")
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
    world.stable_account_id = Some(
        Uuid::from_str(world.response["account_id"].as_str().expect("account id"))
            .expect("UUID account id"),
    );
}

#[when("the service creates and grants an unspent subscription key")]
async fn create_subscription_grant(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .header("idempotency-key", "registration:refund-user")
        .json(&json!({
            "principal_external_id": "refund-user",
            "alias": "refund",
            "currency": "USD",
            "initial_balance": "0"
        }))
        .send()
        .await
        .expect("create refund key");
    assert_eq!(response.status(), StatusCode::CREATED);
    let issued: Value = response.json().await.expect("refund key JSON");
    world.current_key = issued["key"].as_str().expect("refund key").to_owned();
    world.stable_account_id = Some(
        Uuid::from_str(issued["account_id"].as_str().expect("refund account id"))
            .expect("UUID refund account id"),
    );
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/accounts/{}/grants",
            world.service_url,
            world.stable_account_id.expect("refund account id")
        ))
        .bearer_auth("test-service-token")
        .header("idempotency-key", "subscription:refund-subscription:grant")
        .json(&json!({"amount": "7.5", "source": "subscription:pro"}))
        .send()
        .await
        .expect("grant subscription credit");
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[when("the service reverses that subscription grant twice")]
async fn reverse_subscription_grant_twice(world: &mut TokenCenterWorld) {
    for _ in 0..2 {
        let response = world
            .client
            .post(format!(
                "{}/internal/v1/accounts/{}/grant-reversals",
                world.service_url,
                world.stable_account_id.expect("refund account id")
            ))
            .bearer_auth("test-service-token")
            .header(
                "idempotency-key",
                "subscription:refund-subscription:reversal",
            )
            .json(&json!({
                "grant_idempotency_key": "subscription:refund-subscription:grant",
                "source": "subscription_cancelled"
            }))
            .send()
            .await
            .expect("reverse subscription credit");
        world.status = Some(response.status());
        world.response = response.json().await.expect("grant reversal JSON");
        assert_eq!(world.response["reversed"], "7.5");
    }
}

#[then("the subscription balance is zero after one logical reversal")]
async fn subscription_balance_is_zero(world: &mut TokenCenterWorld) {
    assert_eq!(world.status, Some(StatusCode::CREATED));
    let key = world
        .client
        .get(format!("{}/self/v1/key", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("read refund key")
        .json::<Value>()
        .await
        .expect("refund key JSON");
    assert_eq!(key["available_balance"], "0");
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

#[when(expr = "the client sends a parent-linked compacted turn for model {string}")]
async fn send_parent_linked_compaction(world: &mut TokenCenterWorld, model: String) {
    let first = world
        .client
        .post(format!("{}/v1/chat/completions", world.service_url))
        .bearer_auth(&world.current_key)
        .header("x-mtc-conversation-id", "structured-session-a")
        .header("x-mtc-turn-id", "turn-a")
        .header("x-mtc-branch-id", "main")
        .json(&json!({
            "model": model,
            "messages": [
                {"role": "user", "content": "first question"},
                {"role": "assistant", "content": "a long answer"},
                {"role": "user", "content": "follow-up"}
            ]
        }))
        .send()
        .await
        .expect("explicit parent request");
    assert_eq!(first.status(), StatusCode::OK);

    let second = world
        .client
        .post(format!("{}/v1/chat/completions", world.service_url))
        .bearer_auth(&world.current_key)
        .header("x-mtc-conversation-id", "structured-session-a")
        .header("x-mtc-turn-id", "turn-b")
        .header("x-mtc-parent-turn-id", "turn-a")
        .header("x-mtc-branch-id", "main")
        .header("x-mtc-compaction", "true")
        .json(&json!({
            "model": model,
            "messages": [{"role": "user", "content": "compacted summary"}]
        }))
        .send()
        .await
        .expect("compacted child request");
    world.status = Some(second.status());
}

#[then(expr = "the response status is {int}")]
async fn response_status(world: &mut TokenCenterWorld, expected: u16) {
    assert_eq!(
        world.status,
        Some(StatusCode::from_u16(expected).expect("valid HTTP status"))
    );
}

#[then("the operator realtime stream contains started and finished events")]
async fn realtime_stream_contains_request_lifecycle(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .get(format!(
            "{}/internal/v1/request-events?after_event_at=0",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("request event stream");
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.bytes_stream();
    let lifecycle = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let mut body = String::new();
        while let Some(chunk) = stream.next().await {
            body.push_str(&String::from_utf8_lossy(&chunk.expect("SSE chunk")));
            if body.contains("\"event_kind\":\"started\"")
                && body.contains("\"event_kind\":\"finished\"")
            {
                return body;
            }
        }
        body
    })
    .await
    .expect("request lifecycle events before timeout");
    assert!(lifecycle.contains("\"event_kind\":\"started\""));
    assert!(lifecycle.contains("\"event_kind\":\"finished\""));
}

#[when("the bootstrap service creates a tenant scoped service token")]
async fn create_scoped_service_token(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/internal/v1/service-tokens", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": "memeloop-web-test",
            "scopes": ["keys:write"],
            "tenant_external_id": "scoped-tenant"
        }))
        .send()
        .await
        .expect("create scoped service token");
    assert_eq!(response.status(), StatusCode::CREATED);
    let value: Value = response.json().await.expect("service token JSON");
    world.current_service_token = value["token"]
        .as_str()
        .expect("issued service token")
        .to_owned();
    world.stable_service_id = Some(
        Uuid::from_str(value["service_id"].as_str().expect("service id")).expect("service UUID"),
    );
}

#[then("the scoped service token can create a key in its tenant")]
async fn scoped_service_creates_tenant_key(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth(&world.current_service_token)
        .json(&json!({
            "tenant_external_id": "scoped-tenant",
            "principal_external_id": "member-1",
            "alias": "member-key",
            "currency": "USD",
            "initial_balance": "1"
        }))
        .send()
        .await
        .expect("create tenant key with scoped token");
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[then("the scoped service token cannot update global prices")]
async fn scoped_service_cannot_update_prices(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/prices/USD/global-model",
            world.service_url
        ))
        .bearer_auth(&world.current_service_token)
        .json(&json!({"input_per_million": "1", "output_per_million": "1"}))
        .send()
        .await
        .expect("reject global price update");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[when("the bootstrap service rotates the scoped service token")]
async fn rotate_scoped_service_token(world: &mut TokenCenterWorld) {
    world.old_service_token = world.current_service_token.clone();
    let service_id = world.stable_service_id.expect("stable service id");
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/service-tokens/{service_id}/rotate",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("rotate scoped service token");
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response.json().await.expect("rotated service token JSON");
    assert_eq!(value["service_id"], service_id.to_string());
    assert_eq!(value["credential_generation"], 2);
    world.current_service_token = value["token"]
        .as_str()
        .expect("rotated service token")
        .to_owned();
}

#[then("the old service token is rejected")]
async fn old_service_token_is_rejected(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth(&world.old_service_token)
        .json(&json!({
            "tenant_external_id": "scoped-tenant",
            "principal_external_id": "member-old",
            "alias": "old-token",
            "currency": "USD"
        }))
        .send()
        .await
        .expect("old service token check");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[then("the rotated service token retains its stable service id")]
async fn rotated_service_token_retains_identity(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth(&world.current_service_token)
        .json(&json!({
            "tenant_external_id": "scoped-tenant",
            "principal_external_id": "member-new",
            "alias": "new-token",
            "currency": "USD"
        }))
        .send()
        .await
        .expect("rotated service token check");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert!(world.stable_service_id.is_some());
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

#[then("the compacted request is linked to its explicit parent turn")]
async fn compacted_request_links_to_parent(world: &mut TokenCenterWorld) {
    let clusters = world
        .client
        .get(format!("{}/self/v1/conversations", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("structured conversation clusters")
        .json::<Value>()
        .await
        .expect("structured conversation clusters JSON");
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
        .expect("structured conversation detail")
        .json::<Value>()
        .await
        .expect("structured conversation detail JSON");
    assert_eq!(detail["edges"][0]["relation"], "compacts");
    assert_eq!(detail["edges"][0]["evidence"]["explicit_parent"], true);
    assert_eq!(detail["edges"][0]["evidence"]["inference_version"], 2);
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
