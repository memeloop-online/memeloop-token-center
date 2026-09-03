use std::{
    fmt,
    fs::File,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
};

use cucumber::{World, given, then, when};
use futures_util::StreamExt;
use memeloop_token_center::{
    AppState, api,
    archive_staging::{
        ArchiveStagingIntentDigest, ArchiveStagingKey, ArchiveStagingLeaseOwner,
        ArchiveStagingOwner, ArchiveStagingPurpose, ArchiveStagingState, BeginArchiveStagingInput,
        BeginArchiveStagingResult,
    },
    config::Config,
    crypto,
    model::{ArchivedGenerationAsset, GenerationStagedAssets},
    worker,
};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use sqlx::{AnyPool, PgPool, Row};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, header, header_exists, method, path, query_param},
};

#[path = "steps/cloud_entitlements.rs"]
mod cloud_entitlements;
#[path = "steps/logical_sessions.rs"]
mod logical_sessions;
#[path = "steps/security_acceptance.rs"]
mod security_acceptance;

#[derive(World)]
struct TokenCenterWorld {
    client: Client,
    service_url: String,
    state: Option<AppState>,
    mock: Option<MockServer>,
    asset_mock: Option<MockServer>,
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
    synchronous_request_id: Option<Uuid>,
    image_route_id: Option<Uuid>,
    image_route_updated_at: i64,
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
    response_retry_after: Option<String>,
    response: Value,
    synchronous_response_body: Vec<u8>,
    synchronous_response_content_length: Option<usize>,
}

impl Default for TokenCenterWorld {
    fn default() -> Self {
        Self {
            client: Client::new(),
            service_url: String::new(),
            state: None,
            mock: None,
            asset_mock: None,
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
            synchronous_request_id: None,
            image_route_id: None,
            image_route_updated_at: 0,
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
            response_retry_after: None,
            response: Value::Null,
            synchronous_response_body: Vec::new(),
            synchronous_response_content_length: None,
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
    let test_state = state.clone();
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
    world.state = Some(test_state);
    world.mock = Some(mock);
    world.temp_dir = Some(temp_dir);
    world.server_task = Some(server_task);
    world.worker_task = Some(worker_task);
}

#[given("the mock SiliconFlow upstream completes a text to video request")]
async fn mock_siliconflow_video_generation(world: &mut TokenCenterWorld) {
    let asset_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/sf-result.mp4"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "video/mp4")
                .set_body_bytes(b"siliconflow-video-content"),
        )
        .expect(1)
        .mount(&asset_server)
        .await;
    let asset_url = asset_server.uri();
    let server = world.mock.as_ref().expect("mock server");
    Mock::given(method("POST"))
        .and(path("/v1/video/submit"))
        .and(header("authorization", "Bearer siliconflow-secret"))
        .and(header_exists("idempotency-key"))
        .and(body_partial_json(json!({
            "model": "Wan-AI/Wan2.2-T2V-A14B",
            "prompt": "a fox in the wind",
            "image_size": "1280x720",
            "negative_prompt": "blur",
            "seed": 42
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"requestId": "sf-request-1"})),
        )
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/video/status"))
        .and(header("authorization", "Bearer siliconflow-secret"))
        .and(body_partial_json(json!({"requestId": "sf-request-1"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "Succeed",
            "reason": "provider-sensitive-reason-must-not-persist",
            "results": {
                "videos": [{"url": format!("{asset_url}/sf-result.mp4?token=siliconflow-sensitive")}],
                "timings": {"inference": 123},
                "seed": 42
            }
        })))
        .mount(server)
        .await;
    world.asset_mock = Some(asset_server);
}

#[when("the service creates a job-priced SiliconFlow video route and key")]
async fn create_siliconflow_video_route_and_key(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let response = world
        .client
        .post(format!("{}/internal/v1/upstreams", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": "siliconflow-shared-account",
            "driver": "http-json",
            "config": {
                "base_url": format!("{mock_url}/v1"),
                "network_scope": "private",
                "video_api": "siliconflow-v1",
                "video_models": ["Wan-AI/Wan2.2-T2V-A14B"],
                "result_origins": [world.asset_mock.as_ref().expect("asset mock").uri()]
            },
            "credential": {"type": "api_key", "value": "siliconflow-secret"}
        }))
        .send()
        .await
        .expect("create SiliconFlow upstream");
    let status = response.status();
    let response_body = response.text().await.expect("SiliconFlow account response");
    assert_eq!(status, StatusCode::CREATED, "{response_body}");
    let account: Value = serde_json::from_str(&response_body).expect("SiliconFlow account JSON");
    let response = world
        .client
        .post(format!("{}/internal/v1/model-routes", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "public_model": "siliconflow-video-public",
            "upstream_account_id": account["id"],
            "upstream_model": "Wan-AI/Wan2.2-T2V-A14B",
            "protocol": "generation",
            "custom_model_confirmed": true
        }))
        .send()
        .await
        .expect("create SiliconFlow video route");
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/generation-prices/USD/siliconflow-video-public",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"billing_unit": "job", "price_per_unit": "0.2"}))
        .send()
        .await
        .expect("create SiliconFlow video price");
    assert_eq!(response.status(), StatusCode::OK);
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "principal_external_id": "siliconflow-video-user",
            "alias": "siliconflow-video",
            "currency": "USD",
            "initial_balance": "10",
            "policy": {}
        }))
        .send()
        .await
        .expect("create SiliconFlow video key");
    assert_eq!(response.status(), StatusCode::CREATED);
    let key: Value = response.json().await.expect("SiliconFlow video key JSON");
    world.stable_key_id = key["key_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok());
    world.current_key = key["key"].as_str().expect("SiliconFlow key").to_owned();
    grant_fixture_model(
        world,
        "default",
        &key,
        "siliconflow-video-public",
        "generation",
    )
    .await;
}

#[when("the client creates and replays a SiliconFlow text to video generation")]
async fn create_and_replay_siliconflow_video(world: &mut TokenCenterWorld) {
    let body = json!({
        "model": "siliconflow-video-public",
        "input": {"parameters": {
            "prompt": "a fox in the wind",
            "image_size": "1280x720",
            "negative_prompt": "blur",
            "seed": 42
        }}
    });
    let first = world
        .client
        .post(format!("{}/v1/videos/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .header("idempotency-key", "siliconflow-video-idempotency")
        .json(&body)
        .send()
        .await
        .expect("create SiliconFlow video");
    world.status = Some(first.status());
    world.response = first.json().await.expect("SiliconFlow admission JSON");
    world.generation_job_id = world.response["job_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok());
    let replay = world
        .client
        .post(format!("{}/v1/videos/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .header("idempotency-key", "siliconflow-video-idempotency")
        .json(&body)
        .send()
        .await
        .expect("replay SiliconFlow video");
    assert_eq!(replay.status(), StatusCode::OK);
    let replay: Value = replay.json().await.expect("SiliconFlow replay JSON");
    assert_eq!(replay["job_id"], world.response["job_id"]);
}

#[then("the SiliconFlow video is archived once with safe metadata and job billing")]
async fn siliconflow_video_succeeds(world: &mut TokenCenterWorld) {
    let job_id = world
        .generation_job_id
        .expect("SiliconFlow generation job id");
    for _ in 0..80 {
        let value = world
            .client
            .get(format!(
                "{}/self/v1/generations/{job_id}",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .expect("SiliconFlow generation status")
            .json::<Value>()
            .await
            .expect("SiliconFlow generation status JSON");
        if value["status"] == "succeeded" {
            assert_eq!(value["driver"], "http-json");
            assert_eq!(value["billing_unit"], "job");
            assert_eq!(value["billed_units"], 1);
            assert_eq!(value["cost"], "0.2");
            assert_eq!(value["assets"].as_array().map(Vec::len), Some(1));
            assert_eq!(value["result"]["provider"], json!({"status": "Succeed"}));
            let public = value.to_string();
            for secret in [
                "siliconflow-sensitive",
                "provider-sensitive-reason",
                "sf-result.mp4",
                "objects/blake3/",
            ] {
                assert!(!public.contains(secret), "leaked {secret}");
            }
            let asset_id = value["assets"][0]["asset_id"]
                .as_str()
                .expect("SiliconFlow asset id");
            let asset = world
                .client
                .get(format!(
                    "{}/self/v1/generations/{job_id}/assets/{asset_id}",
                    world.service_url
                ))
                .bearer_auth(&world.current_key)
                .send()
                .await
                .expect("download SiliconFlow archive");
            assert_eq!(asset.status(), StatusCode::OK);
            assert_eq!(
                asset.bytes().await.expect("SiliconFlow archive bytes"),
                b"siliconflow-video-content".as_slice()
            );
            let requests = world
                .mock
                .as_ref()
                .expect("mock server")
                .received_requests()
                .await
                .expect("mock request recording");
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request.url.path() == "/v1/video/submit")
                    .count(),
                1
            );
            let submit = requests
                .iter()
                .find(|request| request.url.path() == "/v1/video/submit")
                .expect("one SiliconFlow submit request");
            assert_eq!(
                serde_json::from_slice::<Value>(&submit.body).expect("SiliconFlow submit JSON"),
                json!({
                    "model": "Wan-AI/Wan2.2-T2V-A14B",
                    "prompt": "a fox in the wind",
                    "image_size": "1280x720",
                    "negative_prompt": "blur",
                    "seed": 42
                })
            );
            let asset_requests = world
                .asset_mock
                .as_ref()
                .expect("asset mock")
                .received_requests()
                .await
                .expect("asset mock request recording");
            assert_eq!(asset_requests.len(), 1);
            assert!(
                asset_requests[0].headers.get("authorization").is_none(),
                "the API credential must not cross from the API origin to the result CDN"
            );
            assert_generation_stats(world, "siliconflow-video-public", "0.2").await;
            world.response = value;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!(
        "SiliconFlow generation did not complete: {}",
        world.response
    );
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
            "content": {
                "video_url": format!("{mock_url}/assets/video.mp4?token=seedance-sensitive-token"),
                "internal_path": "/provider/private/video.mp4"
            },
            "provider_token": "seedance-sensitive-envelope-token",
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

#[given("the mock Seedance upstream keeps a generation running until cancellation")]
async fn mock_cancellable_seedance_generation(world: &mut TokenCenterWorld) {
    let server = world.mock.as_ref().expect("mock server");
    Mock::given(method("POST"))
        .and(path("/api/v3/contents/generations/tasks"))
        .and(header("authorization", "Bearer seedance-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "cgt-cancellable"})))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/contents/generations/tasks/cgt-cancellable"))
        .and(header("authorization", "Bearer seedance-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "cgt-cancellable",
            "status": "running"
        })))
        .mount(server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v3/contents/generations/tasks/cgt-cancellable"))
        .and(header("authorization", "Bearer seedance-secret"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(server)
        .await;
}

#[given("the mock Seedance upstream returns an ambiguous server error after one submission")]
async fn mock_seedance_ambiguous_submission(world: &mut TokenCenterWorld) {
    let server = world.mock.as_ref().expect("mock server");
    Mock::given(method("POST"))
        .and(path("/api/v3/contents/generations/tasks"))
        .and(header("authorization", "Bearer seedance-secret"))
        .and(header_exists("idempotency-key"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "provider may have accepted this request",
            "token": "ambiguous-submission-sensitive-token"
        })))
        .expect(1)
        .mount(server)
        .await;
}

#[given("the mock Seedance upstream reports sixty seconds for a five second reservation")]
async fn mock_seedance_usage_exceeds_contract(world: &mut TokenCenterWorld) {
    let server = world.mock.as_ref().expect("mock server");
    Mock::given(method("POST"))
        .and(path("/api/v3/contents/generations/tasks"))
        .and(header("authorization", "Bearer seedance-secret"))
        .and(header_exists("idempotency-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "cgt-over-contract"})))
        .expect(1)
        .mount(server)
        .await;
    let mock_url = server.uri();
    Mock::given(method("GET"))
        .and(path("/api/v3/contents/generations/tasks/cgt-over-contract"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "cgt-over-contract",
            "status": "succeeded",
            "duration": "60",
            "content": {
                "video_url": format!("{mock_url}/assets/must-not-download.mp4?token=must-not-leak")
            }
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/assets/must-not-download.mp4"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "video/mp4")
                .set_body_bytes(b"must-not-be-archived"),
        )
        .expect(0)
        .mount(server)
        .await;
}

#[given("the mock Seedance upstream rejects the generation request")]
async fn mock_seedance_rejection(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/api/v3/contents/generations/tasks"))
        .and(header("authorization", "Bearer seedance-secret"))
        .and(header_exists("idempotency-key"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "rejected: provider-sensitive-token"},
            "internal_url": "https://provider.invalid/private?token=must-not-persist"
        })))
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[given("the mock Seedance upstream reports success without a video asset")]
async fn mock_seedance_success_without_asset(world: &mut TokenCenterWorld) {
    let server = world.mock.as_ref().expect("mock server");
    Mock::given(method("POST"))
        .and(path("/api/v3/contents/generations/tasks"))
        .and(header("authorization", "Bearer seedance-secret"))
        .and(header_exists("idempotency-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "cgt-missing-asset"})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/contents/generations/tasks/cgt-missing-asset"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "cgt-missing-asset",
            "status": "succeeded",
            "duration": "5",
            "content": {
                "internal_path": "/provider/private/missing.mp4",
                "token": "missing-seedance-sensitive-token"
            }
        })))
        .mount(server)
        .await;
}

#[given("the mock Seedance upstream returns a malicious job id")]
async fn mock_seedance_malicious_job_id(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/api/v3/contents/generations/tasks"))
        .and(header("authorization", "Bearer seedance-secret"))
        .and(header_exists("idempotency-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "../private?token=invalid-upstream-id-secret"
        })))
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
            "config": {"base_url": mock_url, "network_scope": "private"},
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
            "protocol": "generation",
            "custom_model_confirmed": true
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
            "policy": {}
        }))
        .send()
        .await
        .expect("create Seedance key");
    assert_eq!(response.status(), StatusCode::CREATED);
    let key: Value = response.json().await.expect("Seedance key JSON");
    world.stable_key_id = key["key_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok());
    world.current_key = key["key"].as_str().expect("Seedance key").to_owned();
    grant_fixture_model(world, "default", &key, "seedance-public", "generation").await;
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
            assert_eq!(value["assets"].as_array().map(Vec::len), Some(1));
            assert_eq!(value["result"]["assets"], value["assets"]);
            assert_eq!(
                value["result"]["provider"],
                json!({"status": "succeeded", "duration": 5})
            );
            assert!(!value.to_string().contains("objects/blake3/"));
            assert!(!value.to_string().contains("seedance-sensitive"));
            assert!(!value.to_string().contains("provider/private"));
            let stored = world
                .state
                .as_ref()
                .expect("test state")
                .db
                .generation_job(world.stable_key_id.expect("Seedance key id"), job_id)
                .await
                .expect("stored successful Seedance generation");
            let stored_result = stored.result.expect("safe Seedance result").to_string();
            assert!(!stored_result.contains("seedance-sensitive"));
            assert!(!stored_result.contains("provider/private"));
            assert_generation_stats(world, "seedance-public", "0.5").await;
            world.response = value;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("generation did not complete: {}", world.response);
}

#[then("the ambiguous Seedance submission fails closed without a second upstream POST")]
async fn seedance_ambiguous_submission_fails_closed(world: &mut TokenCenterWorld) {
    let job_id = world.generation_job_id.expect("generation job id");
    for _ in 0..40 {
        let value = world
            .client
            .get(format!(
                "{}/self/v1/generations/{job_id}",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .expect("ambiguous generation status")
            .json::<Value>()
            .await
            .expect("ambiguous generation status JSON");
        if value["status"] == "failed" {
            assert_eq!(value["error_code"], "submission_outcome_unknown");
            assert_eq!(value["billed_units"], 0);
            assert_eq!(value["cost"], "0");
            assert_eq!(value["result"], Value::Null);
            assert_eq!(value["assets"], json!([]));
            assert!(
                !value
                    .to_string()
                    .contains("ambiguous-submission-sensitive-token")
            );
            world.response = value;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert_eq!(world.response["error_code"], "submission_outcome_unknown");
    let requests = world
        .mock
        .as_ref()
        .expect("mock server")
        .received_requests()
        .await
        .expect("request recording enabled");
    let posts = requests
        .iter()
        .filter(|request| {
            request.method.as_str() == "POST"
                && request.url.path() == "/api/v3/contents/generations/tasks"
        })
        .collect::<Vec<_>>();
    assert_eq!(posts.len(), 1);
    let idempotency_key = posts[0]
        .headers
        .get("idempotency-key")
        .expect("upstream idempotency header")
        .to_str()
        .expect("ASCII idempotency header");
    assert_eq!(
        idempotency_key,
        world
            .generation_job_id
            .expect("generation job id")
            .to_string()
    );
}

#[then("the over-contract Seedance usage charges the reservation ceiling without an asset")]
async fn seedance_usage_exceeds_contract_is_bounded(world: &mut TokenCenterWorld) {
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
            .expect("over-contract generation status")
            .json::<Value>()
            .await
            .expect("over-contract generation status JSON");
        if value["status"] == "failed" {
            assert_eq!(value["error_code"], "upstream_usage_exceeds_contract");
            assert_eq!(value["billed_units"], 5);
            assert_eq!(value["cost"], "0.5");
            assert_eq!(value["assets"], json!([]));
            assert_eq!(value["result"], Value::Null);
            assert!(!value.to_string().contains("must-not-leak"));
            let key = world
                .client
                .get(format!("{}/self/v1/key", world.service_url))
                .bearer_auth(&world.current_key)
                .send()
                .await
                .expect("key after over-contract generation")
                .json::<Value>()
                .await
                .expect("key after over-contract generation JSON");
            assert_eq!(key["available_balance"], "9.5");
            world.response = value;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("over-contract generation did not fail: {}", world.response);
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
            assert_eq!(value["result"], Value::Null);
            assert!(!value.to_string().contains("provider-sensitive-token"));
            assert!(!value.to_string().contains("provider.invalid"));
            let stored = world
                .state
                .as_ref()
                .expect("test state")
                .db
                .generation_job(world.stable_key_id.expect("Seedance key id"), job_id)
                .await
                .expect("stored rejected generation");
            assert_eq!(stored.result, None);
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

#[then("the assetless Seedance success fails safely and refunds its entire reservation")]
async fn assetless_seedance_success_is_refunded(world: &mut TokenCenterWorld) {
    assert_generation_failure_is_sanitized_and_refunded(
        world,
        "seedance_missing_asset",
        &["missing-seedance-sensitive-token", "provider/private"],
    )
    .await;
}

#[then("the malicious Seedance job id is neither stored nor exposed")]
async fn malicious_seedance_job_id_is_rejected(world: &mut TokenCenterWorld) {
    let job_id = world.generation_job_id.expect("generation job id");
    for _ in 0..40 {
        let value = world
            .client
            .get(format!(
                "{}/self/v1/generations/{job_id}",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .expect("malicious id generation status")
            .json::<Value>()
            .await
            .expect("malicious id generation JSON");
        if value["status"] == "failed" {
            assert_eq!(value["error_code"], "submission_outcome_unknown");
            assert_eq!(value["upstream_job_id"], Value::Null);
            assert_eq!(value["billed_units"], 0);
            assert_eq!(value["cost"], "0");
            assert_eq!(value["assets"], json!([]));
            assert_eq!(value["result"], Value::Null);
            assert!(!value.to_string().contains("invalid-upstream-id-secret"));
            assert!(!value.to_string().contains("../private"));
            let stored = world
                .state
                .as_ref()
                .expect("test state")
                .db
                .generation_job(world.stable_key_id.expect("Seedance key id"), job_id)
                .await
                .expect("stored malicious id generation");
            assert_eq!(stored.upstream_job_id, None);
            assert!(!format!("{stored:?}").contains("invalid-upstream-id-secret"));
            let posts = world
                .mock
                .as_ref()
                .expect("mock server")
                .received_requests()
                .await
                .expect("request recording enabled")
                .into_iter()
                .filter(|request| {
                    request.method.as_str() == "POST"
                        && request.url.path() == "/api/v3/contents/generations/tasks"
                })
                .count();
            assert_eq!(posts, 1);
            world.response = value;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!(
        "malicious upstream job id was not rejected: {}",
        world.response
    );
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
                "provider_token": "successful-comfy-sensitive-token",
                "internal_path": "/provider/private/comfy-success.png",
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

#[given("the mock ComfyUI upstream keeps a generation running until cancellation")]
async fn mock_cancellable_comfyui_generation(world: &mut TokenCenterWorld) {
    let server = world.mock.as_ref().expect("mock server");
    Mock::given(method("POST"))
        .and(path("/prompt"))
        .and(header_exists("idempotency-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"prompt_id": "comfy-cancellable"})),
        )
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/history/comfy-cancellable"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/queue"))
        .and(body_partial_json(json!({"delete": ["comfy-cancellable"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"deleted": 1})))
        .expect(1)
        .mount(server)
        .await;
}

#[given("the mock ComfyUI upstream returns two images for a three image request")]
async fn mock_comfyui_megapixel_generation(world: &mut TokenCenterWorld) {
    let server = world.mock.as_ref().expect("mock server");
    Mock::given(method("POST"))
        .and(path("/prompt"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"prompt_id": "comfy-megapixel"})),
        )
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/history/comfy-megapixel"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "comfy-megapixel": {
                "status": {"status_str": "success", "completed": true},
                "outputs": {"9": {"images": [
                    {"filename": "mp-0.png", "subfolder": "", "type": "output"},
                    {"filename": "mp-1.png", "subfolder": "", "type": "output"}
                ]}}
            }
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/view"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(b"mock-megapixel-png"),
        )
        .expect(2)
        .mount(server)
        .await;
}

#[given("the mock ComfyUI upstream reports success without generated assets")]
async fn mock_comfyui_success_without_assets(world: &mut TokenCenterWorld) {
    let server = world.mock.as_ref().expect("mock server");
    Mock::given(method("POST"))
        .and(path("/prompt"))
        .and(header_exists("idempotency-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"prompt_id": "comfy-no-assets"})),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/history/comfy-no-assets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "comfy-no-assets": {
                "status": {"status_str": "success", "completed": true},
                "outputs": {
                    "9": {
                        "text": ["provider-sensitive-comfy-token"],
                        "internal_path": "/provider/private/comfy-output.png"
                    }
                }
            }
        })))
        .mount(server)
        .await;
}

#[given("the mock ComfyUI upstream returns seventeen generated assets")]
async fn mock_comfyui_oversized_manifest(world: &mut TokenCenterWorld) {
    let server = world.mock.as_ref().expect("mock server");
    Mock::given(method("POST"))
        .and(path("/prompt"))
        .and(header_exists("idempotency-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"prompt_id": "comfy-too-many"})),
        )
        .expect(1)
        .mount(server)
        .await;
    let images = (0..17)
        .map(|index| {
            json!({
                "filename": format!("result-{index}.png"),
                "subfolder": "",
                "type": "output"
            })
        })
        .collect::<Vec<_>>();
    Mock::given(method("GET"))
        .and(path("/history/comfy-too-many"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "comfy-too-many": {
                "status": {"status_str": "success", "completed": true},
                "outputs": {"9": {"images": images}}
            }
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/view"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(b"must-not-download-oversized-manifest"),
        )
        .expect(0)
        .mount(server)
        .await;
}

#[when("the service creates a metered ComfyUI route and key")]
async fn create_comfyui_route_and_key(world: &mut TokenCenterWorld) {
    create_metered_comfyui_route_and_key(
        world,
        "comfy-public",
        "workflow-v1",
        json!({
            "3": {"class_type": "KSampler", "inputs": {"seed": {"$mtc_param": "seed"}}},
            "9": {"class_type": "SaveImage", "inputs": {"filename_prefix": "MTC"}}
        }),
        "image-user",
        "image",
    )
    .await;
}

async fn create_metered_comfyui_route_and_key(
    world: &mut TokenCenterWorld,
    public_model: &str,
    workflow_id: &str,
    workflow_template: Value,
    principal: &str,
    alias: &str,
) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let response = world
        .client
        .post(format!("{}/internal/v1/upstreams", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": public_model,
            "driver": "comfyui",
            "config": {
                "base_url": mock_url,
                "api_prefix": "",
                "workflow_id": workflow_id,
                "workflow_template": workflow_template
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
            "public_model": public_model,
            "upstream_account_id": account["id"],
            "upstream_model": workflow_id,
            "protocol": "generation",
            "custom_model_confirmed": true
        }))
        .send()
        .await
        .expect("create ComfyUI route");
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/generation-prices/USD/{public_model}",
            world.service_url,
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
            "principal_external_id": principal,
            "alias": alias,
            "currency": "USD",
            "initial_balance": "10",
            "policy": {}
        }))
        .send()
        .await
        .expect("create ComfyUI key");
    assert_eq!(response.status(), StatusCode::CREATED);
    let key: Value = response.json().await.expect("ComfyUI key JSON");
    world.stable_key_id = key["key_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok());
    world.current_key = key["key"].as_str().expect("ComfyUI key").to_owned();
    grant_fixture_model(world, "default", &key, public_model, "generation").await;
}

#[when("the service creates a megapixel-priced ComfyUI route and key")]
async fn create_megapixel_comfyui_route_and_key(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let upstream = world
        .client
        .post(format!("{}/internal/v1/upstreams", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": "comfy-megapixel",
            "driver": "comfyui",
            "config": {
                "base_url": mock_url,
                "api_prefix": "",
                "workflow_id": "workflow-megapixel-v1",
                "workflow_template": {"1": {"inputs": {
                    "width": {"$mtc_param": "width"},
                    "height": {"$mtc_param": "height"},
                    "batch_size": {"$mtc_param": "batch_size"}
                }}}
            },
            "credential": {"type": "none"}
        }))
        .send()
        .await
        .expect("create megapixel ComfyUI upstream");
    assert_eq!(upstream.status(), StatusCode::CREATED);
    let upstream = upstream
        .json::<Value>()
        .await
        .expect("megapixel upstream JSON");
    let route = world
        .client
        .post(format!("{}/internal/v1/model-routes", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "public_model": "comfy-megapixel-public",
            "upstream_account_id": upstream["id"],
            "upstream_model": "workflow-megapixel-v1",
            "protocol": "generation",
            "custom_model_confirmed": true
        }))
        .send()
        .await
        .expect("create megapixel route");
    assert_eq!(route.status(), StatusCode::CREATED);
    let price = world
        .client
        .post(format!(
            "{}/internal/v1/generation-prices/USD/comfy-megapixel-public",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"billing_unit": "megapixel", "price_per_unit": "1"}))
        .send()
        .await
        .expect("create megapixel price");
    assert_eq!(price.status(), StatusCode::OK);
    let key = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "principal_external_id": "megapixel-user",
            "alias": "megapixel-user",
            "currency": "USD",
            "initial_balance": "10",
            "policy": {
                "tokens_per_minute": 10000000
            }
        }))
        .send()
        .await
        .expect("create megapixel key");
    assert_eq!(key.status(), StatusCode::CREATED);
    let key = key.json::<Value>().await.expect("megapixel key JSON");
    world.current_key = key["key"].as_str().expect("megapixel key").to_owned();
    grant_fixture_model(
        world,
        "default",
        &key,
        "comfy-megapixel-public",
        "generation",
    )
    .await;
}

#[when("the client creates a three-output ComfyUI megapixel generation")]
async fn create_comfyui_megapixel_generation(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/v1/images/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .header("idempotency-key", "comfy-megapixel-three")
        .json(&json!({
            "model": "comfy-megapixel-public",
            "input": {"parameters": {"width": 1024, "height": 512, "batch_size": 3}}
        }))
        .send()
        .await
        .expect("create megapixel generation");
    world.status = Some(response.status());
    world.response = response.json().await.expect("megapixel generation JSON");
    world.generation_job_id = world.response["job_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok());
    assert_eq!(
        world.status,
        Some(StatusCode::ACCEPTED),
        "{}",
        world.response
    );
}

#[then("the ComfyUI generation bills exactly 1.048576 megapixels and refunds the unused output")]
async fn comfyui_megapixel_bills_actual_outputs(world: &mut TokenCenterWorld) {
    let job_id = world.generation_job_id.expect("megapixel job id");
    let mut detail = Value::Null;
    for _ in 0..120 {
        let response = world
            .client
            .get(format!(
                "{}/self/v1/generations/{job_id}",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .expect("poll megapixel generation");
        assert_eq!(response.status(), StatusCode::OK);
        detail = response
            .json::<Value>()
            .await
            .expect("megapixel detail JSON");
        if detail["status"] == "succeeded" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(detail["status"], "succeeded", "{detail}");
    assert_eq!(detail["billing_unit"], "megapixel");
    assert_eq!(detail["estimated_units"], 1_572_864);
    assert_eq!(detail["billed_units"], 1_048_576);
    assert_eq!(detail["cost"], "1.048576");
    assert_eq!(detail["assets"].as_array().map(Vec::len), Some(2));

    let key = world
        .client
        .get(format!("{}/self/v1/key", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("read megapixel balance");
    assert_eq!(key.status(), StatusCode::OK);
    let key = key.json::<Value>().await.expect("megapixel key view JSON");
    assert_eq!(key["available_balance"], "8.951424");
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

#[then("cancelling the running ComfyUI generation is idempotent and refunds exactly once")]
async fn running_comfyui_cancellation_is_safe(world: &mut TokenCenterWorld) {
    let job_id = world.generation_job_id.expect("ComfyUI generation job id");
    let detail_url = format!("{}/self/v1/generations/{job_id}", world.service_url);
    for _ in 0..100 {
        let detail = world
            .client
            .get(&detail_url)
            .bearer_auth(&world.current_key)
            .send()
            .await
            .expect("poll running generation");
        assert_eq!(detail.status(), StatusCode::OK);
        let detail = detail
            .json::<Value>()
            .await
            .expect("running generation JSON");
        if detail["status"] == "running" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let cancellation = world
        .client
        .delete(&detail_url)
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("request running generation cancellation");
    assert_eq!(cancellation.status(), StatusCode::OK);
    let cancellation = cancellation
        .json::<Value>()
        .await
        .expect("cancelling generation JSON");
    assert_eq!(cancellation["status"], "cancelling", "{cancellation}");

    let mut terminal = Value::Null;
    for _ in 0..100 {
        let detail = world
            .client
            .get(&detail_url)
            .bearer_auth(&world.current_key)
            .send()
            .await
            .expect("poll cancelled generation");
        assert_eq!(detail.status(), StatusCode::OK);
        terminal = detail
            .json::<Value>()
            .await
            .expect("cancelled generation JSON");
        if terminal["status"] == "cancelled" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(terminal["status"], "cancelled", "{terminal}");
    assert_eq!(terminal["cost"], "0");
    assert_eq!(terminal["error_code"], "cancelled_by_user");

    let replay = world
        .client
        .delete(&detail_url)
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("replay generation cancellation");
    assert_eq!(replay.status(), StatusCode::OK);
    let replay = replay
        .json::<Value>()
        .await
        .expect("replayed cancellation JSON");
    assert_eq!(replay["status"], "cancelled");

    let key = world
        .client
        .get(format!("{}/self/v1/key", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("read key after cancellation");
    assert_eq!(key.status(), StatusCode::OK);
    let key = key
        .json::<Value>()
        .await
        .expect("key JSON after cancellation");
    assert_eq!(key["available_balance"], "10");
    let limits = world
        .client
        .get(format!("{}/self/v1/key/limits", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("read limits after cancellation");
    assert_eq!(limits.status(), StatusCode::OK);
    let limits = limits
        .json::<Value>()
        .await
        .expect("limits JSON after cancellation");
    assert_eq!(limits["concurrency"]["active"], 0);
}

#[then("cancelling the running Seedance generation is idempotent and refunds exactly once")]
async fn running_seedance_cancellation_is_safe(world: &mut TokenCenterWorld) {
    running_comfyui_cancellation_is_safe(world).await;
}

async fn create_generation_admission_key(
    world: &TokenCenterWorld,
    principal: &str,
    allowed_model: &str,
    initial_balance: &str,
    requests_per_minute: u32,
    tokens_per_minute: u64,
    max_concurrency: u32,
) -> String {
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "principal_external_id": principal,
            "alias": principal,
            "currency": "USD",
            "initial_balance": initial_balance,
            "policy": {
                "requests_per_minute": requests_per_minute,
                "tokens_per_minute": tokens_per_minute,
                "max_concurrency": max_concurrency
            }
        }))
        .send()
        .await
        .expect("create generation admission key");
    assert_eq!(response.status(), StatusCode::CREATED);
    let issued = response
        .json::<Value>()
        .await
        .expect("generation admission key JSON");
    let existing_route = world
        .state
        .as_ref()
        .expect("state")
        .db
        .list_model_routes(Some("default"))
        .await
        .expect("list generation admission routes")
        .into_iter()
        .find(|route| route.public_model == allowed_model && route.protocol == "generation");
    if let Some(route) = existing_route {
        grant_fixture_routes(
            world,
            "default",
            Uuid::parse_str(
                issued["key_id"]
                    .as_str()
                    .expect("generation admission key id"),
            )
            .expect("generation admission key UUID"),
            &[route.id],
        )
        .await;
    }
    issued["key"]
        .as_str()
        .expect("issued generation admission key")
        .to_owned()
}

async fn call_async_generation_admission(
    world: &TokenCenterWorld,
    key: &str,
    endpoint: &str,
    model: &str,
    input: &Value,
) -> (StatusCode, Value) {
    let response = world
        .client
        .post(format!("{}{endpoint}", world.service_url))
        .bearer_auth(key)
        .header("idempotency-key", Uuid::now_v7().to_string())
        .json(&json!({"model": model, "input": input}))
        .send()
        .await
        .expect("call asynchronous generation admission");
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .expect("generation admission response JSON");
    (status, body)
}

async fn assert_generation_admission_matrix(
    world: &mut TokenCenterWorld,
    driver: &str,
    model: &str,
    billing_unit: &str,
    input: Value,
) {
    if let Some(worker) = world.worker_task.take() {
        worker.abort();
        let _ = worker.await;
    }
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let endpoint = if driver == "comfyui" {
        "/v1/images/generations"
    } else {
        "/v1/videos/generations"
    };
    let config = if driver == "comfyui" {
        json!({
            "base_url": mock_url,
            "api_prefix": "",
            "workflow_id": "admission-workflow",
            "workflow_template": {
                "1": {"inputs": {
                    "width": {"$mtc_param": "width"},
                    "height": {"$mtc_param": "height"},
                    "batch_size": {"$mtc_param": "batch_size"}
                }}
            }
        })
    } else {
        json!({"base_url": mock_url})
    };
    let upstream = world
        .client
        .post(format!("{}/internal/v1/upstreams", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": format!("{model}-upstream"),
            "driver": driver,
            "config": config,
            "credential": {"type": "none"}
        }))
        .send()
        .await
        .expect("create admission upstream");
    assert_eq!(upstream.status(), StatusCode::CREATED);
    let upstream = upstream
        .json::<Value>()
        .await
        .expect("admission upstream JSON");
    let route = world
        .client
        .post(format!("{}/internal/v1/model-routes", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "public_model": model,
            "upstream_account_id": upstream["id"],
            "upstream_model": if driver == "comfyui" { "admission-workflow" } else { "seedance-admission" },
            "protocol": "generation",
            "custom_model_confirmed": true
        }))
        .send()
        .await
        .expect("create admission route");
    assert_eq!(route.status(), StatusCode::CREATED);
    let price = world
        .client
        .post(format!(
            "{}/internal/v1/generation-prices/USD/{model}",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"billing_unit": billing_unit, "price_per_unit": "0.1"}))
        .send()
        .await
        .expect("create admission price");
    assert_eq!(price.status(), StatusCode::OK);

    let forbidden = create_generation_admission_key(
        world,
        &format!("{model}-forbidden"),
        "another-model",
        "10",
        60,
        10_000_000,
        8,
    )
    .await;
    let (status, body) =
        call_async_generation_admission(world, &forbidden, endpoint, model, &input).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "forbidden");

    let quota = create_generation_admission_key(
        world,
        &format!("{model}-quota"),
        model,
        "0",
        60,
        10_000_000,
        8,
    )
    .await;
    let (status, body) =
        call_async_generation_admission(world, &quota, endpoint, model, &input).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"]["code"], "insufficient_quota");
    assert_eq!(body["error"]["reason"], "balance_exhausted");

    let rpm = create_generation_admission_key(
        world,
        &format!("{model}-rpm"),
        model,
        "10",
        1,
        10_000_000,
        8,
    )
    .await;
    let (first_status, first_body) =
        call_async_generation_admission(world, &rpm, endpoint, model, &input).await;
    assert_eq!(first_status, StatusCode::ACCEPTED, "{first_body}");
    let (mut status, mut body) =
        call_async_generation_admission(world, &rpm, endpoint, model, &input).await;
    if status == StatusCode::ACCEPTED {
        (status, body) =
            call_async_generation_admission(world, &rpm, endpoint, model, &input).await;
    }
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"]["reason"], "rpm_exhausted");

    let tpm =
        create_generation_admission_key(world, &format!("{model}-tpm"), model, "10", 60, 1, 8)
            .await;
    let (status, body) =
        call_async_generation_admission(world, &tpm, endpoint, model, &input).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"]["reason"], "tpm_exhausted");

    let concurrency = create_generation_admission_key(
        world,
        &format!("{model}-concurrency"),
        model,
        "10",
        60,
        10_000_000,
        1,
    )
    .await;
    let (first_status, first_body) =
        call_async_generation_admission(world, &concurrency, endpoint, model, &input).await;
    assert_eq!(first_status, StatusCode::ACCEPTED, "{first_body}");
    let (status, body) =
        call_async_generation_admission(world, &concurrency, endpoint, model, &input).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"]["reason"], "concurrency_exhausted");

    let received = world
        .mock
        .as_ref()
        .expect("mock server")
        .received_requests()
        .await
        .expect("mock request journal");
    assert!(received.is_empty(), "admission must precede upstream work");
}

#[then(
    "the asynchronous ComfyUI image endpoint rejects permission quota RPM TPM and concurrency violations"
)]
async fn comfyui_image_admission_matrix(world: &mut TokenCenterWorld) {
    assert_generation_admission_matrix(
        world,
        "comfyui",
        "comfy-admission-image",
        "megapixel",
        json!({"parameters": {"width": 512, "height": 512, "batch_size": 1}}),
    )
    .await;
}

#[then(
    "the asynchronous Seedance video endpoint rejects permission quota RPM TPM and concurrency violations"
)]
async fn seedance_video_admission_matrix(world: &mut TokenCenterWorld) {
    assert_generation_admission_matrix(
        world,
        "volcengine-seedance",
        "seedance-admission-video",
        "second",
        json!({"duration": 5, "content": [{"text": "mock video"}]}),
    )
    .await;
}

#[when("the generation worker is stopped before it can submit upstream")]
async fn stop_generation_worker(world: &mut TokenCenterWorld) {
    let worker = world.worker_task.take().expect("generation worker task");
    worker.abort();
    let _ = worker.await;
}

#[when("a durable ComfyUI manifest is persisted before terminal settlement")]
async fn persist_comfyui_manifest_before_terminal_settlement(world: &mut TokenCenterWorld) {
    let state = world.state.as_ref().expect("test state");
    let job_id = world.generation_job_id.expect("ComfyUI generation job id");
    let submitting_worker = "manifest-submitting-worker";
    let claimed = state
        .db
        .claim_generation_job(submitting_worker)
        .await
        .expect("claim queued manifest job")
        .expect("queued manifest job");
    assert_eq!(claimed.job_id, job_id);
    let submission_nonce = Uuid::now_v7();
    state
        .db
        .mark_generation_submitting(job_id, submitting_worker, submission_nonce)
        .await
        .expect("mark manifest job submitting");
    state
        .db
        .mark_generation_submitted(
            job_id,
            submitting_worker,
            submission_nonce,
            "manifest-recovery-upstream-job",
        )
        .await
        .expect("mark manifest job submitted");

    tokio::time::sleep(std::time::Duration::from_millis(2_050)).await;
    let settlement_worker = "manifest-settlement-worker";
    let running = state
        .db
        .claim_generation_job(settlement_worker)
        .await
        .expect("claim running manifest job")
        .expect("running manifest job");
    assert_eq!(running.job_id, job_id);
    let attempt_nonce = Uuid::now_v7();
    let staging_lease = match state
        .db
        .begin_archive_staging_attempt(BeginArchiveStagingInput {
            key: ArchiveStagingKey::new(
                ArchiveStagingOwner::GenerationJob(job_id),
                ArchiveStagingPurpose::Assets,
                attempt_nonce,
            )
            .expect("valid generation staging key"),
            intent_digest: ArchiveStagingIntentDigest::new("b".repeat(64))
                .expect("fixed non-secret typed test intent"),
            lease_token: Uuid::now_v7(),
            lease_owner: ArchiveStagingLeaseOwner::new("manifest-settlement-worker")
                .expect("safe test lease owner"),
        })
        .await
        .expect("begin durable generation staging intent")
    {
        BeginArchiveStagingResult::Created(lease) => lease,
        other => panic!("unexpected generation staging begin result: {other:?}"),
    };
    let object_locator = format!("{}/asset-0", staging_lease.key.canonical_prefix());
    let body = bytes::Bytes::from_static(b"durable-manifest-image");
    state
        .archive
        .put(&object_locator, body.clone())
        .await
        .expect("persist durable manifest object");
    let manifest = GenerationStagedAssets {
        attempt_nonce,
        billed_units: 1,
        assets: vec![ArchivedGenerationAsset {
            asset_id: Uuid::now_v7(),
            index: 0,
            object_locator,
            mime_type: "image/png".to_owned(),
            size_bytes: i64::try_from(body.len()).expect("manifest body size"),
            filename: "recovered.png".to_owned(),
        }],
    };
    assert!(
        state
            .db
            .save_generation_staged_assets_staged(
                job_id,
                settlement_worker,
                &manifest,
                &staging_lease,
            )
            .await
            .expect("persist exact generation manifest and binding")
    );
    state
        .db
        .reschedule_generation_job(job_id, settlement_worker, 0, None)
        .await
        .expect("simulate crash before terminal settlement");
}

#[then("the restarted worker settles the durable manifest without contacting ComfyUI")]
async fn restarted_worker_recovers_comfyui_manifest(world: &mut TokenCenterWorld) {
    let state = world.state.clone().expect("test state");
    world.worker_task = Some(tokio::spawn(worker::run(state)));
    let job_id = world.generation_job_id.expect("ComfyUI generation job id");
    for _ in 0..30 {
        let detail = world
            .client
            .get(format!(
                "{}/self/v1/generations/{job_id}",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .expect("recovered generation status")
            .json::<Value>()
            .await
            .expect("recovered generation status JSON");
        if detail["status"] == "succeeded" {
            assert_eq!(detail["cost"], "0.2");
            assert_eq!(detail["billed_units"], 1);
            assert_eq!(detail["assets"].as_array().map(Vec::len), Some(1));
            let asset_id = detail["assets"][0]["asset_id"]
                .as_str()
                .expect("recovered asset id");
            let downloaded = world
                .client
                .get(format!(
                    "{}/self/v1/generations/{job_id}/assets/{asset_id}",
                    world.service_url
                ))
                .bearer_auth(&world.current_key)
                .send()
                .await
                .expect("download recovered manifest asset");
            assert_eq!(downloaded.status(), StatusCode::OK);
            assert_eq!(
                downloaded.bytes().await.expect("recovered manifest bytes"),
                bytes::Bytes::from_static(b"durable-manifest-image")
            );
            let upstream_requests = world
                .mock
                .as_ref()
                .expect("mock server")
                .received_requests()
                .await
                .expect("request recording enabled")
                .into_iter()
                .filter(|request| {
                    matches!(
                        request.url.path(),
                        "/prompt" | "/history/manifest-recovery-upstream-job" | "/view"
                    )
                })
                .count();
            assert_eq!(upstream_requests, 0);
            world.response = detail;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("durable manifest was not recovered: {}", world.response);
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
            assert_eq!(value["assets"].as_array().map(Vec::len), Some(1));
            assert_eq!(value["assets"][0]["mime_type"], "image/png");
            assert_eq!(value["assets"][0]["filename"], "result.png");
            assert_eq!(value["result"]["assets"], value["assets"]);
            assert!(!value.to_string().contains("objects/blake3/"));
            assert!(
                !value
                    .to_string()
                    .contains("successful-comfy-sensitive-token")
            );
            assert!(!value.to_string().contains("provider/private"));
            let stored = world
                .state
                .as_ref()
                .expect("test state")
                .db
                .generation_job(world.stable_key_id.expect("ComfyUI key id"), job_id)
                .await
                .expect("stored successful ComfyUI generation");
            let stored_result = stored.result.expect("safe ComfyUI result").to_string();
            assert!(!stored_result.contains("successful-comfy-sensitive-token"));
            assert!(!stored_result.contains("provider/private"));
            let asset_id = value["assets"][0]["asset_id"]
                .as_str()
                .expect("ComfyUI image asset id");
            let downloaded = world
                .client
                .get(format!(
                    "{}/self/v1/generations/{job_id}/assets/{asset_id}",
                    world.service_url
                ))
                .bearer_auth(&world.current_key)
                .send()
                .await
                .expect("download ComfyUI PNG");
            assert_eq!(downloaded.status(), StatusCode::OK);
            assert_eq!(
                downloaded.bytes().await.expect("downloaded PNG bytes"),
                bytes::Bytes::from_static(b"mock-png-content")
            );
            assert_generation_stats(world, "comfy-public", "0.2").await;
            world.response = value;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("ComfyUI generation did not complete: {}", world.response);
}

#[then("the assetless ComfyUI success fails safely and refunds its entire reservation")]
async fn assetless_comfyui_success_is_refunded(world: &mut TokenCenterWorld) {
    assert_generation_failure_is_sanitized_and_refunded(
        world,
        "comfyui_missing_assets",
        &["provider-sensitive-comfy-token", "provider/private"],
    )
    .await;
}

#[then("the oversized ComfyUI manifest fails before downloads and refunds its reservation")]
async fn oversized_comfyui_manifest_is_refunded(world: &mut TokenCenterWorld) {
    assert_generation_failure_is_sanitized_and_refunded(world, "comfyui_asset_limit_exceeded", &[])
        .await;
}

async fn assert_generation_failure_is_sanitized_and_refunded(
    world: &mut TokenCenterWorld,
    expected_error_code: &str,
    forbidden: &[&str],
) {
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
            .expect("assetless generation status")
            .json::<Value>()
            .await
            .expect("assetless generation status JSON");
        if value["status"] == "failed" {
            assert_eq!(value["billed_units"], 0);
            assert_eq!(value["cost"], "0");
            assert_eq!(value["error_code"], expected_error_code);
            assert_eq!(value["result"], Value::Null);
            assert_eq!(value["assets"], json!([]));
            let serialized = value.to_string();
            for secret in forbidden {
                assert!(!serialized.contains(secret));
            }
            let stored = world
                .state
                .as_ref()
                .expect("test state")
                .db
                .generation_job(world.stable_key_id.expect("generation key id"), job_id)
                .await
                .expect("stored assetless generation");
            assert_eq!(stored.result, None);
            let key = world
                .client
                .get(format!("{}/self/v1/key", world.service_url))
                .bearer_auth(&world.current_key)
                .send()
                .await
                .expect("key after assetless generation")
                .json::<Value>()
                .await
                .expect("key after assetless generation JSON");
            assert_eq!(key["available_balance"], "10");
            world.response = value;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("assetless generation did not fail: {}", world.response);
}

#[given("the mock ComfyUI upstream completes an MP4 video workflow")]
async fn mock_comfyui_video_generation(world: &mut TokenCenterWorld) {
    let server = world.mock.as_ref().expect("mock server");
    Mock::given(method("POST"))
        .and(path("/prompt"))
        .and(header_exists("idempotency-key"))
        .and(body_partial_json(json!({
            "prompt": {
                "12": {
                    "class_type": "VHS_VideoCombine",
                    "inputs": {"frame_rate": 24}
                }
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"prompt_id": "comfy-video"})))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/history/comfy-video"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "comfy-video": {
                "status": {"status_str": "success", "completed": true},
                "outputs": {
                    "12": {
                        "videos": [{
                            "filename": "result.mp4",
                            "subfolder": "videos",
                            "type": "output"
                        }]
                    }
                }
            }
        })))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/view"))
        .and(query_param("filename", "result.mp4"))
        .and(query_param("subfolder", "videos"))
        .and(query_param("type", "output"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "video/mp4")
                .set_body_bytes(b"mock-comfy-video-content"),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[when("the service creates a metered ComfyUI video route and key")]
async fn create_comfyui_video_route_and_key(world: &mut TokenCenterWorld) {
    create_metered_comfyui_route_and_key(
        world,
        "comfy-video-public",
        "video-workflow-v1",
        json!({
            "12": {
                "class_type": "VHS_VideoCombine",
                "inputs": {"frame_rate": {"$mtc_param": "frame_rate"}}
            }
        }),
        "video-user",
        "video",
    )
    .await;
}

#[when("the client creates a ComfyUI video generation")]
async fn create_comfyui_video_generation(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/v1/videos/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .json(&json!({
            "model": "comfy-video-public",
            "input": {
                "parameters": {"frame_rate": 24}
            }
        }))
        .send()
        .await
        .expect("create ComfyUI video generation");
    world.status = Some(response.status());
    world.response = response
        .json()
        .await
        .expect("ComfyUI video generation JSON");
    world.generation_job_id = world.response["job_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok());
}

#[then(
    "the ComfyUI video is available through self service with exact archived content and cost 0.2"
)]
async fn comfyui_video_generation_succeeds(world: &mut TokenCenterWorld) {
    let job_id = world
        .generation_job_id
        .expect("ComfyUI video generation job id");
    for _ in 0..30 {
        let detail = world
            .client
            .get(format!(
                "{}/self/v1/generations/{job_id}",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .expect("ComfyUI video self-service detail");
        assert_eq!(detail.status(), StatusCode::OK);
        let detail: Value = detail
            .json()
            .await
            .expect("ComfyUI video self-service detail JSON");
        if detail["status"] == "succeeded" {
            assert_eq!(detail["billed_units"], 1);
            assert_eq!(detail["cost"], "0.2");
            assert_eq!(detail["result"]["provider"]["status"], "success");
            let asset = &detail["assets"][0];
            assert_eq!(asset["index"], 0);
            assert_eq!(asset["mime_type"], "video/mp4");
            assert_eq!(
                asset["size_bytes"],
                u64::try_from(b"mock-comfy-video-content".len()).unwrap()
            );
            assert_eq!(asset["filename"], "result.mp4");
            assert_eq!(detail["result"]["assets"][0], *asset);
            assert!(
                !detail.to_string().contains("objects/blake3/"),
                "self-service metadata must not expose an internal archive locator"
            );
            let asset_id = asset["asset_id"]
                .as_str()
                .and_then(|value| Uuid::parse_str(value).ok())
                .expect("ComfyUI video asset id");

            let downloaded = world
                .client
                .get(format!(
                    "{}/self/v1/generations/{job_id}/assets/{asset_id}",
                    world.service_url
                ))
                .bearer_auth(&world.current_key)
                .send()
                .await
                .expect("download ComfyUI video asset");
            assert_eq!(downloaded.status(), StatusCode::OK);
            assert_eq!(
                downloaded
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some("video/mp4")
            );
            assert_eq!(
                downloaded
                    .bytes()
                    .await
                    .expect("downloaded ComfyUI video bytes"),
                bytes::Bytes::from_static(b"mock-comfy-video-content")
            );

            let ranged = world
                .client
                .get(format!(
                    "{}/self/v1/generations/{job_id}/assets/{asset_id}",
                    world.service_url
                ))
                .bearer_auth(&world.current_key)
                .header(reqwest::header::RANGE, "bytes=5-9")
                .send()
                .await
                .expect("range download ComfyUI video asset");
            assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(
                ranged
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|value| value.to_str().ok()),
                Some("bytes 5-9/24")
            );
            assert_eq!(
                ranged.bytes().await.expect("ranged ComfyUI video bytes"),
                bytes::Bytes::from_static(b"comfy")
            );
            for invalid in ["bytes=99-", "bytes=0-1,3-4"] {
                let response = world
                    .client
                    .get(format!(
                        "{}/self/v1/generations/{job_id}/assets/{asset_id}",
                        world.service_url
                    ))
                    .bearer_auth(&world.current_key)
                    .header(reqwest::header::RANGE, invalid)
                    .send()
                    .await
                    .expect("invalid ComfyUI video range");
                assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
                assert_eq!(
                    response
                        .headers()
                        .get(reqwest::header::CONTENT_RANGE)
                        .and_then(|value| value.to_str().ok()),
                    Some("bytes */24")
                );
            }
            let non_utf8_range = world
                .client
                .get(format!(
                    "{}/self/v1/generations/{job_id}/assets/{asset_id}",
                    world.service_url
                ))
                .bearer_auth(&world.current_key)
                .header(
                    reqwest::header::RANGE,
                    reqwest::header::HeaderValue::from_bytes(b"bytes=\xff")
                        .expect("opaque non-UTF-8 Range header"),
                )
                .send()
                .await
                .expect("non-UTF-8 ComfyUI video range");
            assert_eq!(non_utf8_range.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(
                non_utf8_range
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|value| value.to_str().ok()),
                Some("bytes */24")
            );

            let other_key = world
                .client
                .post(format!("{}/internal/v1/keys", world.service_url))
                .bearer_auth("test-service-token")
                .json(&json!({
                    "principal_external_id": "other-asset-user",
                    "alias": "other-asset-user"
                }))
                .send()
                .await
                .expect("create unrelated asset key");
            assert_eq!(other_key.status(), StatusCode::CREATED);
            let other_key: Value = other_key.json().await.expect("unrelated key JSON");
            let wrong_owner = world
                .client
                .get(format!(
                    "{}/self/v1/generations/{job_id}/assets/{asset_id}",
                    world.service_url
                ))
                .bearer_auth(other_key["key"].as_str().expect("unrelated key"))
                .send()
                .await
                .expect("cross-key asset request");
            assert_eq!(wrong_owner.status(), StatusCode::NOT_FOUND);

            let operator = world
                .client
                .get(format!(
                    "{}/internal/v1/generations/{job_id}/assets/{asset_id}?tenant_external_id=default",
                    world.service_url
                ))
                .bearer_auth("test-service-token")
                .send()
                .await
                .expect("operator asset request");
            assert_eq!(operator.status(), StatusCode::OK);
            assert_eq!(
                operator.bytes().await.expect("operator asset bytes"),
                bytes::Bytes::from_static(b"mock-comfy-video-content")
            );
            let wrong_tenant = world
                .client
                .get(format!(
                    "{}/internal/v1/generations/{job_id}/assets/{asset_id}?tenant_external_id=other-tenant",
                    world.service_url
                ))
                .bearer_auth("test-service-token")
                .send()
                .await
                .expect("cross-tenant operator asset request");
            assert_eq!(wrong_tenant.status(), StatusCode::NOT_FOUND);

            let state = world.state.clone().expect("test application state");
            let key = state
                .db
                .authenticate_key(&world.current_key, state.config.key_pepper.as_bytes())
                .await
                .expect("authenticate ComfyUI video key");
            let stored = state
                .db
                .generation_asset_for_key(key.key_id, job_id, asset_id)
                .await
                .expect("stored ComfyUI video asset");
            let expected_prefix = format!("staging/generation/{job_id}/assets/");
            assert!(stored.object_locator.starts_with(&expected_prefix));
            let suffix = stored
                .object_locator
                .strip_prefix(&expected_prefix)
                .expect("job-scoped generation object");
            let (attempt_nonce, asset_name) = suffix
                .split_once('/')
                .expect("attempt-scoped generation object");
            Uuid::parse_str(attempt_nonce).expect("opaque generation attempt nonce");
            assert_eq!(asset_name, "asset-0");
            assert_eq!(stored.view.mime_type, "video/mp4");
            assert_eq!(stored.view.filename, "result.mp4");
            assert_eq!(
                state
                    .archive
                    .get(&stored.object_locator)
                    .await
                    .expect("archived ComfyUI video bytes"),
                bytes::Bytes::from_static(b"mock-comfy-video-content")
            );

            let list = world
                .client
                .get(format!("{}/self/v1/generations", world.service_url))
                .bearer_auth(&world.current_key)
                .send()
                .await
                .expect("ComfyUI video self-service list");
            assert_eq!(list.status(), StatusCode::OK);
            let list: Value = list
                .json()
                .await
                .expect("ComfyUI video self-service list JSON");
            assert_eq!(list.as_array().map(Vec::len), Some(1));
            assert_eq!(list[0]["job_id"], job_id.to_string());
            assert_eq!(list[0]["assets"][0], *asset);
            assert!(!list.to_string().contains("objects/blake3/"));

            let key = world
                .client
                .get(format!("{}/self/v1/key", world.service_url))
                .bearer_auth(&world.current_key)
                .send()
                .await
                .expect("key after ComfyUI video generation");
            assert_eq!(key.status(), StatusCode::OK);
            let key: Value = key
                .json()
                .await
                .expect("key after ComfyUI video generation JSON");
            assert_eq!(key["available_balance"], "9.8");
            assert_generation_stats(world, "comfy-video-public", "0.2").await;
            world.response = detail;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!(
        "ComfyUI video generation did not complete: {}",
        world.response
    );
}

#[given("the mock OpenAI Images upstream returns a generated icon")]
async fn mock_openai_image_generation(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(header("authorization", "Bearer image-secret"))
        .and(header_exists("idempotency-key"))
        .and(body_partial_json(json!({
            "model": "gpt-image-upstream",
            "prompt": "a compact token loop icon"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 1,
            "data": [{"b64_json": "bW9jay1wbmc="}]
        })))
        .expect(2)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[given("the mock OpenAI Images upstream returns a generated icon without requiring idempotency")]
async fn mock_non_idempotent_openai_image_generation(world: &mut TokenCenterWorld) {
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
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[given("the mock OpenAI Images upstream returns an exact-origin signed URL")]
async fn mock_openai_image_url_generation(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(header("authorization", "Bearer image-secret"))
        .and(header_exists("idempotency-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 1,
            "id": "provider-secret-response-id",
            "debug_url": "https://provider.invalid/debug?token=must-not-leak",
            "data": [{
                "url": format!("{mock_url}/generated/SECRET_TOKEN.png?token=must-not-leak"),
                "provider_trace": "must-not-leak"
            }],
            "usage": {"total_tokens": 7, "provider_debug": "must-not-leak"}
        })))
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    Mock::given(method("GET"))
        .and(path("/generated/SECRET_TOKEN.png"))
        .and(query_param("token", "must-not-leak"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(b"exact-url-png"),
        )
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[given("the mock OpenAI Images upstream returns an empty signed URL asset")]
async fn mock_empty_openai_image_url_generation(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(header("authorization", "Bearer image-secret"))
        .and(header_exists("idempotency-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 1,
            "data": [{"url": format!("{mock_url}/generated/empty.png?token=must-not-leak")}]
        })))
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    Mock::given(method("GET"))
        .and(path("/generated/empty.png"))
        .and(query_param("token", "must-not-leak"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(Vec::<u8>::new()),
        )
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[given("the mock OpenAI Images upstream returns ten assets over the aggregate budget")]
async fn mock_openai_image_aggregate_limit(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let data = (0..10)
        .map(|index| json!({"url": format!("{mock_url}/generated/aggregate-{index}.png")}))
        .collect::<Vec<_>>();
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(header("authorization", "Bearer image-secret"))
        .and(header_exists("idempotency-key"))
        .and(body_partial_json(json!({"n": 10})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 1,
            "data": data
        })))
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    for index in 0..9 {
        Mock::given(method("GET"))
            .and(path(format!("/generated/aggregate-{index}.png")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes([u8::try_from(index).expect("small index")]),
            )
            .expect(1)
            .mount(world.mock.as_ref().expect("mock server"))
            .await;
    }
    // This final object fits an empty 512 MiB budget exactly, but cannot fit
    // the same request budget after the first nine assets. The body is never
    // read because Content-Length is checked before streaming.
    Mock::given(method("GET"))
        .and(path("/generated/aggregate-9.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .insert_header("content-length", "536870912"),
        )
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[given("the mock OpenAI Images upstream exceeds the response limit by one byte")]
async fn mock_oversized_openai_image_generation(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(header("authorization", "Bearer image-secret"))
        .and(header_exists("idempotency-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_bytes(vec![b'x'; 16 * 1024 * 1024 + 1]),
        )
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[given("the mock OpenAI Images upstream rejects with a sensitive error body")]
async fn mock_sensitive_openai_image_rejection(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(header("authorization", "Bearer image-secret"))
        .and(header_exists("idempotency-key"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {
                "message": "provider detail must-not-leak",
                "signed_url": "https://images.example.invalid/result.png?token=must-not-leak"
            }
        })))
        .expect(1)
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
            "config": {"base_url": mock_url, "network_scope": "private"},
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
            "protocol": "generation",
            "custom_model_confirmed": true
        }))
        .send()
        .await
        .expect("create Images route");
    assert_eq!(response.status(), StatusCode::CREATED);
    let route: Value = response.json().await.expect("Images route JSON");
    world.image_route_id = route["id"].as_str().and_then(|id| Uuid::parse_str(id).ok());
    world.image_route_updated_at = route["updated_at"]
        .as_i64()
        .expect("Images route updated_at");
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
            "initial_balance": "0.3",
            "policy": {}
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
    world.stable_account_id = key["account_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok());
    grant_fixture_model(world, "default", &key, "gpt-image-public", "generation").await;
}

#[when("the client creates an OpenAI-compatible image")]
async fn create_openai_compatible_image(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/v1/images/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .header("idempotency-key", "openai-image-stable-1")
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
    world.synchronous_request_id = response
        .headers()
        .get("x-mtc-request-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok());
    world.synchronous_response_content_length = response
        .content_length()
        .map(|value| usize::try_from(value).expect("Images Content-Length fits the platform"));
    world.synchronous_response_body = response
        .bytes()
        .await
        .expect("Images response body")
        .to_vec();
    world.response =
        serde_json::from_slice(&world.synchronous_response_body).expect("Images response JSON");
}

#[when("the client creates an OpenAI-compatible image without an idempotency key")]
async fn create_non_idempotent_openai_compatible_image(world: &mut TokenCenterWorld) {
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
        .expect("create non-idempotent OpenAI-compatible image");
    world.status = Some(response.status());
    world.synchronous_request_id = response
        .headers()
        .get("x-mtc-request-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok());
    world.response = response
        .json()
        .await
        .expect("non-idempotent Images response JSON");
}

#[when("the client creates ten OpenAI-compatible images in one request")]
async fn create_ten_openai_compatible_images(world: &mut TokenCenterWorld) {
    let grant = world
        .client
        .post(format!(
            "{}/internal/v1/accounts/{}/grants",
            world.service_url,
            world.stable_account_id.expect("Images account id")
        ))
        .bearer_auth("test-service-token")
        .header("idempotency-key", "image-aggregate-budget-credit")
        .json(&json!({"amount": "2.7", "source": "aggregate-budget-test"}))
        .send()
        .await
        .expect("grant aggregate image test credit");
    assert_eq!(grant.status(), StatusCode::CREATED);
    let response = world
        .client
        .post(format!("{}/v1/images/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .header("idempotency-key", "openai-image-aggregate-budget")
        .json(&json!({
            "model": "gpt-image-public",
            "prompt": "ten bounded icons",
            "n": 10,
            "size": "1024x1024"
        }))
        .send()
        .await
        .expect("create ten OpenAI-compatible images");
    world.status = Some(response.status());
    world.synchronous_request_id = response
        .headers()
        .get("x-mtc-request-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok());
    world.response = response
        .json()
        .await
        .expect("aggregate Images response JSON");
}

async fn replay_openai_compatible_image(world: &TokenCenterWorld) -> reqwest::Response {
    world
        .client
        .post(format!("{}/v1/images/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .header("idempotency-key", "openai-image-stable-1")
        .json(&json!({
            "model": "gpt-image-public",
            "prompt": "a compact token loop icon",
            "n": 1,
            "size": "1024x1024"
        }))
        .send()
        .await
        .expect("replay OpenAI-compatible image")
}

#[then("the OpenAI image response is archived and costs 0.3")]
async fn openai_image_is_archived_and_metered(world: &mut TokenCenterWorld) {
    assert_eq!(world.response["data"][0]["b64_json"], "bW9jay1wbmc=");
    let replay = replay_openai_compatible_image(world).await;
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(
        world.synchronous_response_content_length,
        Some(world.synchronous_response_body.len())
    );
    let expected_request_id = world
        .synchronous_request_id
        .expect("synchronous image request id")
        .to_string();
    assert_eq!(
        replay
            .headers()
            .get("x-mtc-request-id")
            .and_then(|value| value.to_str().ok()),
        Some(expected_request_id.as_str())
    );
    assert_eq!(
        replay
            .content_length()
            .and_then(|value| usize::try_from(value).ok()),
        Some(world.synchronous_response_body.len())
    );
    assert_eq!(
        replay.bytes().await.expect("replayed image body").as_ref(),
        world.synchronous_response_body.as_slice()
    );
    let state = world.state.clone().expect("test application state");
    let archived_response = state
        .db
        .request_archive_refs(
            world.stable_key_id.expect("stable image key id"),
            world
                .synchronous_request_id
                .expect("synchronous image request id"),
        )
        .await
        .expect("OpenAI image archive references")
        .response_object
        .expect("OpenAI image response object");
    assert_eq!(
        state
            .archive
            .get(&archived_response)
            .await
            .expect("read OpenAI image response archive")
            .as_ref(),
        world.synchronous_response_body.as_slice()
    );
    let mismatch = world
        .client
        .post(format!("{}/v1/images/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .header("idempotency-key", "openai-image-stable-1")
        .json(&json!({
            "model": "gpt-image-public",
            "prompt": "a different image must not reuse the claim",
            "n": 1,
            "size": "1024x1024"
        }))
        .send()
        .await
        .expect("mismatched OpenAI image replay");
    assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);
    let key: Value = world
        .client
        .get(format!("{}/self/v1/key", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("image key after exact replay")
        .json()
        .await
        .expect("image key JSON after exact replay");
    assert_eq!(key["available_balance"], "0");
    let upstream_requests = world
        .mock
        .as_ref()
        .expect("mock server")
        .received_requests()
        .await
        .expect("received image upstream requests")
        .into_iter()
        .filter(|request| request.url.path() == "/v1/images/generations")
        .collect::<Vec<_>>();
    assert_eq!(upstream_requests.len(), 1);
    let upstream_idempotency = upstream_requests[0]
        .headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .expect("derived upstream image idempotency header");
    assert!(upstream_idempotency.starts_with("mtc-img-"));
    assert_ne!(upstream_idempotency, "openai-image-stable-1");
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
            let other = world
                .client
                .post(format!("{}/internal/v1/keys", world.service_url))
                .bearer_auth("test-service-token")
                .json(&json!({
                    "principal_external_id": "image-api-second-identity",
                    "alias": "image-api-second-identity",
                    "currency": "USD",
                    "initial_balance": "0.3",
                    "policy": {}
                }))
                .send()
                .await
                .expect("create second image identity")
                .json::<Value>()
                .await
                .expect("second image identity JSON");
            grant_fixture_model(world, "default", &other, "gpt-image-public", "generation").await;
            let second = world
                .client
                .post(format!("{}/v1/images/generations", world.service_url))
                .bearer_auth(other["key"].as_str().expect("second image identity secret"))
                .header("idempotency-key", "openai-image-stable-1")
                .json(&json!({
                    "model": "gpt-image-public",
                    "prompt": "a compact token loop icon",
                    "n": 1,
                    "size": "1024x1024"
                }))
                .send()
                .await
                .expect("same image idempotency key under another identity");
            assert_eq!(second.status(), StatusCode::OK);
            let upstream_idempotencies = world
                .mock
                .as_ref()
                .expect("mock server")
                .received_requests()
                .await
                .expect("cross-identity upstream image requests")
                .into_iter()
                .filter(|request| request.url.path() == "/v1/images/generations")
                .map(|request| {
                    request
                        .headers
                        .get("idempotency-key")
                        .and_then(|value| value.to_str().ok())
                        .expect("derived cross-identity idempotency header")
                        .to_owned()
                })
                .collect::<Vec<_>>();
            assert_eq!(upstream_idempotencies.len(), 2);
            assert_ne!(upstream_idempotencies[0], upstream_idempotencies[1]);
            assert!(
                upstream_idempotencies
                    .iter()
                    .all(|value| value.starts_with("mtc-img-") && value != "openai-image-stable-1")
            );
            let route_id = world.image_route_id.expect("OpenAI image route id");
            let disabled = world
                .client
                .patch(format!(
                    "{}/internal/v1/model-routes/{route_id}",
                    world.service_url
                ))
                .bearer_auth("test-service-token")
                .json(&json!({
                    "tenant_external_id": "default",
                    "enabled": false,
                    "expected_updated_at": world.image_route_updated_at
                }))
                .send()
                .await
                .expect("disable OpenAI image route after completion");
            assert_eq!(disabled.status(), StatusCode::OK);
            let route_independent_replay = replay_openai_compatible_image(world).await;
            assert_eq!(route_independent_replay.status(), StatusCode::OK);
            assert_eq!(
                route_independent_replay
                    .json::<Value>()
                    .await
                    .expect("route-independent image replay"),
                world.response
            );
            let key_id = world.stable_key_id.expect("OpenAI image key id");
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
                .expect("suspend OpenAI image key after completion");
            assert_eq!(suspended.status(), StatusCode::OK);
            let rejected_replay = replay_openai_compatible_image(world).await;
            assert_eq!(rejected_replay.status(), StatusCode::UNAUTHORIZED);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("OpenAI image request was not metered");
}

#[then("the non-idempotent OpenAI image is atomically archived and costs 0.3")]
async fn non_idempotent_openai_image_is_atomic(world: &mut TokenCenterWorld) {
    assert_eq!(world.response["data"][0]["b64_json"], "bW9jay1wbmc=");
    let request_id = world
        .synchronous_request_id
        .expect("non-idempotent synchronous image request id");
    let detail = world
        .client
        .get(format!(
            "{}/self/v1/requests/{request_id}",
            world.service_url
        ))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("non-idempotent image detail")
        .json::<Value>()
        .await
        .expect("non-idempotent image detail JSON");
    assert_eq!(detail["status_code"], 200);
    assert_eq!(detail["cost"], "0.3");
    assert_eq!(detail["archive_complete"], true);
    let key = world
        .client
        .get(format!("{}/self/v1/key", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("non-idempotent image key")
        .json::<Value>()
        .await
        .expect("non-idempotent image key JSON");
    assert_eq!(key["available_balance"], "0");
    let upstream_requests = world
        .mock
        .as_ref()
        .expect("mock server")
        .received_requests()
        .await
        .expect("non-idempotent upstream image requests")
        .into_iter()
        .filter(|request| request.url.path() == "/v1/images/generations")
        .collect::<Vec<_>>();
    assert_eq!(upstream_requests.len(), 1);
    assert!(!upstream_requests[0].headers.contains_key("idempotency-key"));
}

#[then("the signed URL image is stored in CAS without exposing its secret URL")]
async fn openai_url_image_is_archived(world: &mut TokenCenterWorld) {
    assert!(!world.response.to_string().contains("must-not-leak"));
    assert!(!world.response.to_string().contains("SECRET_TOKEN"));
    let public_url = world.response["data"][0]["url"]
        .as_str()
        .expect("normalized MTC asset URL");
    assert!(public_url.starts_with("/self/v1/requests/"));
    assert!(!public_url.contains("must-not-leak"));
    let request_id = world
        .synchronous_request_id
        .expect("synchronous image request id");
    let replay = replay_openai_compatible_image(world).await;
    assert_eq!(replay.status(), StatusCode::OK);
    let expected_request_id = request_id.to_string();
    assert_eq!(
        replay
            .headers()
            .get("x-mtc-request-id")
            .and_then(|value| value.to_str().ok()),
        Some(expected_request_id.as_str())
    );
    assert_eq!(
        replay
            .json::<Value>()
            .await
            .expect("replayed URL image JSON"),
        world.response
    );
    let state = world.state.clone().expect("test application state");
    let assets = state
        .db
        .synchronous_generation_assets(request_id)
        .await
        .expect("synchronous image assets");
    assert_eq!(assets.len(), 1);
    let result_prefix = format!("staging/synchronous/{request_id}/result/");
    let result_suffix = assets[0]
        .object_locator
        .strip_prefix(&result_prefix)
        .expect("canonical synchronous result prefix");
    let (result_attempt, result_name) = result_suffix
        .split_once('/')
        .expect("attempt-scoped synchronous result");
    let result_attempt = Uuid::parse_str(result_attempt).expect("result attempt UUID");
    assert_eq!(result_name, "asset-0");
    assert_eq!(
        state
            .db
            .archive_staging_attempt(result_attempt)
            .await
            .expect("read synchronous result staging attempt")
            .expect("synchronous result staging attempt")
            .state,
        ArchiveStagingState::Bound
    );
    assert_eq!(assets[0].view.mime_type, "image/png");
    assert_eq!(assets[0].view.filename, "asset-0.png");
    assert_eq!(
        public_url,
        format!(
            "/self/v1/requests/{request_id}/assets/{}",
            assets[0].view.asset_id
        )
    );
    assert_eq!(
        state
            .archive
            .get(&assets[0].object_locator)
            .await
            .expect("read URL-backed image CAS"),
        bytes::Bytes::from_static(b"exact-url-png")
    );
    let response_refs = state
        .db
        .request_archive_refs(
            world.stable_key_id.expect("stable image key id"),
            request_id,
        )
        .await
        .expect("URL-backed image archive references");
    let stored_response = state
        .archive
        .get(
            response_refs
                .response_object
                .as_deref()
                .expect("stored normalized image response"),
        )
        .await
        .expect("read normalized image response CAS");
    assert!(!String::from_utf8_lossy(&stored_response).contains("must-not-leak"));
    let detail = world
        .client
        .get(format!(
            "{}/self/v1/requests/{request_id}",
            world.service_url
        ))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("URL-backed image request detail");
    assert_eq!(detail.status(), StatusCode::OK);
    let detail: Value = detail.json().await.expect("URL-backed image detail JSON");
    let rendered = detail["response_body"].to_string();
    assert!(!rendered.contains("must-not-leak"));
    assert!(!rendered.contains("objects/blake3/"));
    assert_eq!(
        detail["response_body"]["data"][0]["archived_asset"]["asset_id"],
        assets[0].view.asset_id.to_string()
    );
    let asset_url = format!(
        "{}/self/v1/requests/{request_id}/assets/{}",
        world.service_url, assets[0].view.asset_id
    );
    let download = world
        .client
        .get(&asset_url)
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("download synchronous image asset");
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(download.headers()["content-length"], "13");
    assert_eq!(
        download.bytes().await.expect("synchronous image bytes"),
        bytes::Bytes::from_static(b"exact-url-png")
    );
    let range = world
        .client
        .get(&asset_url)
        .bearer_auth(&world.current_key)
        .header("range", "bytes=6-8")
        .send()
        .await
        .expect("range synchronous image asset");
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.headers()["content-range"], "bytes 6-8/13");
    assert_eq!(
        range.bytes().await.expect("synchronous image range"),
        bytes::Bytes::from_static(b"url")
    );
    let invalid_range = world
        .client
        .get(&asset_url)
        .bearer_auth(&world.current_key)
        .header("range", "bytes=99-")
        .send()
        .await
        .expect("invalid synchronous image range");
    assert_eq!(invalid_range.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    let other = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "principal_external_id": "other-image-api-user",
            "alias": "other-image-api",
            "currency": "USD",
            "initial_balance": "0",
            "policy": {}
        }))
        .send()
        .await
        .expect("create other image key")
        .json::<Value>()
        .await
        .expect("other image key JSON");
    let hidden = world
        .client
        .get(&asset_url)
        .bearer_auth(other["key"].as_str().expect("other image secret"))
        .send()
        .await
        .expect("cross-key synchronous image download");
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
    let operator = world
        .client
        .get(format!(
            "{}/internal/v1/requests/{request_id}/assets/{}?tenant_external_id=default",
            world.service_url, assets[0].view.asset_id
        ))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("operator synchronous image download");
    assert_eq!(operator.status(), StatusCode::OK);
    assert_eq!(
        operator.bytes().await.expect("operator image bytes"),
        bytes::Bytes::from_static(b"exact-url-png")
    );
    let hidden_tenant = world
        .client
        .get(format!(
            "{}/internal/v1/requests/{request_id}/assets/{}?tenant_external_id=other-tenant",
            world.service_url, assets[0].view.asset_id
        ))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("cross-tenant synchronous image download");
    assert_eq!(hidden_tenant.status(), StatusCode::NOT_FOUND);
    let stats: Value = world
        .client
        .get(format!("{}/self/v1/stats", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("URL-backed image stats")
        .json()
        .await
        .expect("URL-backed image stats JSON");
    assert_eq!(stats["summary"]["total_requests"], 1);
    assert_eq!(stats["summary"]["total_cost"], "0.3");
}

#[then("the empty URL image is rejected unbilled without exposing the signed URL")]
async fn empty_openai_url_image_is_rejected(world: &mut TokenCenterWorld) {
    assert_eq!(world.status, Some(StatusCode::BAD_GATEWAY));
    assert_eq!(world.response["error"]["code"], "upstream_error");
    assert!(!world.response.to_string().contains("must-not-leak"));
    let request_id = world
        .synchronous_request_id
        .expect("empty URL image request id");
    let replay = replay_openai_compatible_image(world).await;
    assert_eq!(replay.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        replay
            .json::<Value>()
            .await
            .expect("empty URL image replay JSON"),
        world.response
    );
    let detail = world
        .client
        .get(format!(
            "{}/self/v1/requests/{request_id}",
            world.service_url
        ))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("empty URL image detail")
        .json::<Value>()
        .await
        .expect("empty URL image detail JSON");
    assert_eq!(detail["status_code"], 502);
    assert_eq!(detail["error_code"], "upstream_image_asset");
    assert_eq!(detail["cost"], "0");
    assert!(detail["response_body"].is_null());
    assert_eq!(detail["archive_complete"], false);
    assert!(!detail.to_string().contains("must-not-leak"));
    let state = world.state.clone().expect("test application state");
    assert!(
        state
            .db
            .synchronous_generation_assets(request_id)
            .await
            .expect("empty URL image assets")
            .is_empty()
    );
}

#[then("the ten image request is refunded and leaves no staged assets")]
async fn aggregate_openai_images_are_refunded_and_cleaned(world: &mut TokenCenterWorld) {
    assert_eq!(world.status, Some(StatusCode::BAD_GATEWAY));
    assert_eq!(world.response["error"]["code"], "upstream_error");
    let request_id = world
        .synchronous_request_id
        .expect("aggregate image request id");
    let state = world.state.clone().expect("test application state");
    assert!(
        state
            .db
            .synchronous_generation_assets(request_id)
            .await
            .expect("aggregate image DB assets")
            .is_empty()
    );
    let inspection = AnyPool::connect(&state.config.database_url)
        .await
        .expect("connect synchronous staging inspection pool");
    let attempt = sqlx::query(
        "SELECT attempt_id, state FROM archive_staging_attempts WHERE owner_kind = 'synchronous_request' AND owner_id = $1 AND purpose = 'result' ORDER BY created_at DESC LIMIT 1",
    )
    .bind(request_id.to_string())
    .fetch_one(&inspection)
    .await
    .expect("failed synchronous result staging attempt");
    assert_eq!(
        attempt.get::<String, _>("state"),
        "cleanup_pending",
        "only the durable reaper state may authorize deletion"
    );
    let attempt_id: String = attempt.get("attempt_id");
    let result_prefix = format!("staging/synchronous/{request_id}/result/{attempt_id}");
    for index in 0..9 {
        assert!(
            state
                .archive
                .get(&format!("{result_prefix}/asset-{index}"))
                .await
                .is_ok(),
            "partial asset {index} stays durable until the reaper cleans cleanup_pending"
        );
    }
    let detail = world
        .client
        .get(format!(
            "{}/self/v1/requests/{request_id}",
            world.service_url
        ))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("aggregate image request detail")
        .json::<Value>()
        .await
        .expect("aggregate image detail JSON");
    assert_eq!(detail["status_code"], 502);
    assert_eq!(detail["error_code"], "upstream_image_asset");
    assert_eq!(detail["cost"], "0");
    let key = world
        .client
        .get(format!("{}/self/v1/key", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("key after aggregate image failure")
        .json::<Value>()
        .await
        .expect("key after aggregate image failure JSON");
    assert_eq!(key["available_balance"], "3");
    let downloads = world
        .mock
        .as_ref()
        .expect("mock server")
        .received_requests()
        .await
        .expect("aggregate image upstream requests")
        .into_iter()
        .filter(|request| request.url.path().starts_with("/generated/aggregate-"))
        .count();
    assert_eq!(downloads, 10);
}

#[then("the oversized image is unbilled and has no partial response archive")]
async fn oversized_openai_image_is_unbilled(world: &mut TokenCenterWorld) {
    let request_id = world
        .synchronous_request_id
        .expect("oversized synchronous image request id");
    let replay = replay_openai_compatible_image(world).await;
    assert_eq!(replay.status(), StatusCode::BAD_GATEWAY);
    let expected_request_id = request_id.to_string();
    assert_eq!(
        replay
            .headers()
            .get("x-mtc-request-id")
            .and_then(|value| value.to_str().ok()),
        Some(expected_request_id.as_str())
    );
    let detail = world
        .client
        .get(format!(
            "{}/self/v1/requests/{request_id}",
            world.service_url
        ))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("oversized image detail");
    assert_eq!(detail.status(), StatusCode::OK);
    let detail: Value = detail.json().await.expect("oversized image detail JSON");
    assert_eq!(detail["status_code"], 502);
    assert_eq!(detail["cost"], "0");
    assert_eq!(detail["error_code"], "upstream_image_too_large");
    assert!(detail["response_body"].is_null());
    assert_eq!(detail["archive_complete"], false);
    let key: Value = world
        .client
        .get(format!("{}/self/v1/key", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("key after oversized image")
        .json()
        .await
        .expect("key after oversized image JSON");
    assert_eq!(key["available_balance"], "0.3");
}

#[then("the upstream image rejection is sanitized archived as a gap and replayed safely")]
async fn sensitive_openai_image_rejection_is_sanitized(world: &mut TokenCenterWorld) {
    assert_eq!(world.status, Some(StatusCode::BAD_GATEWAY));
    let rendered = world.response.to_string();
    assert!(!rendered.contains("must-not-leak"));
    assert_eq!(world.response["error"]["code"], "upstream_error");
    let request_id = world
        .synchronous_request_id
        .expect("rejected synchronous image request id");
    let replay = replay_openai_compatible_image(world).await;
    assert_eq!(replay.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        replay
            .json::<Value>()
            .await
            .expect("sanitized rejection replay"),
        world.response
    );
    let state = world.state.clone().expect("test application state");
    let refs = state
        .db
        .request_archive_refs(
            world.stable_key_id.expect("rejected image key id"),
            request_id,
        )
        .await
        .expect("rejected image archive references");
    let expected_gap = format!("gap://{request_id}/response");
    assert_eq!(refs.response_object.as_deref(), Some(expected_gap.as_str()));
    let detail = world
        .client
        .get(format!(
            "{}/self/v1/requests/{request_id}",
            world.service_url
        ))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("rejected image detail")
        .json::<Value>()
        .await
        .expect("rejected image detail JSON");
    assert_eq!(detail["status_code"], 502);
    assert_eq!(detail["error_code"], "upstream_http_400");
    assert_eq!(detail["cost"], "0");
    assert!(detail["response_body"].is_null());
    assert_eq!(detail["archive_complete"], false);
    assert!(!detail.to_string().contains("must-not-leak"));
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

#[given("the mock Codex Responses upstream returns a sensitive invalid image payload")]
async fn mock_sensitive_invalid_codex_image(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer codex-image-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_invalid_image_test",
            "output": [{
                "type": "image_generation_call",
                "id": "ig_invalid",
                "result": "",
                "debug_url": "https://images.example.invalid/result.png?token=must-not-leak"
            }]
        })))
        .expect(1)
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
            "protocol": "generation",
            "custom_model_confirmed": true
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
            "policy": {}
        }))
        .send()
        .await
        .expect("create Codex Responses Images key");
    assert_eq!(response.status(), StatusCode::CREATED);
    let key: Value = response.json().await.expect("Codex Images key JSON");
    world.current_key = key["key"].as_str().expect("Codex Images key").to_owned();
    world.stable_key_id = key["key_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok());
    grant_fixture_model(world, "default", &key, "codex-image-public", "generation").await;
}

#[when("the client creates a Codex-backed OpenAI-compatible image")]
async fn create_codex_backed_openai_image(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .post(format!("{}/v1/images/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .header("idempotency-key", "codex-image-stable-1")
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
    world.synchronous_request_id = response
        .headers()
        .get("x-mtc-request-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok());
    world.synchronous_response_content_length = response.content_length().map(|value| {
        usize::try_from(value).expect("Codex Images Content-Length fits the platform")
    });
    world.synchronous_response_body = response
        .bytes()
        .await
        .expect("Codex Images response body")
        .to_vec();
    world.response = serde_json::from_slice(&world.synchronous_response_body)
        .expect("Codex Images response JSON");
}

#[then("the Codex-backed image response is archived and costs 0.4")]
async fn codex_image_is_archived_and_metered(world: &mut TokenCenterWorld) {
    assert_eq!(
        world.response["data"][0]["b64_json"],
        "Y29kZXgtbW9jay1wbmc="
    );
    assert_eq!(
        world.synchronous_response_content_length,
        Some(world.synchronous_response_body.len())
    );
    let replay = world
        .client
        .post(format!("{}/v1/images/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .header("idempotency-key", "codex-image-stable-1")
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
        .expect("replay Codex-backed image");
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(
        replay
            .content_length()
            .and_then(|value| usize::try_from(value).ok()),
        Some(world.synchronous_response_body.len())
    );
    assert_eq!(
        replay
            .bytes()
            .await
            .expect("replayed Codex image body")
            .as_ref(),
        world.synchronous_response_body.as_slice()
    );
    let state = world.state.clone().expect("test application state");
    let archived_response = state
        .db
        .request_archive_refs(
            world.stable_key_id.expect("stable Codex image key id"),
            world
                .synchronous_request_id
                .expect("synchronous Codex image request id"),
        )
        .await
        .expect("Codex image archive references")
        .response_object
        .expect("Codex image response object");
    assert_eq!(
        state
            .archive
            .get(&archived_response)
            .await
            .expect("read Codex image response archive")
            .as_ref(),
        world.synchronous_response_body.as_slice()
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

#[then("the invalid Codex image payload is sanitized and never archived")]
async fn invalid_codex_image_is_sanitized(world: &mut TokenCenterWorld) {
    assert_eq!(world.status, Some(StatusCode::BAD_GATEWAY));
    assert_eq!(world.response["error"]["code"], "upstream_error");
    assert!(!world.response.to_string().contains("must-not-leak"));
    let request_id = world
        .synchronous_request_id
        .expect("invalid Codex image request id");
    let replay = world
        .client
        .post(format!("{}/v1/images/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .header("idempotency-key", "codex-image-stable-1")
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
        .expect("replay invalid Codex image");
    assert_eq!(replay.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        replay
            .json::<Value>()
            .await
            .expect("invalid Codex image replay JSON"),
        world.response
    );
    let detail = world
        .client
        .get(format!(
            "{}/self/v1/requests/{request_id}",
            world.service_url
        ))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("invalid Codex image detail")
        .json::<Value>()
        .await
        .expect("invalid Codex image detail JSON");
    assert_eq!(detail["status_code"], 502);
    assert_eq!(detail["error_code"], "upstream_image_invalid_payload");
    assert_eq!(detail["cost"], "0");
    assert!(detail["response_body"].is_null());
    assert_eq!(detail["archive_complete"], false);
    assert!(!detail.to_string().contains("must-not-leak"));
    let state = world.state.clone().expect("test application state");
    let refs = state
        .db
        .request_archive_refs(
            world.stable_key_id.expect("invalid Codex image key id"),
            request_id,
        )
        .await
        .expect("invalid Codex image archive references");
    let expected_gap = format!("gap://{request_id}/response");
    assert_eq!(refs.response_object.as_deref(), Some(expected_gap.as_str()));
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
            detail["response_body"]["assets"]
                .as_array()
                .is_some_and(|assets| !assets.is_empty())
        );
        assert!(!detail.to_string().contains("objects/blake3/"));
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
                "policy": {}
            }))
            .send()
            .await
            .expect("create tenant credential");
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: Value = response.json().await.expect("tenant credential JSON");
        grant_fixture_model(world, &tenant, &body, "global-stats-model", "openai").await;
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
            "policy": {}
        }))
        .send()
        .await
        .expect("create authorization-matrix downstream credential");
    assert_eq!(response.status(), StatusCode::CREATED);
    let issued = response
        .json::<Value>()
        .await
        .expect("authorization-matrix downstream credential JSON");
    grant_fixture_model(world, tenant, &issued, "matrix-model", "openai").await;
    issued["key"]
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
        let status = response.status();
        let _ = response
            .bytes()
            .await
            .expect("consume authorization-matrix model response");
        assert_eq!(status, StatusCode::OK);
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
) -> Uuid {
    let response = world
        .client
        .post(format!("{}/internal/v1/model-routes", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "public_model": public_model,
            "upstream_account_id": upstream_account_id,
            "upstream_model": upstream_model,
            "protocol": "openai",
            "custom_model_confirmed": true
        }))
        .send()
        .await
        .expect("create model route");
    let status = response.status();
    let body: Value = response.json().await.expect("model route JSON");
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Uuid::parse_str(body["id"].as_str().expect("model route id")).expect("model route UUID")
}

async fn ensure_fixture_route(
    world: &TokenCenterWorld,
    tenant_external_id: &str,
    public_model: &str,
    protocol: &str,
) -> Uuid {
    let state = world.state.as_ref().expect("state");
    if let Some(route) = state
        .db
        .list_model_routes(Some(tenant_external_id))
        .await
        .expect("list fixture routes")
        .into_iter()
        .find(|route| route.public_model == public_model && route.protocol == protocol)
    {
        return route.id;
    }

    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let account = world
        .client
        .post(format!("{}/internal/v1/upstreams", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "tenant_external_id": tenant_external_id,
            "name": format!("fixture-{public_model}-{}", Uuid::now_v7()),
            "driver": "http-json",
            "config": {"base_url": mock_url},
            "credential": {"type": "none"}
        }))
        .send()
        .await
        .expect("create fixture upstream account");
    let status = account.status();
    let account: Value = account.json().await.expect("fixture upstream account JSON");
    assert_eq!(status, StatusCode::CREATED, "{account}");

    let route = world
        .client
        .post(format!("{}/internal/v1/model-routes", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "tenant_external_id": tenant_external_id,
            "public_model": public_model,
            "upstream_account_id": account["id"],
            "upstream_model": public_model,
            "protocol": protocol,
            "custom_model_confirmed": true
        }))
        .send()
        .await
        .expect("create fixture model route");
    let status = route.status();
    let route: Value = route.json().await.expect("fixture model route JSON");
    assert_eq!(status, StatusCode::CREATED, "{route}");
    Uuid::parse_str(route["id"].as_str().expect("fixture route id")).expect("fixture route UUID")
}

async fn grant_fixture_routes(
    world: &TokenCenterWorld,
    tenant_external_id: &str,
    key_id: Uuid,
    route_ids: &[Uuid],
) {
    let current = world
        .client
        .get(format!(
            "{}/internal/v1/keys/{key_id}/routing?tenant_external_id={tenant_external_id}",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("read fixture routing revision");
    let current_status = current.status();
    let current: Value = current.json().await.expect("fixture routing view JSON");
    assert_eq!(current_status, StatusCode::OK, "{current}");
    let expected_grant_revision = current["grant_revision"]
        .as_i64()
        .expect("fixture routing grant revision");
    let mut merged_route_ids = current["route_ids"]
        .as_array()
        .expect("fixture routing route ids")
        .iter()
        .map(|value| {
            Uuid::parse_str(value.as_str().expect("fixture routing route id"))
                .expect("fixture routing route UUID")
        })
        .chain(route_ids.iter().copied())
        .collect::<Vec<_>>();
    merged_route_ids.sort_unstable();
    merged_route_ids.dedup();
    let route_group_ids = current["route_group_ids"]
        .as_array()
        .expect("fixture routing route group ids");
    let response = world
        .client
        .put(format!(
            "{}/internal/v1/keys/{key_id}/routing",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({
            "tenant_external_id": tenant_external_id,
            "route_ids": merged_route_ids,
            "route_group_ids": route_group_ids,
            "expected_grant_revision": expected_grant_revision
        }))
        .send()
        .await
        .expect("grant fixture routes");
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .expect("fixture routing response JSON");
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn grant_fixture_model(
    world: &TokenCenterWorld,
    tenant_external_id: &str,
    issued_key: &Value,
    public_model: &str,
    protocol: &str,
) {
    let key_id = Uuid::parse_str(
        issued_key["key_id"]
            .as_str()
            .expect("fixture issued key id"),
    )
    .expect("fixture issued key UUID");
    let route_id = ensure_fixture_route(world, tenant_external_id, public_model, protocol).await;
    grant_fixture_routes(world, tenant_external_id, key_id, &[route_id]).await;
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
            "policy": {}
        }))
        .send()
        .await
        .expect("create routed key");
    assert_eq!(response.status(), StatusCode::CREATED);
    let value: Value = response.json().await.expect("routed key JSON");
    world.current_key = value["key"].as_str().expect("issued key").to_owned();
    let api_route = ensure_fixture_route(world, "default", "api-public", "openai").await;
    let oauth_route = ensure_fixture_route(world, "default", "oauth-public", "openai").await;
    grant_fixture_routes(
        world,
        "default",
        Uuid::parse_str(value["key_id"].as_str().expect("routed key id")).expect("routed key UUID"),
        &[api_route, oauth_route],
    )
    .await;
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
            "refreshToken": "cursor-refresh-1",
            "accountId": "cursor-stable-account-1"
        })))
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    Mock::given(method("POST"))
        .and(path("/cursor/auth/exchange_user_api_key"))
        .and(header("authorization", "Bearer cursor-refresh-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accessToken": "cursor-access-2",
            "accountId": "cursor-stable-account-1"
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
    assert!(value["config"].get("oauth").is_none());
    assert_eq!(value["can_refresh"], true);
    assert!(!value.to_string().contains("cursor-access-1"));
    assert!(!value.to_string().contains("cursor-refresh-1"));
    world.cursor_account_id = Some(
        Uuid::from_str(value["id"].as_str().expect("Cursor account id"))
            .expect("Cursor account UUID"),
    );
    world.cursor_generation = value["credential_generation"]
        .as_i64()
        .expect("Cursor generation");
    let retry = poll().await.expect("retry completed Cursor OAuth poll");
    assert_eq!(retry.status(), StatusCode::OK);
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
    assert!(value.get("credential").is_none());
    assert!(!value.to_string().contains("cursor-access-2"));
    assert!(!value.to_string().contains("cursor-refresh-1"));
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

#[when("the service starts reauthorization for the Cursor OAuth account")]
async fn start_cursor_oauth_reauthorization(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let account_id = world.cursor_account_id.expect("Cursor account id");
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
            "upstream_account_id": account_id,
            "endpoints": {
                "login_url": format!("{mock_url}/cursor/loginDeepControl"),
                "poll_url": format!("{mock_url}/cursor/auth/poll"),
                "refresh_url": format!("{mock_url}/cursor/auth/exchange_user_api_key")
            }
        }))
        .send()
        .await
        .expect("start Cursor OAuth reauthorization");
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response
        .json()
        .await
        .expect("Cursor OAuth reauthorization start JSON");
    world.cursor_session_token = value["session_token"]
        .as_str()
        .expect("Cursor reauthorization session token")
        .to_owned();
    assert!(
        !world
            .cursor_session_token
            .contains(account_id.to_string().as_str())
    );
}

#[when("the service polls the completed Cursor OAuth reauthorization")]
async fn poll_cursor_oauth_reauthorization(world: &mut TokenCenterWorld) {
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
    let response = poll().await.expect("poll Cursor OAuth reauthorization");
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response
        .json()
        .await
        .expect("reauthorized Cursor account JSON");
    assert_eq!(
        value["id"],
        world
            .cursor_account_id
            .expect("Cursor account id")
            .to_string()
    );
    assert_eq!(value["credential_generation"], 3);
    assert_eq!(value["route_count"], 1);
    assert_eq!(value["can_reauthorize"], true);
    assert!(value.get("credential").is_none());
    world.cursor_generation = 3;

    let replay = poll()
        .await
        .expect("replay Cursor OAuth reauthorization poll");
    assert_eq!(replay.status(), StatusCode::OK);
    let replay: Value = replay.json().await.expect("reauthorization replay JSON");
    assert_eq!(replay["credential_generation"], 3);
}

#[then("the reauthorized Cursor account keeps its id and route and uses generation 3")]
async fn reauthorized_cursor_account_is_stable(world: &mut TokenCenterWorld) {
    assert!(world.cursor_account_id.is_some());
    assert_eq!(world.cursor_generation, 3);
    let state = world.state.as_ref().expect("state");
    let routes = state
        .db
        .list_model_routes(Some("default"))
        .await
        .expect("routes");
    let cursor_routes = routes
        .iter()
        .filter(|route| route.upstream_account_id == world.cursor_account_id.unwrap())
        .count();
    assert_eq!(cursor_routes, 1);
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
            "policy": {}
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
    let protocol = if model.starts_with("claude") {
        "anthropic"
    } else {
        "openai"
    };
    let route_id = ensure_fixture_route(world, "default", &model, protocol).await;
    grant_fixture_routes(
        world,
        "default",
        world.stable_key_id.expect("stable key id"),
        &[route_id],
    )
    .await;
}

#[when(expr = "the service renames the key alias to {string}")]
async fn rename_key_alias(world: &mut TokenCenterWorld, alias: String) {
    let key_id = world.stable_key_id.expect("stable key id");
    let response = world
        .client
        .patch(format!(
            "{}/internal/v1/keys/{key_id}/alias",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"alias": alias}))
        .send()
        .await
        .expect("rename key alias");
    world.status = Some(response.status());
    world.response = response.json().await.expect("renamed key alias JSON");
}

#[then("the renamed alias retains the stable key identity")]
async fn renamed_alias_retains_identity(world: &mut TokenCenterWorld) {
    let key_id = world.stable_key_id.expect("stable key id");
    assert_eq!(world.status, Some(StatusCode::OK));
    assert_eq!(world.response["key_id"], key_id.to_string());
    assert_eq!(world.response["alias"], "renamed credential");
    let response = world
        .client
        .get(format!("{}/self/v1/key", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("read renamed key with unchanged credential");
    assert_eq!(response.status(), StatusCode::OK);
    let key: Value = response.json().await.expect("renamed key JSON");
    assert_eq!(key["key_id"], key_id.to_string());
    assert_eq!(key["alias"], "renamed credential");
    assert_eq!(key["credential_generation"], 1);
}

#[when("the client views its own limit snapshot")]
async fn view_own_limit_snapshot(world: &mut TokenCenterWorld) {
    let response = world
        .client
        .get(format!("{}/self/v1/key/limits", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("read own key limit snapshot");
    world.status = Some(response.status());
    world.response = response.json().await.expect("own key limit snapshot JSON");
}

#[then("the own limit snapshot belongs to the stable key")]
async fn own_limit_snapshot_is_stably_bound(world: &mut TokenCenterWorld) {
    assert_eq!(world.status, Some(StatusCode::OK));
    assert_eq!(
        world.response["key_id"],
        world.stable_key_id.expect("stable key id").to_string()
    );
    assert_eq!(world.response["currency"], "USD");
    assert_eq!(world.response["rpm"]["used"], 0);
    assert_eq!(world.response["tpm"]["used"], 0);
    assert_eq!(world.response["concurrency"]["active"], 0);
    assert_eq!(world.response["daily_budget"]["settled"], "0");
    assert_eq!(world.response["weekly_budget"]["settled"], "0");
    assert_eq!(world.response["lifetime_budget"]["settled"], "0");
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
        "requests_per_minute": 7,
        "tokens_per_minute": 7000,
        "max_concurrency": 2,
        "enforcement_mode": "prepaid",
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
    grant_fixture_model(world, "continuity-tenant", &issued, "gpt-test", "openai").await;

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

#[when("the service installs an imported opaque CPA key")]
async fn install_imported_opaque_cpa_key(world: &mut TokenCenterWorld) {
    let opaque = "sk-cpa-linux-codex-unchanged-credential-1234567890";
    let key_id = world.stable_key_id.expect("stable key id");
    let state = world.state.as_ref().expect("token center state");
    let (secret_hash, fingerprint) =
        crypto::hash_credential(opaque, state.config.key_pepper.as_bytes());
    let pool = AnyPool::connect(&state.config.database_url)
        .await
        .expect("connect imported credential fixture");
    let updated = sqlx::query(
        "UPDATE key_credentials SET secret_hash=$1,fingerprint=$2 WHERE key_id=$3 AND generation=1 AND revoked_at IS NULL",
    )
    .bind(secret_hash)
    .bind(fingerprint)
    .bind(key_id.to_string())
    .execute(&pool)
    .await
    .expect("install imported opaque credential");
    assert_eq!(updated.rows_affected(), 1);
    world.current_key = opaque.to_owned();
}

#[when("the client views statistics with the imported opaque CPA key")]
async fn view_stats_with_imported_opaque_cpa_key(world: &mut TokenCenterWorld) {
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
            "policy": {}
        }))
        .send()
        .await
        .expect("create exhausted key");
    world.response = response.json().await.expect("create key JSON");
    world.current_key = world.response["key"]
        .as_str()
        .expect("issued key")
        .to_owned();
    grant_fixture_model(world, "default", &world.response, &model, "openai").await;
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
    grant_fixture_model(world, "default", &world.response, &model, "openai").await;
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
    world.response_retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
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
    let _ = first
        .bytes()
        .await
        .expect("first full-context response body");
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
    let _ = second
        .bytes()
        .await
        .expect("second full-context response body");
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
    let _ = first.bytes().await.expect("explicit parent response body");

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
    let _ = second.bytes().await.expect("compacted child response body");
}

#[then(expr = "the response status is {int}")]
async fn response_status(world: &mut TokenCenterWorld, expected: u16) {
    assert_eq!(
        world.status,
        Some(StatusCode::from_u16(expected).expect("valid HTTP status"))
    );
}

#[then(expr = "the rejection reason is {string} and is not retryable")]
async fn rejection_is_permanent(world: &mut TokenCenterWorld, reason: String) {
    assert_eq!(world.response["error"]["reason"], reason);
    assert_eq!(world.response["error"]["retryable"], false);
    assert!(world.response_retry_after.is_none());
}

#[then(expr = "the rejection reason is {string} and is retryable with Retry-After")]
async fn rejection_is_retryable(world: &mut TokenCenterWorld, reason: String) {
    assert_eq!(world.response["error"]["reason"], reason);
    assert_eq!(world.response["error"]["retryable"], true);
    let retry_after = world
        .response_retry_after
        .as_deref()
        .expect("Retry-After header");
    assert!(retry_after.parse::<u64>().is_ok_and(|seconds| seconds >= 1));
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
    let request_uuid = Uuid::parse_str(request_id).expect("request UUID");
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
    let state = world.state.as_ref().expect("test application state");
    let refs = state
        .db
        .request_archive_refs(world.stable_key_id.expect("key id"), request_uuid)
        .await
        .expect("proxy archive references");
    let request_prefix = format!("staging/proxy/{request_id}/request/");
    let response_prefix = format!("staging/proxy/{request_id}/response/");
    assert!(refs.request_object.starts_with(&request_prefix));
    assert!(refs.request_object.ends_with("/body"));
    let response_object = refs.response_object.expect("response object");
    assert!(response_object.starts_with(&response_prefix));
    assert!(response_object.ends_with("/body"));
    for locator in [&refs.request_object, &response_object] {
        let attempt_id = locator
            .strip_suffix("/body")
            .and_then(|prefix| prefix.rsplit('/').next())
            .and_then(|value| Uuid::parse_str(value).ok())
            .expect("canonical proxy attempt UUID");
        assert_eq!(
            state
                .db
                .archive_staging_attempt(attempt_id)
                .await
                .expect("staging attempt query")
                .expect("staging attempt")
                .state,
            ArchiveStagingState::Bound
        );
    }
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

fn cpamp_importer_output(world: &TokenCenterWorld) -> std::process::Output {
    let sqlite_path = world
        .import_sqlite_path
        .as_ref()
        .expect("CPAMP SQLite fixture path");
    let mut command = Command::new("node");
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("ops/migrate-cpamp.ts"))
        .envs(postgres_command_environment(&world.import_database_url))
        .env("CPAMP_SQLITE_PATH", sqlite_path)
        .env("IMPORT_TENANT_EXTERNAL_ID", &world.import_tenant)
        .env("CPAMP_IMPORT_SOURCE", &world.import_source)
        .env("CPAMP_OVERLAP_MS", "86400000")
        .env("CPAMP_RESET_IMPORT", "false");
    command.output().expect("execute CPAMP importer")
}

fn run_cpamp_importer(world: &TokenCenterWorld) {
    let output = cpamp_importer_output(world);
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

    // Model prices are global rather than tenant-scoped. Keep this reserved
    // fixture model isolated across local retries of the same PostgreSQL gate.
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect for CPAMP fixture price cleanup");
    sqlx::query("DELETE FROM model_price_tiers WHERE model = 'fixture-model'")
        .execute(&pool)
        .await
        .expect("clear prior CPAMP fixture price tiers");
    sqlx::query("DELETE FROM model_prices WHERE model = 'fixture-model'")
        .execute(&pool)
        .await
        .expect("clear prior CPAMP fixture base price");
    pool.close().await;

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
        "SELECT COUNT(*) AS requests, COUNT(DISTINCT reservation_id) AS distinct_events, COALESCE(SUM(input_tokens), 0)::BIGINT AS input_tokens, COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens, COALESCE(SUM(cost_micros), 0)::BIGINT AS cost_micros FROM request_records WHERE tenant_id = $1",
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
        "SELECT COALESCE(SUM(a.requests), 0)::BIGINT AS requests, COALESCE(SUM(a.input_tokens), 0)::BIGINT AS input_tokens, COALESCE(SUM(a.output_tokens), 0)::BIGINT AS output_tokens, COALESCE(SUM(a.cost_micros), 0)::BIGINT AS cost_micros FROM usage_daily_aggregates a JOIN key_records k ON k.id = a.key_id WHERE k.tenant_id = $1",
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
    let compact_stats = sqlx::query(
        "SELECT (SELECT COUNT(*) FROM request_stats_facts WHERE tenant_id = $1) AS facts, COALESCE((SELECT SUM(requests) FROM request_daily_aggregates WHERE tenant_id = $2), 0)::BIGINT AS aggregate_requests",
    )
    .bind(&tenant_id)
    .bind(&tenant_id)
    .fetch_one(&pool)
    .await
    .expect("compact request statistics");
    assert_eq!(compact_stats.get::<i64, _>("facts"), expected_requests);
    assert_eq!(
        compact_stats.get::<i64, _>("aggregate_requests"),
        expected_requests
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
        !identity
            .get::<String, _>("policy_json")
            .contains("allowed_models")
    );
    let global_price_rows: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM model_prices WHERE model = 'fixture-model') + (SELECT COUNT(*) FROM model_price_tiers WHERE model = 'fixture-model')",
    )
    .fetch_one(&pool)
    .await
    .expect("CPAMP source prices remain provenance-only");
    assert_eq!(
        global_price_rows, 0,
        "an import must not overwrite the operator-managed global price catalog"
    );
    pool.close().await;
}

#[then("the imported requests aggregates and checkpoint contain exactly the initial events")]
async fn initial_cpamp_import_is_exact(world: &mut TokenCenterWorld) {
    assert_cpamp_import_state(world, 2, 28, 8, 88, 300_000_000, "fixture-event-initial-b").await;

    let pool = PgPool::connect(&world.import_database_url)
        .await
        .expect("connect for legacy CPAMP marker reconciliation");
    let initial_gaps: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_records r JOIN tenants t ON t.id = r.tenant_id WHERE t.external_id = $1 AND r.request_object LIKE 'gap://%' AND r.response_object LIKE 'gap://%'",
    )
    .bind(&world.import_tenant)
    .fetch_one(&pool)
    .await
    .expect("new CPAMP rows use archive gaps");
    assert_eq!(initial_gaps, 2);
    sqlx::query(
        "UPDATE request_records r SET response_object = CASE WHEN r.error_code = 'http_502' THEN 'inline-json:' || jsonb_build_object('source','cpamp','error','fixture upstream failure')::text ELSE 'inline-json:{\"source\":\"protected-real-body\"}' END FROM tenants t WHERE t.id = r.tenant_id AND t.external_id = $1",
    )
    .bind(&world.import_tenant)
    .execute(&pool)
    .await
    .expect("install legacy and protected response markers");
    pool.close().await;
    run_cpamp_importer(world);
    let pool = PgPool::connect(&world.import_database_url)
        .await
        .expect("verify legacy CPAMP marker reconciliation");
    let reconciled = sqlx::query(
        "SELECT COUNT(*) FILTER (WHERE r.error_code = 'http_502' AND r.response_object = 'gap://cpamp/fixture-event-initial-b/response') AS normalized, COUNT(*) FILTER (WHERE r.error_code IS NULL AND r.response_object = 'inline-json:{\"source\":\"protected-real-body\"}') AS protected FROM request_records r JOIN tenants t ON t.id = r.tenant_id WHERE t.external_id = $1",
    )
    .bind(&world.import_tenant)
    .fetch_one(&pool)
    .await
    .expect("reconciled response markers");
    assert_eq!(reconciled.get::<i64, _>("normalized"), 1);
    assert_eq!(reconciled.get::<i64, _>("protected"), 1);
    sqlx::query(
        "UPDATE request_records r SET response_object = 'cas://fixture/archive-response' FROM tenants t WHERE t.id = r.tenant_id AND t.external_id = $1 AND r.error_code = 'http_502'",
    )
    .bind(&world.import_tenant)
    .execute(&pool)
    .await
    .expect("emulate archive replacement");
    pool.close().await;
    run_cpamp_importer(world);
    let pool = PgPool::connect(&world.import_database_url)
        .await
        .expect("verify archived response remains protected");
    let archived: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_records r JOIN tenants t ON t.id = r.tenant_id WHERE t.external_id = $1 AND r.response_object = 'cas://fixture/archive-response'",
    )
    .bind(&world.import_tenant)
    .fetch_one(&pool)
    .await
    .expect("archived response locator");
    assert_eq!(archived, 1);
    pool.close().await;
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
    let sqlite_path = world
        .import_sqlite_path
        .as_ref()
        .expect("CPAMP SQLite fixture path");
    let status = Command::new("sqlite3")
        .arg(sqlite_path)
        .arg("UPDATE model_prices SET prompt_per_1m = 3.0, completion_per_1m = 5.0, updated_at_ms = 500000000 WHERE model = 'fixture-model';")
        .status()
        .expect("update CPAMP source price");
    assert!(status.success());
    run_cpamp_importer(world);
    run_cpamp_importer(world);

    let pool = PgPool::connect(&world.import_database_url)
        .await
        .expect("verify source price isolation");
    let global_price_rows: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM model_prices WHERE model = 'fixture-model') + (SELECT COUNT(*) FROM model_price_tiers WHERE model = 'fixture-model')",
    )
    .fetch_one(&pool)
    .await
    .expect("updated CPAMP source prices remain provenance-only");
    assert_eq!(
        global_price_rows, 0,
        "source price updates must not create operator-managed catalog rows"
    );
    pool.close().await;

    let status = Command::new("sqlite3")
        .arg(sqlite_path)
        .arg("INSERT INTO usage_events SELECT * FROM usage_events WHERE event_hash = 'fixture-event-new-watermark' LIMIT 1;")
        .status()
        .expect("append exact duplicate CPAMP event");
    assert!(status.success());
    run_cpamp_importer(world);
    let status = Command::new("sqlite3")
        .arg(sqlite_path)
        .arg("INSERT INTO usage_events (event_hash, request_id, timestamp_ms, provider, model, endpoint, api_key_hash, input_tokens, output_tokens, latency_ms, failed, fail_status_code, fail_summary) SELECT event_hash, request_id, timestamp_ms, provider, model, endpoint, api_key_hash, 999, output_tokens, latency_ms, failed, fail_status_code, fail_summary FROM usage_events WHERE event_hash = 'fixture-event-new-watermark' LIMIT 1;")
        .status()
        .expect("append conflicting duplicate CPAMP event");
    assert!(status.success());
    let conflict = cpamp_importer_output(world);
    assert!(!conflict.status.success());
    assert!(
        String::from_utf8_lossy(&conflict.stderr)
            .contains("event hashes map to conflicting source rows")
    );
    let status = Command::new("sqlite3")
        .arg(sqlite_path)
        .arg("DELETE FROM usage_events WHERE event_hash = 'fixture-event-new-watermark' AND input_tokens = 999; DELETE FROM usage_events WHERE rowid NOT IN (SELECT min(rowid) FROM usage_events GROUP BY event_hash);")
        .status()
        .expect("remove duplicate CPAMP fixtures");
    assert!(status.success());
    let status = Command::new("sqlite3")
        .arg(sqlite_path)
        .arg("INSERT INTO usage_events (event_hash, request_id, timestamp_ms, provider, model, endpoint, api_key_hash, input_tokens, output_tokens, latency_ms, failed, fail_status_code, fail_summary) VALUES ('fixture-invalid-hash', 'invalid-hash-request', 450000000, 'openai', 'fixture-model', '/v1/responses', 'zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz', 1, 1, 1, 0, NULL, NULL);")
        .status()
        .expect("append invalid CPAMP key hash");
    assert!(status.success());
    let invalid = cpamp_importer_output(world);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("no supported key identity"));
    let status = Command::new("sqlite3")
        .arg(sqlite_path)
        .arg("DELETE FROM usage_events WHERE event_hash = 'fixture-invalid-hash'; UPDATE usage_events SET input_tokens = input_tokens + 1 WHERE event_hash = 'fixture-event-new-watermark';")
        .status()
        .expect("mutate an already imported CPAMP event");
    assert!(status.success());
    let drift = cpamp_importer_output(world);
    assert!(!drift.status.success());
    let pool = PgPool::connect(&world.import_database_url)
        .await
        .expect("verify CPAMP conflict failures leave no target writes");
    let stable = sqlx::query(
        "SELECT (SELECT COUNT(*) FROM request_records r JOIN tenants t ON t.id = r.tenant_id WHERE t.external_id = $1) AS requests, (SELECT COUNT(*) FROM import_request_links l JOIN tenants t ON t.id = l.tenant_id WHERE t.external_id = $2 AND l.source_digest <> '') AS digested_links, (SELECT imported_events FROM cpamp_import_checkpoints WHERE tenant_external_id = $3 AND source = $4) AS imported_events",
    )
    .bind(&world.import_tenant)
    .bind(&world.import_tenant)
    .bind(&world.import_tenant)
    .bind(&world.import_source)
    .fetch_one(&pool)
    .await
    .expect("stable CPAMP target after rejected inputs");
    assert_eq!(stable.get::<i64, _>("requests"), 4);
    assert_eq!(stable.get::<i64, _>("digested_links"), 4);
    assert_eq!(stable.get::<i64, _>("imported_events"), 4);
    pool.close().await;
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
    let mut parent_stream = String::with_capacity(2 * 1024 * 1024 + 128 * 1024);
    parent_stream.push_str(concat!(
        "event: response.created\r\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-streaming-parent\",\"object\":\"response\"}}\r\n",
        "\r\n"
    ));
    let delta = "x".repeat(4 * 1024);
    for _ in 0..520 {
        parent_stream.push_str("event: response.output_text.delta\n");
        parent_stream.push_str("data: {\"type\":\"response.output_text.delta\",\"delta\":\"");
        parent_stream.push_str(&delta);
        parent_stream.push_str("\"}\n\n");
    }
    parent_stream.push_str(concat!(
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-streaming-parent\",\"usage\":{\"input_tokens\":4,\"output_tokens\":2,\"total_tokens\":6}}}\n\n",
        "data: [DONE]\n\n"
    ));
    assert!(
        parent_stream.len() > 2 * 1024 * 1024,
        "the streaming fixture must exceed the legacy usage-tail capture"
    );
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(
            json!({"input": "streaming parent", "stream": true}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(parent_stream, "text/event-stream"))
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
                .set_body_raw(
                    concat!(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-streaming-child\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-streaming-child\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
                    "data: [DONE]\n\n"
                    ),
                    "text/event-stream",
                ),
        )
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[given("the mock failed streaming Responses upstream returns parent and child events")]
async fn mock_failed_streaming_responses_parent_and_child(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(
            json!({"input": "failed streaming parent", "stream": true}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(
                    concat!(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-failed-parent\"}}\n\n",
                    "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp-failed-parent\"}}\n\n",
                    "data: [DONE]\n\n"
                    ),
                    "text/event-stream",
                ),
        )
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(json!({
            "input": "failed streaming child",
            "previous_response_id": "resp-failed-parent",
            "stream": true
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(
                    concat!(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-after-failure\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-after-failure\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n"
                    ),
                    "text/event-stream",
                ),
        )
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
}

#[given("the mock streaming Responses upstream exceeds the admitted usage")]
async fn mock_streaming_responses_invalid_usage(world: &mut TokenCenterWorld) {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(json!({
            "input": "invalid usage stream",
            "stream": true,
            "max_output_tokens": 2
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(
                    concat!(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-invalid-usage\"}}\n\n",
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"delivered output\"}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-invalid-usage\",\"usage\":{\"input_tokens\":1,\"output_tokens\":999}}}\n\n",
                    "data: [DONE]\n\n"
                    ),
                    "text/event-stream",
                ),
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

#[when(expr = "the Responses client sends a failed streaming parent and child for model {string}")]
async fn send_failed_streaming_responses_parent_and_child(
    world: &mut TokenCenterWorld,
    model: String,
) {
    assert_eq!(
        send_responses_turn(world, &model, "failed streaming parent", None, true).await,
        StatusCode::OK
    );
    world.status = Some(
        send_responses_turn(
            world,
            &model,
            "failed streaming child",
            Some("resp-failed-parent"),
            true,
        )
        .await,
    );
}

#[when(expr = "the Responses client consumes the invalid usage stream for model {string}")]
async fn consume_streaming_responses_invalid_usage(world: &mut TokenCenterWorld, model: String) {
    let response = world
        .client
        .post(format!("{}/v1/responses", world.service_url))
        .bearer_auth(&world.current_key)
        .json(&json!({
            "model": model,
            "input": "invalid usage stream",
            "stream": true,
            "max_output_tokens": 2
        }))
        .send()
        .await
        .expect("invalid usage Responses request");
    world.status = Some(response.status());
    let body = response.text().await.expect("invalid usage Responses body");
    assert!(body.contains("delivered output"), "{body}");
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

#[then("the failed Responses id does not form a continuation edge")]
async fn failed_responses_id_is_not_a_parent(world: &mut TokenCenterWorld) {
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
    let clusters = clusters.as_array().expect("conversation cluster array");
    assert_eq!(clusters.len(), 2, "{clusters:?}");
    assert!(
        clusters.iter().all(|cluster| cluster["request_count"] == 1),
        "{clusters:?}"
    );
    let requests = world
        .client
        .get(format!("{}/self/v1/requests", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("failed Responses request history")
        .json::<Value>()
        .await
        .expect("failed Responses request history JSON");
    let failed = requests
        .as_array()
        .expect("request history array")
        .iter()
        .find(|request| request["error_code"] == "upstream_failed_response")
        .expect("HTTP 200 response.failed terminal record");
    assert_eq!(failed["status_code"], 502, "{failed}");
    assert_ne!(failed["cost"], "0", "delivered failed streams are billed");
    assert_eq!(failed["output_tokens"], 4096, "{failed}");

    let stats = world
        .client
        .get(format!("{}/self/v1/stats", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("failed Responses statistics")
        .json::<Value>()
        .await
        .expect("failed Responses statistics JSON");
    assert_eq!(stats["summary"]["failed_requests"], 1, "{stats}");
    assert!(
        stats["errors"]
            .as_array()
            .is_some_and(|errors| errors.iter().any(|bucket| {
                bucket["name"] == "upstream_failed_response" && bucket["requests"] == 1
            })),
        "{stats}"
    );
}

#[then("the delivered invalid stream is a fully billed failure without response lineage")]
async fn invalid_stream_is_billed_failure(world: &mut TokenCenterWorld) {
    assert_eq!(world.status, Some(StatusCode::OK));
    let requests = world
        .client
        .get(format!("{}/self/v1/requests", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("invalid stream request history")
        .json::<Value>()
        .await
        .expect("invalid stream request history JSON");
    assert_eq!(requests.as_array().map(Vec::len), Some(1), "{requests}");
    assert_eq!(requests[0]["status_code"], 502, "{requests}");
    assert_eq!(
        requests[0]["error_code"], "upstream_invalid_usage",
        "{requests}"
    );
    assert_eq!(requests[0]["output_tokens"], 2, "{requests}");
    assert_ne!(requests[0]["cost"], "0", "{requests}");

    let detail = own_conversation_detail(world).await;
    assert_eq!(detail["cluster"]["request_count"], 1, "{detail}");
    assert_eq!(
        detail["edges"].as_array().map(Vec::len),
        Some(0),
        "{detail}"
    );
    let stats = world
        .client
        .get(format!("{}/self/v1/stats", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("invalid stream stats")
        .json::<Value>()
        .await
        .expect("invalid stream stats JSON");
    assert_eq!(stats["summary"]["failed_requests"], 1, "{stats}");
    assert_eq!(stats["summary"]["output_tokens"], 2, "{stats}");
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

async fn send_subagent_chat(
    world: &TokenCenterWorld,
    model: &str,
    content: &str,
    headers: &[(&str, &str)],
    metadata: Option<Value>,
) -> StatusCode {
    let mut request = world
        .client
        .post(format!("{}/v1/chat/completions", world.service_url))
        .bearer_auth(&world.current_key);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut body = json!({
        "model": model,
        "messages": [{"role": "user", "content": content}]
    });
    if let Some(metadata) = metadata {
        body["metadata"] = metadata;
    }
    let response = request
        .json(&body)
        .send()
        .await
        .expect("subagent gateway request");
    let status = response.status();
    let _ = response.bytes().await.expect("subagent gateway response");
    status
}

#[when(
    expr = "the client sends a parent with header-marked and body-marked subagents for model {string}"
)]
async fn client_sends_explicit_subagent_turns(world: &mut TokenCenterWorld, model: String) {
    assert_eq!(
        send_subagent_chat(
            world,
            &model,
            "gateway parent",
            &[
                ("x-mtc-conversation-id", "gateway-subagent-session"),
                ("x-mtc-turn-id", "gateway-parent")
            ],
            None,
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        send_subagent_chat(
            world,
            &model,
            "header child",
            &[
                ("x-mtc-conversation-id", "gateway-subagent-session"),
                ("x-mtc-parent-turn-id", "gateway-parent"),
                ("x-mtc-subagent", "true"),
                // A branch hint must not override the explicit subagent relation.
                ("x-mtc-branch-id", "worker-header")
            ],
            None,
        )
        .await,
        StatusCode::OK
    );
    world.status = Some(
        send_subagent_chat(
            world,
            &model,
            "body child",
            &[],
            Some(json!({
                "conversation_id": "gateway-subagent-session",
                "parent_turn_id": "gateway-parent",
                "subagent": true
            })),
        )
        .await,
    );
}

#[then("the paginated conversation exposes two explicit subagent edges")]
async fn paginated_conversation_exposes_subagent_edges(world: &mut TokenCenterWorld) {
    let clusters = world
        .client
        .get(format!(
            "{}/self/v1/conversations?limit=1",
            world.service_url
        ))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("subagent conversation list")
        .json::<Value>()
        .await
        .expect("subagent conversation list JSON");
    assert_eq!(clusters.as_array().map(Vec::len), Some(1), "{clusters}");
    assert_eq!(clusters[0]["request_count"], 3, "{clusters}");
    let cluster_id = clusters[0]["cluster_id"]
        .as_str()
        .expect("subagent cluster id");

    let mut cursor: Option<(i64, String)> = None;
    let mut request_ids = std::collections::HashSet::new();
    let mut subagent_targets = std::collections::HashSet::new();
    loop {
        let mut url = format!(
            "{}/self/v1/conversations/{cluster_id}?limit=1",
            world.service_url
        );
        if let Some((created_at, request_id)) = &cursor {
            url.push_str(&format!(
                "&before_created_at={created_at}&before_request_id={request_id}"
            ));
        }
        let page = world
            .client
            .get(url)
            .bearer_auth(&world.current_key)
            .send()
            .await
            .expect("paginated subagent detail")
            .json::<Value>()
            .await
            .expect("paginated subagent detail JSON");
        assert_eq!(page["requests"].as_array().map(Vec::len), Some(1), "{page}");
        request_ids.insert(
            page["requests"][0]["request_id"]
                .as_str()
                .expect("page request id")
                .to_owned(),
        );
        for edge in page["edges"].as_array().expect("page edges") {
            if edge["relation"] == "subagent" {
                assert_eq!(edge["evidence"]["explicit_parent"], true, "{edge}");
                assert_eq!(edge["evidence"]["subagent"], true, "{edge}");
                subagent_targets.insert(
                    edge["to_request_id"]
                        .as_str()
                        .expect("subagent target id")
                        .to_owned(),
                );
            }
        }
        cursor = page["next_cursor"].as_object().map(|next| {
            (
                next["before_created_at"]
                    .as_i64()
                    .expect("cursor timestamp"),
                next["before_request_id"]
                    .as_str()
                    .expect("cursor request id")
                    .to_owned(),
            )
        });
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(request_ids.len(), 3);
    assert_eq!(subagent_targets.len(), 2);
}

#[when(expr = "the client sends UA branch and orphan subagent hints for model {string}")]
async fn client_sends_implicit_subagent_hints(world: &mut TokenCenterWorld, model: String) {
    assert_eq!(
        send_subagent_chat(
            world,
            &model,
            "implicit root",
            &[("x-mtc-turn-id", "implicit-root")],
            None,
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        send_subagent_chat(
            world,
            &model,
            "UA branch vocabulary",
            &[
                ("user-agent", "codex-subagent/1.0"),
                ("x-mtc-client-name", "subagent-originator"),
                ("x-mtc-branch-id", "subagent-worker")
            ],
            None,
        )
        .await,
        StatusCode::OK
    );
    world.status = Some(
        send_subagent_chat(
            world,
            &model,
            "orphan explicit marker",
            &[("x-mtc-subagent", "true")],
            None,
        )
        .await,
    );
}

#[then("no logical conversation contains a subagent edge")]
async fn no_conversation_contains_subagent_edge(world: &mut TokenCenterWorld) {
    let clusters = world
        .client
        .get(format!(
            "{}/self/v1/conversations?limit=100",
            world.service_url
        ))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .expect("implicit subagent conversation list")
        .json::<Value>()
        .await
        .expect("implicit subagent conversation list JSON");
    assert_eq!(clusters.as_array().map(Vec::len), Some(3), "{clusters}");
    for cluster in clusters.as_array().expect("conversation array") {
        let cluster_id = cluster["cluster_id"].as_str().expect("cluster id");
        let detail = world
            .client
            .get(format!(
                "{}/self/v1/conversations/{cluster_id}?limit=200",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .expect("implicit subagent detail")
            .json::<Value>()
            .await
            .expect("implicit subagent detail JSON");
        assert!(
            detail["edges"]
                .as_array()
                .expect("conversation edges")
                .iter()
                .all(|edge| edge["relation"] != "subagent"),
            "{detail}"
        );
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
    let anthropic_route = ensure_fixture_route(world, "default", &model, "anthropic").await;
    grant_fixture_routes(
        world,
        "default",
        world.stable_key_id.expect("stable key id"),
        &[anthropic_route],
    )
    .await;
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

async fn control_json(
    world: &TokenCenterWorld,
    method: Method,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = world
        .client
        .request(method, format!("{}{}", world.service_url, path))
        .bearer_auth("test-service-token")
        .json(&body)
        .send()
        .await
        .expect("group routing control request");
    let status = response.status();
    let value = response
        .json::<Value>()
        .await
        .expect("group routing control response JSON");
    (status, value)
}

#[when("the operator configures overlapping provider groups and two route groups")]
async fn configure_overlapping_routing_groups(world: &mut TokenCenterWorld) {
    let tenant = "cucumber-routing-groups";
    let (status, issued) = control_json(
        world,
        Method::POST,
        "/internal/v1/keys",
        json!({
            "tenant_external_id": tenant,
            "principal_external_id": "group-routed-user",
            "alias": "group-routed-credential",
            "currency": "USD",
            "initial_balance": "10",
            "policy": {}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    let key_id = issued["key_id"]
        .as_str()
        .expect("group-routed key id")
        .to_owned();
    let old_key = issued["key"]
        .as_str()
        .expect("group-routed secret")
        .to_owned();

    let mut accounts = Vec::new();
    for name in ["excluded-upstream", "selected-upstream"] {
        let (status, account) = control_json(
            world,
            Method::POST,
            "/internal/v1/upstreams",
            json!({
                "tenant_external_id": tenant,
                "name": name,
                "driver": "http-json",
                "config": {"base_url": world.mock.as_ref().expect("mock upstream").uri()},
                "credential": {"type": "none"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{account}");
        accounts.push(
            account["id"]
                .as_str()
                .expect("group-routed account id")
                .to_owned(),
        );
    }

    let mut provider_groups = Vec::new();
    for (name, members) in [
        (
            "all-providers",
            vec![accounts[0].clone(), accounts[1].clone()],
        ),
        ("temporarily-excluded", vec![accounts[0].clone()]),
    ] {
        let (status, group) = control_json(
            world,
            Method::POST,
            "/internal/v1/provider-groups",
            json!({"tenant_external_id": tenant, "name": name}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{group}");
        let group_id = group["id"].as_str().expect("provider group id").to_owned();
        let (status, updated) = control_json(
            world,
            Method::PUT,
            &format!("/internal/v1/provider-groups/{group_id}/members"),
            json!({
                "tenant_external_id": tenant,
                "member_ids": members,
                "expected_updated_at": group["updated_at"]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{updated}");
        provider_groups.push(group_id);
    }

    let (status, route) = control_json(
        world,
        Method::POST,
        "/internal/v1/model-routes",
        json!({
            "tenant_external_id": tenant,
            "public_model": "group-routed-model",
            "upstream_model": "group-routed-upstream-model",
            "protocol": "openai",
            "priority": 0,
            "upstream_account_ids": accounts,
            "included_provider_group_ids": [provider_groups[0]],
            "excluded_provider_group_ids": [provider_groups[1]],
            "route_group_names": ["primary-routes", "codex-routes"],
            "granted_credential_ids": [key_id],
            "custom_model_confirmed": true
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");
    let route_id = route["id"]
        .as_str()
        .expect("group-routed route id")
        .to_owned();
    let route_group_ids = route["route_group_ids"]
        .as_array()
        .expect("two route group ids")
        .clone();
    assert_eq!(route_group_ids.len(), 2, "{route}");

    let (status, current) = control_json(
        world,
        Method::GET,
        &format!("/internal/v1/keys/{key_id}/routing?tenant_external_id={tenant}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{current}");
    let (status, routing) = control_json(
        world,
        Method::PUT,
        &format!("/internal/v1/keys/{key_id}/routing"),
        json!({
            "tenant_external_id": tenant,
            "route_ids": [route_id],
            "route_group_ids": route_group_ids,
            "expected_grant_revision": current["grant_revision"]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{routing}");

    world.response = json!({
        "tenant": tenant,
        "key_id": key_id,
        "old_key": old_key,
        "selected_account_id": accounts[1],
        "route_id": route_id,
        "route_group_ids": route_group_ids,
        "routing_before_rotation": routing,
        "route": route
    });
}

#[then("provider exclusions win and overlapping route group grants are deduplicated")]
async fn group_exclusions_and_intersections_are_exact(world: &mut TokenCenterWorld) {
    let route = &world.response["route"];
    assert_eq!(
        route["candidate_upstream_account_ids"],
        json!([world.response["selected_account_id"]]),
        "an excluded provider must be removed even when explicit and included: {route}"
    );
    assert_eq!(
        route["route_group_ids"].as_array().map(Vec::len),
        Some(2),
        "one route must be allowed to belong to multiple route groups"
    );
    let routing = &world.response["routing_before_rotation"];
    assert_eq!(routing["route_ids"], json!([world.response["route_id"]]));
    assert_eq!(
        routing["route_group_ids"],
        world.response["route_group_ids"]
    );
    assert_eq!(
        routing["effective_route_ids"],
        json!([world.response["route_id"]]),
        "direct and overlapping group grants must deduplicate the effective route"
    );

    let response = world
        .client
        .get(format!("{}/v1/models", world.service_url))
        .bearer_auth(world.response["old_key"].as_str().expect("old routed key"))
        .send()
        .await
        .expect("list models for group-routed credential");
    assert_eq!(response.status(), StatusCode::OK);
    let models: Value = response.json().await.expect("group-routed models JSON");
    assert_eq!(models["data"][0]["id"], "group-routed-model");
    world.response["models_before_rotation"] = models;
}

#[when("the operator rotates the group-routed credential")]
async fn rotate_group_routed_credential(world: &mut TokenCenterWorld) {
    let key_id = world.response["key_id"]
        .as_str()
        .expect("group-routed key id");
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/keys/{key_id}/rotate",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .header("idempotency-key", "cucumber-group-routing-rotation")
        .send()
        .await
        .expect("rotate group-routed credential");
    assert_eq!(response.status(), StatusCode::OK);
    let rotated: Value = response
        .json()
        .await
        .expect("rotated group-routed key JSON");
    assert_eq!(rotated["key_id"], world.response["key_id"]);
    world.response["rotated_key"] = rotated["key"].clone();
}

#[then("the rotated credential preserves its route and route-group authorization")]
async fn rotated_group_routing_is_stable(world: &mut TokenCenterWorld) {
    let old_response = world
        .client
        .get(format!("{}/v1/models", world.service_url))
        .bearer_auth(world.response["old_key"].as_str().expect("old routed key"))
        .send()
        .await
        .expect("old group-routed key authentication");
    assert_eq!(old_response.status(), StatusCode::UNAUTHORIZED);

    let tenant = world.response["tenant"].as_str().expect("routing tenant");
    let key_id = world.response["key_id"].as_str().expect("routing key id");
    let (status, routing) = control_json(
        world,
        Method::GET,
        &format!("/internal/v1/keys/{key_id}/routing?tenant_external_id={tenant}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{routing}");
    let before = &world.response["routing_before_rotation"];
    for field in [
        "route_ids",
        "route_group_ids",
        "effective_route_ids",
        "grant_revision",
    ] {
        assert_eq!(routing[field], before[field], "rotation changed {field}");
    }

    let response = world
        .client
        .get(format!("{}/v1/models", world.service_url))
        .bearer_auth(
            world.response["rotated_key"]
                .as_str()
                .expect("rotated routed key"),
        )
        .send()
        .await
        .expect("rotated group-routed key model list");
    assert_eq!(response.status(), StatusCode::OK);
    let models: Value = response
        .json()
        .await
        .expect("rotated group-routed models JSON");
    assert_eq!(models, world.response["models_before_rotation"]);
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
