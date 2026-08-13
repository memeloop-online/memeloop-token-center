use std::{
    fmt,
    fs::File,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
};

use cucumber::{World, given, then, when};
use futures_util::StreamExt;
use memeloop_token_center::{AppState, api, config::Config, worker};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, header, header_exists, method, path},
};

#[path = "steps/security_acceptance.rs"]
mod security_acceptance;

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
    matrix_global_service_token: String,
    matrix_scoped_service_token: String,
    matrix_first_key: String,
    matrix_second_key: String,
    matrix_first_request_id: Option<Uuid>,
    matrix_second_request_id: Option<Uuid>,
    expected_policy: Value,
    expected_balance: String,
    import_database_url: String,
    import_tenant: String,
    import_source: String,
    import_sqlite_path: Option<PathBuf>,
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
            matrix_global_service_token: String::new(),
            matrix_scoped_service_token: String::new(),
            matrix_first_key: String::new(),
            matrix_second_key: String::new(),
            matrix_first_request_id: None,
            matrix_second_request_id: None,
            expected_policy: Value::Null,
            expected_balance: String::new(),
            import_database_url: String::new(),
            import_tenant: String::new(),
            import_source: String::new(),
            import_sqlite_path: None,
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
        .and(header_exists("idempotency-key"))
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

#[given("the mock Seedance upstream transiently fails once and then completes")]
async fn mock_seedance_retry(world: &mut TokenCenterWorld) {
    let server = world.mock.as_ref().expect("mock server");
    Mock::given(method("POST"))
        .and(path("/api/v3/contents/generations/tasks"))
        .and(header("authorization", "Bearer seedance-secret"))
        .and(header_exists("idempotency-key"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "temporary"})))
        .with_priority(1)
        .up_to_n_times(1)
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v3/contents/generations/tasks"))
        .and(header("authorization", "Bearer seedance-secret"))
        .and(header_exists("idempotency-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "cgt-retry"})))
        .expect(1)
        .mount(server)
        .await;
    let mock_url = server.uri();
    Mock::given(method("GET"))
        .and(path("/api/v3/contents/generations/tasks/cgt-retry"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "cgt-retry",
            "status": "succeeded",
            "duration": "5",
            "content": {"video_url": format!("{mock_url}/assets/retry-video.mp4")}
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/assets/retry-video.mp4"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"mock-retry-video"))
        .mount(server)
        .await;
}

#[given("the mock Seedance upstream rejects the generation request")]
async fn mock_seedance_rejection(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/api/v3/contents/generations/tasks"))
        .and(header("authorization", "Bearer seedance-secret"))
        .and(header_exists("idempotency-key"))
        .respond_with(ResponseTemplate::new(400).set_body_string("request rejected"))
        .expect(1)
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

#[then("both Seedance submission attempts use the same upstream idempotency key")]
async fn seedance_retries_are_idempotent(world: &mut TokenCenterWorld) {
    let requests = world
        .mock
        .as_ref()
        .expect("mock server")
        .received_requests()
        .await
        .expect("request recording enabled");
    let keys = requests
        .iter()
        .filter(|request| {
            request.method.as_str() == "POST"
                && request.url.path() == "/api/v3/contents/generations/tasks"
        })
        .map(|request| {
            request
                .headers
                .get("idempotency-key")
                .expect("upstream idempotency header")
                .to_str()
                .expect("ASCII idempotency header")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0], keys[1]);
    assert_eq!(
        keys[0],
        world
            .generation_job_id
            .expect("generation job id")
            .to_string()
    );
}

#[then("the rejected generation fails once and refunds its entire reservation")]
async fn rejected_generation_is_refunded(world: &mut TokenCenterWorld) {
    let job_id = world.generation_job_id.expect("generation job id");
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
            .expect("rejected generation status")
            .json::<Value>()
            .await
            .expect("rejected generation status JSON");
        if value["status"] == "failed" {
            assert_eq!(value["billed_units"], 0);
            assert_eq!(value["cost"], "0");
            assert_eq!(value["error_code"], "generation_rejected");
            let key = world
                .client
                .get(format!("{}/self/v1/key", world.service_url))
                .bearer_auth(&world.current_key)
                .send()
                .await
                .expect("key after rejected generation")
                .json::<Value>()
                .await
                .expect("key JSON after rejected generation");
            assert_eq!(key["available_balance"], "10");
            world.response = value;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("rejected generation did not fail: {}", world.response);
}

#[given("the mock ComfyUI upstream completes an image workflow")]
async fn mock_comfyui_generation(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/prompt"))
        .and(header_exists("idempotency-key"))
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
            "config": {
                "base_url": mock_url,
                "api_prefix": "",
                "workflow_id": "workflow-v1",
                "workflow_template": {
                    "3": {"class_type": "KSampler", "inputs": {"seed": {"$mtc_param": "seed"}}},
                    "9": {"class_type": "SaveImage", "inputs": {"filename_prefix": "MTC"}}
                }
            },
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
                "parameters": {"seed": 42}
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

#[given("the mock OpenAI Images upstream returns a generated icon")]
async fn mock_openai_image_generation(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(header("authorization", "Bearer image-secret"))
        .and(body_partial_json(json!({
            "model": "gpt-image-upstream",
            "prompt": "a compact token loop icon"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 1,
            "data": [{"b64_json": "bW9jay1wbmc="}]
        })))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[when("the service creates a metered OpenAI Images route and key")]
async fn create_openai_image_route_and_key(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let response = world
        .client
        .post(format!("{}/internal/v1/upstreams", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": "openai-images",
            "driver": "http-json",
            "config": {"base_url": mock_url},
            "credential": {"type": "api_key", "value": "image-secret"}
        }))
        .send()
        .await
        .expect("create Images upstream");
    assert_eq!(response.status(), StatusCode::CREATED);
    let account: Value = response.json().await.expect("Images account JSON");
    let response = world
        .client
        .post(format!("{}/internal/v1/model-routes", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "public_model": "gpt-image-public",
            "upstream_account_id": account["id"],
            "upstream_model": "gpt-image-upstream",
            "protocol": "generation"
        }))
        .send()
        .await
        .expect("create Images route");
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/generation-prices/USD/gpt-image-public",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"billing_unit": "image", "price_per_unit": "0.3"}))
        .send()
        .await
        .expect("create Images price");
    assert_eq!(response.status(), StatusCode::OK);
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "principal_external_id": "image-api-user",
            "alias": "image-api",
            "currency": "USD",
            "initial_balance": "10",
            "policy": {"allowed_models": ["gpt-image-public"]}
        }))
        .send()
        .await
        .expect("create Images key");
    assert_eq!(response.status(), StatusCode::CREATED);
    let key: Value = response.json().await.expect("Images key JSON");
    world.current_key = key["key"].as_str().expect("Images key").to_owned();
    world.stable_key_id = key["key_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok());
}

#[when("the client creates an OpenAI-compatible image")]
async fn create_openai_compatible_image(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/v1/images/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .json(&json!({
            "model": "gpt-image-public",
            "prompt": "a compact token loop icon",
            "n": 1,
            "size": "1024x1024"
        }))
        .send()
        .await
        .expect("create OpenAI-compatible image");
    world.status = Some(response.status());
    world.response = response.json().await.expect("Images response JSON");
}

#[then("the OpenAI image response is archived and costs 0.3")]
async fn openai_image_is_archived_and_metered(world: &mut TokenCenterWorld) {
    assert_eq!(world.response["data"][0]["b64_json"], "bW9jay1wbmc=");
    for _ in 0..30 {
        let stats: Value = world
            .client
            .get(format!("{}/self/v1/stats", world.service_url))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .expect("image stats")
            .json()
            .await
            .expect("image stats JSON");
        if stats["summary"]["total_requests"] == 1 {
            assert_eq!(stats["summary"]["total_cost"], "0.3");
            let requests: Value = world
                .client
                .get(format!("{}/self/v1/requests?limit=1", world.service_url))
                .bearer_auth(&world.current_key)
                .send()
                .await
                .expect("image request history")
                .json()
                .await
                .expect("image request history JSON");
            assert_eq!(requests[0]["protocol"], "openai-image");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("OpenAI image request was not metered");
}

#[given("the mock Codex Responses upstream returns a generated icon")]
async fn mock_codex_responses_image_generation(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer codex-image-secret"))
        .and(body_partial_json(json!({
            "model": "gpt-5.6-sol",
            "tools": [{"type": "image_generation", "model": "gpt-image-2"}],
            "tool_choice": {"type": "image_generation"},
            "stream": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_image_test",
            "output": [{
                "type": "image_generation_call",
                "id": "ig_test",
                "result": "Y29kZXgtbW9jay1wbmc="
            }],
            "usage": {"input_tokens": 12, "output_tokens": 34, "total_tokens": 46}
        })))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[when("the service creates a metered Codex Responses image route and key")]
async fn create_codex_responses_image_route_and_key(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let response = world
        .client
        .post(format!("{}/internal/v1/upstreams", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": "codex-responses-images",
            "driver": "http-json",
            "config": {
                "base_url": mock_url,
                "image_api_mode": "responses-tool",
                "image_main_model": "gpt-5.6-sol"
            },
            "credential": {"type": "api_key", "value": "codex-image-secret"}
        }))
        .send()
        .await
        .expect("create Codex Responses Images upstream");
    assert_eq!(response.status(), StatusCode::CREATED);
    let account: Value = response.json().await.expect("Codex Images account JSON");
    let response = world
        .client
        .post(format!("{}/internal/v1/model-routes", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "public_model": "codex-image-public",
            "upstream_account_id": account["id"],
            "upstream_model": "gpt-image-2",
            "protocol": "generation"
        }))
        .send()
        .await
        .expect("create Codex Responses Images route");
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/generation-prices/USD/codex-image-public",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"billing_unit": "image", "price_per_unit": "0.4"}))
        .send()
        .await
        .expect("create Codex Responses Images price");
    assert_eq!(response.status(), StatusCode::OK);
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "principal_external_id": "codex-image-api-user",
            "alias": "codex-image-api",
            "currency": "USD",
            "initial_balance": "10",
            "policy": {"allowed_models": ["codex-image-public"]}
        }))
        .send()
        .await
        .expect("create Codex Responses Images key");
    assert_eq!(response.status(), StatusCode::CREATED);
    let key: Value = response.json().await.expect("Codex Images key JSON");
    world.current_key = key["key"].as_str().expect("Codex Images key").to_owned();
}

#[when("the client creates a Codex-backed OpenAI-compatible image")]
async fn create_codex_backed_openai_image(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/v1/images/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .json(&json!({
            "model": "codex-image-public",
            "prompt": "a compact token loop icon",
            "n": 1,
            "size": "1024x1024",
            "quality": "medium",
            "output_format": "png"
        }))
        .send()
        .await
        .expect("create Codex-backed OpenAI-compatible image");
    world.status = Some(response.status());
    world.response = response.json().await.expect("Codex Images response JSON");
}

#[then("the Codex-backed image response is archived and costs 0.4")]
async fn codex_image_is_archived_and_metered(world: &mut TokenCenterWorld) {
    assert_eq!(
        world.response["data"][0]["b64_json"],
        "Y29kZXgtbW9jay1wbmc="
    );
    let stats: Value = world
        .client
        .get(format!("{}/self/v1/stats", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("Codex image stats")
        .json()
        .await
        .expect("Codex image stats JSON");
    assert_eq!(stats["summary"]["total_requests"], 1);
    assert_eq!(stats["summary"]["successful_requests"], 1);
    assert_eq!(stats["summary"]["total_cost"], "0.4");
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
    let job_id = world.generation_job_id.expect("generation job id");
    for (path, token) in [
        (
            format!("/self/v1/requests/{job_id}"),
            world.current_key.as_str(),
        ),
        (
            format!("/internal/v1/requests/{job_id}"),
            "test-service-token",
        ),
    ] {
        let detail = world
            .client
            .get(format!("{}{path}", world.service_url))
            .bearer_auth(token)
            .send()
            .await
            .expect("generation request detail");
        assert_eq!(detail.status(), StatusCode::OK);
        let detail: Value = detail.json().await.expect("generation request detail JSON");
        assert_eq!(detail["protocol"], "generation");
        assert_eq!(detail["model"], model);
        assert_eq!(detail["cost"], cost);
        assert_eq!(detail["archive_complete"], true);
        assert!(detail["request_body"].is_object());
        assert!(
            detail["response_body"]["archive_objects"]
                .as_array()
                .is_some_and(|objects| !objects.is_empty())
        );
    }

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
                "filename": "github-copilot-opaqueimporthandle123.json",
                "document": {
                    "type": "subscription-bridge",
                    "upstream": "copilot",
                    "handle": "opaqueimporthandle123",
                    "label": "Imported Copilot"
                }
            },
            {
                "filename": "codex-codex-import-secret.json",
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

#[given("the mock OpenAI upstream returns cached priority usage")]
async fn mock_cached_priority_openai(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"service_tier": "priority"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-cache-tier",
            "service_tier": "priority",
            "choices": [{"message": {"role": "assistant", "content": "cached"}}],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "prompt_tokens_details": {"cached_tokens": 40}
            }
        })))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[when(expr = "the operator configures cache-aware default and priority prices for model {string}")]
async fn configure_cache_tier_prices(world: &mut TokenCenterWorld, model: String) {
    for (service_tier, input, cached, cache_write, output) in [
        ("default", "1", "0.1", "2", "3"),
        ("priority", "5", "0.5", "6", "7"),
    ] {
        let response = world
            .client
            .post(format!(
                "{}/internal/v1/prices/USD/{model}",
                world.service_url
            ))
            .bearer_auth("test-service-token")
            .json(&json!({
                "service_tier": service_tier,
                "input_per_million": input,
                "cached_input_per_million": cached,
                "cache_write_per_million": cache_write,
                "output_per_million": output
            }))
            .send()
            .await
            .expect("configure cache-aware tier price");
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[when(expr = "the client calls priority model {string}")]
async fn call_priority_model(world: &mut TokenCenterWorld, model: String) {
    let response = world
        .client
        .post(format!("{}/v1/chat/completions", world.service_url))
        .bearer_auth(&world.current_key)
        .json(&json!({
            "model": model,
            "service_tier": "priority",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("priority model request");
    world.status = Some(response.status());
    world.response = response.json().await.unwrap_or(Value::Null);
}

#[then(expr = "the cache-aware priority request costs {float} for {int} tokens")]
async fn cache_tier_request_cost(world: &mut TokenCenterWorld, cost: f64, tokens: i64) {
    for _ in 0..20 {
        let stats = world
            .client
            .get(format!("{}/self/v1/stats", world.service_url))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .expect("cache-aware stats")
            .json::<Value>()
            .await
            .expect("cache-aware stats JSON");
        if stats["summary"]["total_requests"] == 1 {
            assert_eq!(
                stats["summary"]["input_tokens"]
                    .as_i64()
                    .unwrap_or_default()
                    + stats["summary"]["output_tokens"]
                        .as_i64()
                        .unwrap_or_default(),
                tokens
            );
            assert_eq!(
                stats["summary"]["total_cost"],
                Value::String(format!("{cost:.6}").trim_end_matches('0').to_owned())
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("cache-aware request did not settle");
}

#[when(expr = "the service records requests for tenants {string} and {string}")]
async fn record_requests_for_two_tenants(
    world: &mut TokenCenterWorld,
    first_tenant: String,
    second_tenant: String,
) {
    let service = world
        .client
        .post(format!("{}/internal/v1/service-tokens", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": "review-operator",
            "scopes": ["requests:read"]
        }))
        .send()
        .await
        .expect("create issued global operator credential");
    assert_eq!(service.status(), StatusCode::CREATED);
    let service: Value = service
        .json()
        .await
        .expect("global operator credential JSON");
    world.current_service_token = service["token"]
        .as_str()
        .expect("issued global service credential")
        .to_owned();
    let price = world
        .client
        .post(format!(
            "{}/internal/v1/prices/USD/global-stats-model",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"input_per_million": "1", "output_per_million": "1"}))
        .send()
        .await
        .expect("create global stats price");
    assert_eq!(price.status(), StatusCode::OK);
    for tenant in [first_tenant, second_tenant] {
        let response = world
            .client
            .post(format!("{}/internal/v1/keys", world.service_url))
            .bearer_auth("test-service-token")
            .json(&json!({
                "tenant_external_id": tenant,
                "principal_external_id": format!("{tenant}-user"),
                "alias": "imported",
                "currency": "USD",
                "initial_balance": "10",
                "policy": {"allowed_models": ["global-stats-model"]}
            }))
            .send()
            .await
            .expect("create tenant credential");
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: Value = response.json().await.expect("tenant credential JSON");
        let credential = body["key"].as_str().expect("tenant credential");
        let response = world
            .client
            .post(format!("{}/v1/chat/completions", world.service_url))
            .bearer_auth(credential)
            .json(&json!({
                "model": "global-stats-model",
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("call tenant model");
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[then("global operator statistics contain both tenant requests")]
async fn global_operator_stats_contain_both(world: &mut TokenCenterWorld) {
    let mut observed = 0;
    for _ in 0..20 {
        let response = world
            .client
            .get(format!("{}/internal/v1/stats", world.service_url))
            .bearer_auth(&world.current_service_token)
            .send()
            .await
            .expect("global operator stats");
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.expect("global operator stats JSON");
        observed = body["summary"]["total_requests"].as_i64().unwrap_or(0);
        if observed == 2 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(observed, 2);
}

#[then(expr = "tenant filtered operator statistics contain only {string}")]
async fn tenant_filtered_operator_stats(world: &mut TokenCenterWorld, tenant: String) {
    let response = world
        .client
        .get(format!(
            "{}/internal/v1/stats?tenant_external_id={tenant}",
            world.service_url
        ))
        .bearer_auth(&world.current_service_token)
        .send()
        .await
        .expect("tenant operator stats");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("tenant operator stats JSON");
    assert_eq!(body["summary"]["total_requests"], 1);
}

async fn create_matrix_service_token(
    world: &TokenCenterWorld,
    name: &str,
    scopes: &[&str],
    tenant: Option<&str>,
) -> String {
    let response = world
        .client
        .post(format!("{}/internal/v1/service-tokens", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": name,
            "scopes": scopes,
            "tenant_external_id": tenant
        }))
        .send()
        .await
        .expect("create authorization-matrix service credential");
    assert_eq!(response.status(), StatusCode::CREATED);
    response
        .json::<Value>()
        .await
        .expect("authorization-matrix service credential JSON")["token"]
        .as_str()
        .expect("issued authorization-matrix token")
        .to_owned()
}

async fn create_matrix_key(world: &TokenCenterWorld, tenant: &str, principal: &str) -> String {
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "tenant_external_id": tenant,
            "principal_external_id": principal,
            "alias": format!("{tenant}-credential"),
            "currency": "USD",
            "initial_balance": "10",
            "policy": {"allowed_models": ["matrix-model"]}
        }))
        .send()
        .await
        .expect("create authorization-matrix downstream credential");
    assert_eq!(response.status(), StatusCode::CREATED);
    response
        .json::<Value>()
        .await
        .expect("authorization-matrix downstream credential JSON")["key"]
        .as_str()
        .expect("issued authorization-matrix key")
        .to_owned()
}

async fn matrix_request_id(world: &TokenCenterWorld, tenant: &str) -> Uuid {
    let response = world
        .client
        .get(format!(
            "{}/internal/v1/requests?tenant_external_id={tenant}&limit=10",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("list authorization-matrix tenant requests");
    assert_eq!(response.status(), StatusCode::OK);
    let requests: Value = response
        .json()
        .await
        .expect("authorization-matrix request list JSON");
    assert_eq!(requests.as_array().map(Vec::len), Some(1));
    Uuid::parse_str(
        requests[0]["request_id"]
            .as_str()
            .expect("matrix request id"),
    )
    .expect("matrix request UUID")
}

#[when("the service prepares two tenants and credentials for the authorization matrix")]
async fn prepare_authorization_matrix(world: &mut TokenCenterWorld) {
    let price = world
        .client
        .post(format!(
            "{}/internal/v1/prices/USD/matrix-model",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"input_per_million": "1", "output_per_million": "1"}))
        .send()
        .await
        .expect("create authorization-matrix model price");
    assert_eq!(price.status(), StatusCode::OK);

    world.matrix_global_service_token = create_matrix_service_token(
        world,
        "matrix-global-reader",
        &["keys:read", "requests:read"],
        None,
    )
    .await;
    world.matrix_scoped_service_token = create_matrix_service_token(
        world,
        "matrix-first-tenant",
        &["keys:read", "requests:read", "prices:write"],
        Some("matrix-first"),
    )
    .await;
    world.matrix_first_key = create_matrix_key(world, "matrix-first", "first-user").await;
    world.matrix_second_key = create_matrix_key(world, "matrix-second", "second-user").await;

    for key in [&world.matrix_first_key, &world.matrix_second_key] {
        let response = world
            .client
            .post(format!("{}/v1/chat/completions", world.service_url))
            .bearer_auth(key)
            .json(&json!({
                "model": "matrix-model",
                "messages": [{"role": "user", "content": "matrix probe"}]
            }))
            .send()
            .await
            .expect("send authorization-matrix model request");
        assert_eq!(response.status(), StatusCode::OK);
    }
    world.matrix_first_request_id = Some(matrix_request_id(world, "matrix-first").await);
    world.matrix_second_request_id = Some(matrix_request_id(world, "matrix-second").await);
}

#[then("the global service credential lists both tenants and reads both request details")]
async fn global_service_reads_both_tenants(world: &mut TokenCenterWorld) {
    let keys = world
        .client
        .get(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth(&world.matrix_global_service_token)
        .send()
        .await
        .expect("global key list");
    assert_eq!(keys.status(), StatusCode::OK);
    let keys: Value = keys.json().await.expect("global key list JSON");
    assert_eq!(keys.as_array().map(Vec::len), Some(2));
    assert!(keys.as_array().is_some_and(|rows| {
        ["matrix-first", "matrix-second"]
            .iter()
            .all(|tenant| rows.iter().any(|key| key["tenant_external_id"] == **tenant))
    }));

    for request_id in [
        world.matrix_first_request_id.expect("first request id"),
        world.matrix_second_request_id.expect("second request id"),
    ] {
        let detail = world
            .client
            .get(format!(
                "{}/internal/v1/requests/{request_id}",
                world.service_url
            ))
            .bearer_auth(&world.matrix_global_service_token)
            .send()
            .await
            .expect("global request detail");
        assert_eq!(detail.status(), StatusCode::OK);
        let detail: Value = detail.json().await.expect("global request detail JSON");
        assert_eq!(detail["request_id"], request_id.to_string());
        assert_eq!(
            detail["request_body"]["messages"][0]["content"],
            "matrix probe"
        );
    }
}

#[then("the tenant scoped service credential lists and reads only its own tenant")]
async fn scoped_service_reads_own_tenant(world: &mut TokenCenterWorld) {
    let keys = world
        .client
        .get(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth(&world.matrix_scoped_service_token)
        .send()
        .await
        .expect("scoped key list");
    assert_eq!(keys.status(), StatusCode::OK);
    let keys: Value = keys.json().await.expect("scoped key list JSON");
    assert_eq!(keys.as_array().map(Vec::len), Some(1));
    assert_eq!(keys[0]["tenant_external_id"], "matrix-first");

    let requests = world
        .client
        .get(format!(
            "{}/internal/v1/requests?limit=10",
            world.service_url
        ))
        .bearer_auth(&world.matrix_scoped_service_token)
        .send()
        .await
        .expect("scoped request list");
    assert_eq!(requests.status(), StatusCode::OK);
    let requests: Value = requests.json().await.expect("scoped request list JSON");
    assert_eq!(requests.as_array().map(Vec::len), Some(1));
    assert_eq!(
        requests[0]["request_id"],
        world
            .matrix_first_request_id
            .expect("first request id")
            .to_string()
    );

    let detail = world
        .client
        .get(format!(
            "{}/internal/v1/requests/{}",
            world.service_url,
            world.matrix_first_request_id.expect("first request id")
        ))
        .bearer_auth(&world.matrix_scoped_service_token)
        .send()
        .await
        .expect("scoped own request detail");
    assert_eq!(detail.status(), StatusCode::OK);
}

#[then(
    "the tenant scoped service credential cannot read another tenant or synchronize global prices"
)]
async fn scoped_service_cannot_cross_tenants_or_sync_prices(world: &mut TokenCenterWorld) {
    for path in [
        "/internal/v1/keys?tenant_external_id=matrix-second".to_owned(),
        "/internal/v1/requests?tenant_external_id=matrix-second".to_owned(),
        "/internal/v1/stats?tenant_external_id=matrix-second".to_owned(),
    ] {
        let response = world
            .client
            .get(format!("{}{path}", world.service_url))
            .bearer_auth(&world.matrix_scoped_service_token)
            .send()
            .await
            .expect("reject cross-tenant service query");
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    let other_detail = world
        .client
        .get(format!(
            "{}/internal/v1/requests/{}",
            world.service_url,
            world.matrix_second_request_id.expect("second request id")
        ))
        .bearer_auth(&world.matrix_scoped_service_token)
        .send()
        .await
        .expect("reject scoped access to another tenant detail");
    assert_eq!(other_detail.status(), StatusCode::NOT_FOUND);

    let sync = world
        .client
        .post(format!(
            "{}/internal/v1/model-prices/sync",
            world.service_url
        ))
        .bearer_auth(&world.matrix_scoped_service_token)
        .json(&json!({
            "models": ["matrix-model"],
            "currency": "USD",
            "tenant_external_id": "matrix-first"
        }))
        .send()
        .await
        .expect("reject scoped global price synchronization");
    assert_eq!(sync.status(), StatusCode::FORBIDDEN);
}

#[then(
    "the downstream credential cannot administer the service or read another credential history"
)]
async fn downstream_cannot_cross_authority_boundaries(world: &mut TokenCenterWorld) {
    let management = world
        .client
        .get(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth(&world.matrix_first_key)
        .send()
        .await
        .expect("reject downstream management access");
    assert_eq!(management.status(), StatusCode::UNAUTHORIZED);

    let other_detail = world
        .client
        .get(format!(
            "{}/self/v1/requests/{}",
            world.service_url,
            world.matrix_second_request_id.expect("second request id")
        ))
        .bearer_auth(&world.matrix_first_key)
        .send()
        .await
        .expect("reject downstream cross-key request detail");
    assert_eq!(other_detail.status(), StatusCode::NOT_FOUND);

    let own_requests = world
        .client
        .get(format!("{}/self/v1/requests", world.service_url))
        .bearer_auth(&world.matrix_first_key)
        .send()
        .await
        .expect("read own downstream request list");
    assert_eq!(own_requests.status(), StatusCode::OK);
    let own_requests: Value = own_requests.json().await.expect("own request list JSON");
    assert_eq!(own_requests.as_array().map(Vec::len), Some(1));
    assert_eq!(
        own_requests[0]["request_id"],
        world
            .matrix_first_request_id
            .expect("first request id")
            .to_string()
    );
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
        .header(
            "Idempotency-Key",
            format!("cucumber-oauth-rotate-{account_id}"),
        )
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
        .header(
            "Idempotency-Key",
            format!("cucumber-oauth-refresh-{account_id}"),
        )
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

#[when("the service creates and uses a credential with an explicit policy and budget")]
async fn create_and_use_policy_credential(world: &mut TokenCenterWorld) {
    let price = world
        .client
        .post(format!(
            "{}/internal/v1/prices/USD/gpt-test",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"input_per_million": "1", "output_per_million": "1"}))
        .send()
        .await
        .expect("create continuity-test model price");
    assert_eq!(price.status(), StatusCode::OK);

    world.expected_policy = json!({
        "allowed_models": ["gpt-test"],
        "requests_per_minute": 7,
        "tokens_per_minute": 7000,
        "max_concurrency": 2,
        "daily_budget": "5",
        "weekly_budget": "20",
        "lifetime_budget": "50"
    });
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "tenant_external_id": "continuity-tenant",
            "principal_external_id": "continuity-user",
            "alias": "continuity-credential",
            "currency": "USD",
            "initial_balance": "10",
            "policy": world.expected_policy
        }))
        .send()
        .await
        .expect("create continuity-test credential");
    assert_eq!(response.status(), StatusCode::CREATED);
    let issued: Value = response.json().await.expect("continuity credential JSON");
    world.current_key = issued["key"]
        .as_str()
        .expect("continuity credential")
        .to_owned();
    world.stable_key_id = Some(
        Uuid::parse_str(issued["key_id"].as_str().expect("continuity key id"))
            .expect("continuity key UUID"),
    );
    world.stable_account_id = Some(
        Uuid::parse_str(
            issued["account_id"]
                .as_str()
                .expect("continuity account id"),
        )
        .expect("continuity account UUID"),
    );

    call_model(world, "gpt-test".to_owned()).await;
    assert_eq!(world.status, Some(StatusCode::OK));
    let key_view = world
        .client
        .get(format!("{}/self/v1/key", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("read continuity key before rotation");
    assert_eq!(key_view.status(), StatusCode::OK);
    let key_view: Value = key_view.json().await.expect("continuity key view JSON");
    assert_eq!(key_view["policy"], world.expected_policy);
    world.expected_balance = key_view["available_balance"]
        .as_str()
        .expect("continuity available balance")
        .to_owned();
}

#[when("the service suspends and reactivates that credential")]
async fn suspend_and_reactivate_credential(world: &mut TokenCenterWorld) {
    let key_id = world.stable_key_id.expect("continuity key id");
    let suspended = world
        .client
        .patch(format!(
            "{}/internal/v1/keys/{key_id}/status",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"status": "suspended"}))
        .send()
        .await
        .expect("suspend continuity credential");
    assert_eq!(suspended.status(), StatusCode::OK);
    let suspended: Value = suspended.json().await.expect("suspended status JSON");
    assert_eq!(suspended["key_id"], key_id.to_string());
    assert_eq!(suspended["status"], "suspended");

    let rejected = world
        .client
        .get(format!("{}/self/v1/key", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("suspended credential authentication check");
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

    let active = world
        .client
        .patch(format!(
            "{}/internal/v1/keys/{key_id}/status",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"status": "active"}))
        .send()
        .await
        .expect("reactivate continuity credential");
    assert_eq!(active.status(), StatusCode::OK);
    let active: Value = active.json().await.expect("active status JSON");
    assert_eq!(active["status"], "active");

    let restored = world
        .client
        .get(format!("{}/self/v1/key", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("reactivated credential authentication check");
    assert_eq!(restored.status(), StatusCode::OK);
}

#[then("the rotated credential retains stable identity policy balance and history")]
async fn rotated_credential_retains_all_state(world: &mut TokenCenterWorld) {
    let key_id = world.stable_key_id.expect("continuity key id");
    assert_eq!(world.response["key_id"], key_id.to_string());
    assert_eq!(
        world.response["account_id"],
        world.stable_account_id.expect("account id").to_string()
    );
    assert_eq!(world.response["credential_generation"], 2);

    let key_view = world
        .client
        .get(format!("{}/self/v1/key", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("read rotated continuity credential");
    assert_eq!(key_view.status(), StatusCode::OK);
    let key_view: Value = key_view.json().await.expect("rotated key view JSON");
    assert_eq!(key_view["key_id"], key_id.to_string());
    assert_eq!(key_view["alias"], "continuity-credential");
    assert_eq!(key_view["credential_generation"], 2);
    assert_eq!(key_view["policy"], world.expected_policy);
    assert_eq!(key_view["available_balance"], world.expected_balance);

    let managed = world
        .client
        .get(format!(
            "{}/internal/v1/keys?tenant_external_id=continuity-tenant",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("list rotated continuity credential");
    assert_eq!(managed.status(), StatusCode::OK);
    let managed: Value = managed.json().await.expect("managed key list JSON");
    assert_eq!(managed.as_array().map(Vec::len), Some(1));
    assert_eq!(managed[0]["key_id"], key_id.to_string());
    assert_eq!(
        managed[0]["account_id"],
        world.stable_account_id.expect("account id").to_string()
    );
    assert_eq!(managed[0]["status"], "active");
    assert_eq!(managed[0]["credential_generation"], 2);
    assert_eq!(managed[0]["policy"], world.expected_policy);
    assert_eq!(managed[0]["available_balance"], world.expected_balance);

    let stats = world
        .client
        .get(format!("{}/self/v1/stats", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("rotated continuity statistics");
    assert_eq!(stats.status(), StatusCode::OK);
    let stats: Value = stats.json().await.expect("rotated continuity stats JSON");
    assert_eq!(stats["summary"]["total_requests"], 1);
    assert_eq!(stats["summary"]["input_tokens"], 7);
    assert_eq!(stats["summary"]["output_tokens"], 3);

    let requests = world
        .client
        .get(format!("{}/self/v1/requests", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("rotated continuity history");
    assert_eq!(requests.status(), StatusCode::OK);
    let requests: Value = requests.json().await.expect("continuity history JSON");
    assert_eq!(requests.as_array().map(Vec::len), Some(1));
    let request_id = requests[0]["request_id"]
        .as_str()
        .expect("continuity request id");
    let detail = world
        .client
        .get(format!(
            "{}/self/v1/requests/{request_id}",
            world.service_url
        ))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("rotated continuity request detail");
    assert_eq!(detail.status(), StatusCode::OK);
    let detail: Value = detail.json().await.expect("continuity detail JSON");
    assert_eq!(detail["archive_complete"], true);
    assert_eq!(detail["request_body"]["messages"][0]["content"], "hi");
    assert_eq!(
        detail["response_body"]["choices"][0]["message"]["content"],
        "hello"
    );
}

#[when("the service attaches an unchanged legacy CPA key")]
async fn attach_legacy_cpa_key(world: &mut TokenCenterWorld) {
    let legacy = "sk-cpa-linux-codex-unchanged-credential-1234567890";
    let source_hash = format!("{:x}", Sha256::digest(legacy.as_bytes()));
    let key_id = world.stable_key_id.expect("stable key id");
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/keys/{key_id}/legacy-credentials",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"credential": legacy, "source_hash": source_hash}))
        .send()
        .await
        .expect("register legacy CPA credential");
    assert_eq!(response.status(), StatusCode::CREATED);
    world.current_key = legacy.to_owned();
}

#[when("the client views statistics with the legacy CPA key")]
async fn view_stats_with_legacy_cpa_key(world: &mut TokenCenterWorld) {
    view_stats(world).await;
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
    call_model(world, model.clone()).await;
    // A fixed minute window may roll over between the first two calls. A third immediate
    // request is guaranteed to share the second request's window and must be rejected.
    if world.status == Some(StatusCode::OK) {
        call_model(world, model).await;
    }
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

    let stats = world
        .client
        .get(format!("{}/internal/v1/stats", world.service_url))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("operator aggregate statistics");
    assert_eq!(stats.status(), StatusCode::OK);
    let stats: Value = stats.json().await.expect("operator statistics JSON");
    assert_eq!(stats["summary"]["total_requests"], 1);
    assert_eq!(stats["summary"]["successful_requests"], 1);

    let requests = world
        .client
        .get(format!(
            "{}/internal/v1/requests?limit=5",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("operator requests")
        .json::<Value>()
        .await
        .expect("operator requests JSON");
    let request_id = requests[0]["request_id"].as_str().expect("request id");
    let detail = world
        .client
        .get(format!(
            "{}/internal/v1/requests/{request_id}",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("operator request detail");
    assert_eq!(detail.status(), StatusCode::OK);
    let detail: Value = detail.json().await.expect("operator request detail JSON");
    assert_eq!(detail["archive_complete"], true);
    assert_eq!(detail["request_body"]["messages"][0]["content"], "hi");
    assert_eq!(
        detail["response_body"]["choices"][0]["message"]["content"],
        "hello"
    );
}

#[when("the bootstrap service creates a tenant scoped service token")]
async fn create_scoped_service_token(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/internal/v1/service-tokens", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": "memeloop-web-test",
            "scopes": ["keys:write", "prices:write"],
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

    let response = world
        .client
        .post(format!(
            "{}/internal/v1/model-prices/sync",
            world.service_url
        ))
        .bearer_auth(&world.current_service_token)
        .json(&json!({
            "tenant_external_id": "scoped-tenant",
            "models": ["global-model"],
            "currency": "USD"
        }))
        .send()
        .await
        .expect("reject global model price synchronization");
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
        .header("idempotency-key", "cucumber:rotate-scoped-service-token")
        .send()
        .await
        .expect("rotate scoped service token");
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response.json().await.expect("rotated service token JSON");
    assert_eq!(value["service_id"], service_id.to_string());
    assert_eq!(value["credential_generation"], 2);
    let replay = world
        .client
        .post(format!(
            "{}/internal/v1/service-tokens/{service_id}/rotate",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .header("idempotency-key", "cucumber:rotate-scoped-service-token")
        .send()
        .await
        .expect("replay scoped service token rotation");
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(
        replay.json::<Value>().await.expect("rotation replay JSON"),
        value
    );
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
        .header("idempotency-key", "cucumber:rotate-key")
        .send()
        .await
        .expect("rotate key request");
    world.status = Some(response.status());
    world.response = response.json().await.expect("rotate key JSON");
    let replay = world
        .client
        .post(format!(
            "{}/internal/v1/keys/{key_id}/rotate",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .header("idempotency-key", "cucumber:rotate-key")
        .send()
        .await
        .expect("replay key rotation request");
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(
        replay
            .json::<Value>()
            .await
            .expect("key rotation replay JSON"),
        world.response
    );
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

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cpamp")
        .join(name)
}

fn apply_sqlite_fixture(database: &Path, fixture: &Path) {
    let input = File::open(fixture).expect("open CPAMP fixture SQL");
    let output = Command::new("sqlite3")
        .arg(database)
        .stdin(Stdio::from(input))
        .output()
        .expect("execute sqlite3 CPAMP fixture");
    assert!(
        output.status.success(),
        "sqlite3 fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn postgres_command_environment(database_url: &str) -> Vec<(String, String)> {
    let url =
        url::Url::parse(database_url).expect("MTC_TEST_POSTGRES_URL must be a PostgreSQL URL");
    assert!(
        matches!(url.scheme(), "postgres" | "postgresql"),
        "MTC_TEST_POSTGRES_URL must use postgres://"
    );
    vec![
        (
            "PGHOST".to_owned(),
            url.host_str().expect("PostgreSQL URL host").to_owned(),
        ),
        (
            "PGPORT".to_owned(),
            url.port_or_known_default().unwrap_or(5432).to_string(),
        ),
        ("PGUSER".to_owned(), url.username().to_owned()),
        (
            "PGPASSWORD".to_owned(),
            url.password().unwrap_or_default().to_owned(),
        ),
        (
            "PGDATABASE".to_owned(),
            url.path().trim_start_matches('/').to_owned(),
        ),
    ]
}

fn run_cpamp_importer(world: &TokenCenterWorld) {
    let sqlite_path = world
        .import_sqlite_path
        .as_ref()
        .expect("CPAMP SQLite fixture path");
    let mut command = Command::new("sh");
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("ops/migrate-cpamp.sh"))
        .envs(postgres_command_environment(&world.import_database_url))
        .env("CPAMP_SQLITE_PATH", sqlite_path)
        .env("IMPORT_TENANT_EXTERNAL_ID", &world.import_tenant)
        .env("CPAMP_IMPORT_SOURCE", &world.import_source)
        .env("CPAMP_OVERLAP_MS", "86400000")
        .env("CPAMP_RESET_IMPORT", "false");
    let output = command.output().expect("execute CPAMP importer");
    assert!(
        output.status.success(),
        "CPAMP importer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[given("a migrated PostgreSQL schema and a CPAMP SQLite fixture")]
async fn prepare_cpamp_import_fixture(world: &mut TokenCenterWorld) {
    for executable in ["psql", "sqlite3"] {
        assert!(
            Command::new(executable)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success()),
            "{executable} is required when MTC_TEST_POSTGRES_URL enables @postgres Cucumber scenarios"
        );
    }
    let database_url = std::env::var("MTC_TEST_POSTGRES_URL")
        .expect("MTC_TEST_POSTGRES_URL enables the @postgres Cucumber scenario");
    let database = memeloop_token_center::db::Database::connect(&database_url)
        .await
        .expect("connect CPAMP importer acceptance database");
    database
        .migrate()
        .await
        .expect("migrate CPAMP importer acceptance database");
    database
        .maintain_partitions()
        .await
        .expect("maintain CPAMP importer acceptance partitions");

    let unique = Uuid::now_v7().simple().to_string();
    let temp_dir = tempfile::tempdir().expect("CPAMP importer temporary directory");
    let sqlite_path = temp_dir.path().join("usage.sqlite");
    apply_sqlite_fixture(&sqlite_path, &fixture_path("initial.sql"));
    world.import_database_url = database_url;
    world.import_tenant = format!("cpamp-acceptance-{unique}");
    world.import_source = format!("cpamp-acceptance:{unique}");
    world.import_sqlite_path = Some(sqlite_path);
    world.temp_dir = Some(temp_dir);
}

#[when("the CPAMP importer runs twice over the initial fixture")]
async fn import_initial_cpamp_fixture_twice(world: &mut TokenCenterWorld) {
    run_cpamp_importer(world);
    run_cpamp_importer(world);
}

async fn assert_cpamp_import_state(
    world: &TokenCenterWorld,
    expected_requests: i64,
    expected_input_tokens: i64,
    expected_output_tokens: i64,
    expected_cost_micros: i64,
    expected_watermark: i64,
    expected_watermark_hash: &str,
) {
    let pool = PgPool::connect(&world.import_database_url)
        .await
        .expect("connect for CPAMP import assertions");
    let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
        .bind(&world.import_tenant)
        .fetch_one(&pool)
        .await
        .expect("imported tenant")
        .get("id");
    let request_row = sqlx::query(
        "SELECT COUNT(*) AS requests, COUNT(DISTINCT reservation_id) AS distinct_events, COALESCE(SUM(input_tokens), 0) AS input_tokens, COALESCE(SUM(output_tokens), 0) AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM request_records WHERE tenant_id = $1",
    )
    .bind(&tenant_id)
    .fetch_one(&pool)
    .await
    .expect("imported request totals");
    assert_eq!(request_row.get::<i64, _>("requests"), expected_requests);
    assert_eq!(
        request_row.get::<i64, _>("distinct_events"),
        expected_requests,
        "every imported CPAMP event must produce one stable request"
    );
    assert_eq!(
        request_row.get::<i64, _>("input_tokens"),
        expected_input_tokens
    );
    assert_eq!(
        request_row.get::<i64, _>("output_tokens"),
        expected_output_tokens
    );
    assert_eq!(
        request_row.get::<i64, _>("cost_micros"),
        expected_cost_micros
    );

    let aggregate_row = sqlx::query(
        "SELECT COALESCE(SUM(a.requests), 0) AS requests, COALESCE(SUM(a.input_tokens), 0) AS input_tokens, COALESCE(SUM(a.output_tokens), 0) AS output_tokens, COALESCE(SUM(a.cost_micros), 0) AS cost_micros FROM usage_daily_aggregates a JOIN key_records k ON k.id = a.key_id WHERE k.tenant_id = $1",
    )
    .bind(&tenant_id)
    .fetch_one(&pool)
    .await
    .expect("imported aggregate totals");
    assert_eq!(aggregate_row.get::<i64, _>("requests"), expected_requests);
    assert_eq!(
        aggregate_row.get::<i64, _>("input_tokens"),
        expected_input_tokens
    );
    assert_eq!(
        aggregate_row.get::<i64, _>("output_tokens"),
        expected_output_tokens
    );
    assert_eq!(
        aggregate_row.get::<i64, _>("cost_micros"),
        expected_cost_micros
    );

    let checkpoint = sqlx::query(
        "SELECT watermark_ms, watermark_hash, imported_events FROM cpamp_import_checkpoints WHERE source = $1",
    )
    .bind(&world.import_source)
    .fetch_one(&pool)
    .await
    .expect("CPAMP import checkpoint");
    assert_eq!(checkpoint.get::<i64, _>("watermark_ms"), expected_watermark);
    assert_eq!(
        checkpoint.get::<String, _>("watermark_hash"),
        expected_watermark_hash
    );
    assert_eq!(
        checkpoint.get::<i64, _>("imported_events"),
        expected_requests,
        "repeated imports must not increment the logical imported-event count"
    );

    let identity = sqlx::query(
        "SELECT k.alias, k.credential_generation, k.policy_json FROM key_records k WHERE k.tenant_id = $1",
    )
    .bind(&tenant_id)
    .fetch_one(&pool)
    .await
    .expect("imported stable key identity");
    assert_eq!(identity.get::<String, _>("alias"), "Fixture Linux Codex");
    assert_eq!(identity.get::<i64, _>("credential_generation"), 0);
    assert!(
        identity
            .get::<String, _>("policy_json")
            .contains("\"allowed_models\":[\"*\"]")
    );
    let price = sqlx::query(
        "SELECT input_micros_per_million, output_micros_per_million, source FROM model_prices WHERE model = 'fixture-model' AND currency = 'USD'",
    )
    .fetch_one(&pool)
    .await
    .expect("imported fixture price");
    assert_eq!(price.get::<i64, _>("input_micros_per_million"), 2_000_000);
    assert_eq!(price.get::<i64, _>("output_micros_per_million"), 4_000_000);
    assert_eq!(price.get::<String, _>("source"), "cpamp:fixture");
    pool.close().await;
}

#[then("the imported requests aggregates and checkpoint contain exactly the initial events")]
async fn initial_cpamp_import_is_exact(world: &mut TokenCenterWorld) {
    assert_cpamp_import_state(world, 2, 28, 8, 88, 300_000_000, "fixture-event-initial-b").await;
}

#[when("a late overlap event and a newer event are appended and the importer runs twice")]
async fn append_and_import_late_cpamp_events(world: &mut TokenCenterWorld) {
    apply_sqlite_fixture(
        world
            .import_sqlite_path
            .as_deref()
            .expect("CPAMP SQLite fixture path"),
        &fixture_path("late.sql"),
    );
    run_cpamp_importer(world);
    run_cpamp_importer(world);
}

#[then("the imported requests aggregates and checkpoint contain every event exactly once")]
async fn late_cpamp_import_is_exact(world: &mut TokenCenterWorld) {
    assert_cpamp_import_state(
        world,
        4,
        70,
        26,
        244,
        400_000_000,
        "fixture-event-new-watermark",
    )
    .await;
}

#[given("the mock buffered Responses upstream returns parent and child responses")]
async fn mock_buffered_responses_parent_and_child(world: &mut TokenCenterWorld) {
    let parent: Value = serde_json::from_str(include_str!(
        "fixtures/protocols/responses-buffered-parent.json"
    ))
    .expect("buffered Responses fixture JSON");
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(json!({"input": "buffered parent"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(parent))
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(json!({
            "input": "buffered child",
            "previous_response_id": "resp-buffered-parent"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp-buffered-child",
            "object": "response",
            "output": [],
            "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
        })))
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[given("the mock streaming Responses upstream returns parent and child events")]
async fn mock_streaming_responses_parent_and_child(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(
            json!({"input": "streaming parent", "stream": true}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(include_str!(
                    "fixtures/protocols/responses-streaming-parent.sse"
                )),
        )
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(json!({
            "input": "streaming child",
            "previous_response_id": "resp-streaming-parent",
            "stream": true
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(concat!(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-streaming-child\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-streaming-child\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
                    "data: [DONE]\n\n"
                )),
        )
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

async fn send_responses_turn(
    world: &TokenCenterWorld,
    model: &str,
    input: &str,
    previous_response_id: Option<&str>,
    stream: bool,
) -> StatusCode {
    let mut body = json!({"model": model, "input": input, "stream": stream});
    if let Some(previous_response_id) = previous_response_id {
        body["previous_response_id"] = Value::String(previous_response_id.to_owned());
    }
    let response = world
        .client
        .post(format!("{}/v1/responses", world.service_url))
        .bearer_auth(&world.current_key)
        .json(&body)
        .send()
        .await
        .expect("Responses request");
    let status = response.status();
    let _ = response.bytes().await.expect("Responses body");
    status
}

#[when(expr = "the Responses client sends a buffered parent and child for model {string}")]
async fn send_buffered_responses_parent_and_child(world: &mut TokenCenterWorld, model: String) {
    assert_eq!(
        send_responses_turn(world, &model, "buffered parent", None, false).await,
        StatusCode::OK
    );
    world.status = Some(
        send_responses_turn(
            world,
            &model,
            "buffered child",
            Some("resp-buffered-parent"),
            false,
        )
        .await,
    );
}

#[when(expr = "the Responses client sends a streaming parent and child for model {string}")]
async fn send_streaming_responses_parent_and_child(world: &mut TokenCenterWorld, model: String) {
    assert_eq!(
        send_responses_turn(world, &model, "streaming parent", None, true).await,
        StatusCode::OK
    );
    world.status = Some(
        send_responses_turn(
            world,
            &model,
            "streaming child",
            Some("resp-streaming-parent"),
            true,
        )
        .await,
    );
}

async fn own_conversation_detail(world: &TokenCenterWorld) -> Value {
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
    assert_eq!(clusters.as_array().map(Vec::len), Some(1), "{clusters}");
    let cluster_id = clusters[0]["cluster_id"]
        .as_str()
        .expect("conversation cluster id");
    world
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
        .expect("conversation detail JSON")
}

#[then("the two Responses requests have a direct continuation edge")]
async fn responses_requests_have_direct_parent_edge(world: &mut TokenCenterWorld) {
    let detail = own_conversation_detail(world).await;
    assert_eq!(detail["cluster"]["request_count"], 2, "{detail}");
    assert_eq!(
        detail["edges"].as_array().map(Vec::len),
        Some(1),
        "{detail}"
    );
    let edge = &detail["edges"][0];
    assert_eq!(edge["relation"], "continues", "{detail}");
    assert_eq!(edge["evidence"]["explicit_parent"], true, "{detail}");
    assert_eq!(edge["from_request_id"], detail["requests"][0]["request_id"]);
    assert_eq!(edge["to_request_id"], detail["requests"][1]["request_id"]);
}

#[when(
    expr = "the client sends two consecutive compactions followed by a branch for model {string}"
)]
async fn send_compaction_chain_and_branch(world: &mut TokenCenterWorld, model: String) {
    let turns = [
        ("root", None, "main", false, "full initial context"),
        ("compact-a", Some("root"), "main", true, "summary one"),
        ("compact-b", Some("compact-a"), "main", true, "summary two"),
        (
            "branch-c",
            Some("compact-b"),
            "alternative",
            false,
            "branched request",
        ),
    ];
    for (turn, parent, branch, compaction, content) in turns {
        let mut request = world
            .client
            .post(format!("{}/v1/chat/completions", world.service_url))
            .bearer_auth(&world.current_key)
            .header("x-mtc-conversation-id", "compaction-session")
            .header("x-mtc-turn-id", turn)
            .header("x-mtc-branch-id", branch);
        if let Some(parent) = parent {
            request = request.header("x-mtc-parent-turn-id", parent);
        }
        if compaction {
            request = request.header("x-mtc-compaction", "true");
        }
        let response = request
            .json(&json!({
                "model": model,
                "messages": [{"role": "user", "content": content}]
            }))
            .send()
            .await
            .expect("compaction-chain request");
        world.status = Some(response.status());
        let _ = response.bytes().await.expect("compaction-chain response");
        assert_eq!(world.status, Some(StatusCode::OK));
    }
}

#[then("the conversation contains two compaction edges followed by a branch edge")]
async fn conversation_has_compactions_then_branch(world: &mut TokenCenterWorld) {
    let detail = own_conversation_detail(world).await;
    assert_eq!(detail["cluster"]["request_count"], 4, "{detail}");
    let edges = detail["edges"].as_array().expect("conversation edges");
    assert_eq!(edges.len(), 3, "{detail}");
    for (request_index, expected_relation) in [(1, "compacts"), (2, "compacts"), (3, "branch")] {
        let request_id = &detail["requests"][request_index]["request_id"];
        let edge = edges
            .iter()
            .find(|edge| edge["to_request_id"] == *request_id)
            .expect("edge for each child request");
        assert_eq!(edge["relation"], expected_relation, "{detail}");
        assert_eq!(edge["evidence"]["explicit_parent"], true, "{detail}");
    }
}

#[given("the mock Anthropic upstream requires metadata and a beta header")]
async fn mock_anthropic_metadata_and_beta(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("anthropic-beta", "prompt-caching-2024-07-31"))
        .and(body_partial_json(json!({
            "model": "claude-test",
            "metadata": {"session_id": "anthropic-metadata-session"}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg-metadata",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "metadata accepted"}],
            "usage": {"input_tokens": 5, "output_tokens": 2}
        })))
        .expect(2)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[when(expr = "the Claude client sends metadata-linked turns for model {string}")]
async fn claude_sends_metadata_linked_turns(world: &mut TokenCenterWorld, model: String) {
    let turns = [
        json!({
            "session_id": "anthropic-metadata-session",
            "turn_id": "anthropic-root",
            "branch_id": "main"
        }),
        json!({
            "session_id": "anthropic-metadata-session",
            "turn_id": "anthropic-compact",
            "parent_turn_id": "anthropic-root",
            "branch_id": "main",
            "compaction": true
        }),
    ];
    for metadata in turns {
        let response = world
            .client
            .post(format!("{}/v1/messages", world.service_url))
            .bearer_auth(&world.current_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "prompt-caching-2024-07-31")
            .header("user-agent", "claude-code/2.0")
            .json(&json!({
                "model": model,
                "max_tokens": 128,
                "metadata": metadata,
                "messages": [{"role": "user", "content": "summarize context"}]
            }))
            .send()
            .await
            .expect("Anthropic metadata request");
        world.status = Some(response.status());
        let _ = response.bytes().await.expect("Anthropic response body");
        assert_eq!(world.status, Some(StatusCode::OK));
    }
}

#[then("the Anthropic turns have a direct compaction edge")]
async fn anthropic_turns_have_compaction_edge(world: &mut TokenCenterWorld) {
    let detail = own_conversation_detail(world).await;
    assert_eq!(detail["cluster"]["request_count"], 2, "{detail}");
    assert_eq!(detail["edges"][0]["relation"], "compacts", "{detail}");
    assert_eq!(
        detail["edges"][0]["evidence"]["explicit_parent"], true,
        "{detail}"
    );
}

#[given("the mock utility and OpenAI upstreams return successful responses")]
async fn mock_utility_protocols(world: &mut TokenCenterWorld) {
    mock_successful_openai(world).await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"object": "embedding", "embedding": [0.1], "index": 0}],
            "usage": {"prompt_tokens": 3, "total_tokens": 3}
        })))
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 5})))
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[when(expr = "the client sends chat, embedding, and token counting requests for model {string}")]
async fn send_chat_and_utility_requests(world: &mut TokenCenterWorld, model: String) {
    let requests = [
        (
            "/v1/chat/completions",
            json!({"model": model, "messages": [{"role": "user", "content": "chat"}]}),
        ),
        (
            "/v1/embeddings",
            json!({"model": model, "input": "embedding input"}),
        ),
        (
            "/v1/messages/count_tokens",
            json!({"model": model, "messages": [{"role": "user", "content": "count"}]}),
        ),
    ];
    for (path, body) in requests {
        let response = world
            .client
            .post(format!("{}{path}", world.service_url))
            .bearer_auth(&world.current_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .expect("chat or utility request");
        world.status = Some(response.status());
        let _ = response.bytes().await.expect("chat or utility response");
        assert_eq!(world.status, Some(StatusCode::OK));
    }
}

#[then("only the chat request appears in logical conversations")]
async fn utility_requests_do_not_pollute_conversations(world: &mut TokenCenterWorld) {
    let detail = own_conversation_detail(world).await;
    assert_eq!(detail["cluster"]["request_count"], 1, "{detail}");
    assert_eq!(detail["requests"].as_array().map(Vec::len), Some(1));
    assert_eq!(detail["requests"][0]["protocol"], "openai");
    assert_eq!(detail["edges"].as_array().map(Vec::len), Some(0));
}

#[given("the mock WorkBuddy OpenAI upstream requires max_completion_tokens")]
async fn mock_workbuddy_openai(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({
            "model": "gpt-test",
            "max_completion_tokens": 64
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-workbuddy",
            "choices": [{"message": {"role": "assistant", "content": "work complete"}}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
        })))
        .expect(2)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[when(expr = "WorkBuddy sends two OpenAI chat turns for model {string}")]
async fn workbuddy_sends_openai_turns(world: &mut TokenCenterWorld, model: String) {
    let messages = [
        json!([{"role": "user", "content": "inspect repository"}]),
        json!([
            {"role": "user", "content": "inspect repository"},
            {"role": "assistant", "content": "inspection complete"},
            {"role": "user", "content": "apply the fix"}
        ]),
    ];
    for messages in messages {
        let response = world
            .client
            .post(format!("{}/v1/chat/completions", world.service_url))
            .header("x-api-key", &world.current_key)
            .header("user-agent", "WorkBuddy/1.0")
            .header("x-session-id", "workbuddy-session")
            .json(&json!({
                "model": model,
                "max_completion_tokens": 64,
                "messages": messages
            }))
            .send()
            .await
            .expect("WorkBuddy OpenAI request");
        world.status = Some(response.status());
        let _ = response.bytes().await.expect("WorkBuddy OpenAI response");
        assert_eq!(world.status, Some(StatusCode::OK));
    }
}

#[then("the WorkBuddy requests form one logical conversation")]
async fn workbuddy_requests_share_conversation(world: &mut TokenCenterWorld) {
    let detail = own_conversation_detail(world).await;
    assert_eq!(detail["cluster"]["request_count"], 2, "{detail}");
    assert_eq!(detail["edges"][0]["relation"], "continues", "{detail}");
    assert!(
        detail["edges"][0]["confidence"]
            .as_f64()
            .is_some_and(|confidence| confidence >= 0.95)
    );
    assert_eq!(detail["edges"][0]["evidence"]["explicit_session"], true);
}

#[tokio::main]
async fn main() {
    let postgres_enabled = std::env::var_os("MTC_TEST_POSTGRES_URL").is_some();
    TokenCenterWorld::cucumber()
        // Every scenario boots an isolated application, database and plugin runtime. Cucumber's
        // default of 64 concurrent scenarios can turn the acceptance harness itself into a
        // multi-gigabyte workload and make CI results depend on host memory pressure.
        .max_concurrent_scenarios(2)
        .filter_run_and_exit("tests/features", move |_, _, scenario| {
            postgres_enabled || !scenario.tags.iter().any(|tag| tag == "postgres")
        })
        .await;
}
