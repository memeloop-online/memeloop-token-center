use std::{convert::Infallible, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

use super::*;
use crate::config::{ArchiveBackend, Config};

pub(super) async fn test_state() -> (AppState, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("roles.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap();
    (state, directory)
}

fn json_post(path: &str) -> Request<Body> {
    Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap()
}

fn file_count(root: &std::path::Path) -> usize {
    std::fs::read_dir(root)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| {
                    if entry.path().is_dir() {
                        file_count(&entry.path())
                    } else {
                        1
                    }
                })
                .sum()
        })
        .unwrap_or_default()
}

struct GenerationApiFixture {
    state: AppState,
    database_url: String,
    archive_path: std::path::PathBuf,
    key: String,
    key_id: Uuid,
    model: String,
    _directory: tempfile::TempDir,
}

async fn generation_api_fixture(
    label: &str,
    driver: &str,
    billing_unit: &str,
    policy: KeyPolicy,
    initial_balance: Decimal,
) -> GenerationApiFixture {
    let directory = tempfile::tempdir().unwrap();
    let archive_path = directory.path().join("archive");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join(format!("{label}.db")).display()
    );
    let mut config = Config::for_test(database_url.clone());
    config.archive_backend = ArchiveBackend::Filesystem;
    config.archive_path = Some(archive_path.display().to_string());
    let state = AppState::initialize(config).await.unwrap();
    let tenant = format!("generation-api-{label}");
    let model = format!("generation-api-model-{label}");
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.clone(),
                principal_external_id: "member".to_owned(),
                alias: label.to_owned(),
                currency: "USD".to_owned(),
                policy,
                initial_balance,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let upstream = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: format!("{label}-upstream"),
                driver: driver.to_owned(),
                config: json!({
                    "base_url": "http://127.0.0.1:8188",
                    "workflow_id": "workflow-v1",
                    "workflow_template": {"1": {"inputs": {}}}
                }),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant,
            public_model: model.clone(),
            upstream_account_id: upstream.id,
            upstream_model: "workflow-v1".to_owned(),
            protocol: "generation".to_owned(),
            priority: 0,
        })
        .await
        .unwrap();
    state
        .db
        .upsert_generation_price(&model, "USD", billing_unit, Decimal::new(25, 2))
        .await
        .unwrap();
    GenerationApiFixture {
        state,
        database_url,
        archive_path,
        key: issued.key,
        key_id: issued.key_id,
        model,
        _directory: directory,
    }
}

async fn post_generation(
    fixture: &GenerationApiFixture,
    idempotency_key: Option<&str>,
    input: Value,
) -> Response {
    let mut request = Request::post("/v1/generations")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key));
    if let Some(idempotency_key) = idempotency_key {
        request = request.header("idempotency-key", idempotency_key);
    }
    router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
        .oneshot(
            request
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "model": fixture.model,
                        "input": input
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn generation_fixture_balance(fixture: &GenerationApiFixture) -> String {
    let authenticated = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    fixture
        .state
        .db
        .key_view(&authenticated)
        .await
        .unwrap()
        .available_balance
}

#[tokio::test]
async fn rejected_proxy_admission_does_not_write_the_archive() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let archive_path = directory.path().join("archive");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("archive-admission.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.archive_backend = ArchiveBackend::Filesystem;
    config.archive_path = Some(archive_path.display().to_string());
    config.upstream_openai_url = Some(upstream.uri());
    config.upstream_openai_key = Some("unused-upstream-key".to_owned());
    let state = AppState::initialize(config).await.unwrap();
    state
        .db
        .upsert_model_price("archive-admission-model", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "archive-admission".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "archive-admission".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    allowed_models: vec!["archive-admission-model".to_owned()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let response = router(state)
        .oneshot(
            Request::post("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {}", issued.key))
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "model": "archive-admission-model",
                        "input": "unique rejected body"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let response_body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        String::from_utf8_lossy(&response_body)
    );
    assert_eq!(file_count(&archive_path), 0);
}

#[tokio::test]
async fn upstream_rate_limit_status_is_preserved_but_its_body_is_sanitized() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"message": "provider secret: must-not-reach-client"},
            "debug_token": "provider-sensitive-token"
        })))
        .expect(1)
        .mount(&upstream)
        .await;

    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("upstream-429.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.upstream_openai_url = Some(upstream.uri());
    config.upstream_openai_key = Some("test-upstream-key".to_owned());
    let state = AppState::initialize(config).await.unwrap();
    state
        .db
        .upsert_model_price("upstream-429-model", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "upstream-429".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "upstream-429".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    allowed_models: vec!["upstream-429-model".to_owned()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let response = router_for_role(state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {}", issued.key))
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "model": "upstream-429-model",
                        "messages": [{"role": "user", "content": "hello"}]
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("upstream rejected the request"));
    assert!(!body.contains("must-not-reach-client"));
    assert!(!body.contains("provider-sensitive-token"));

    let requests = state.db.list_requests(issued.key_id, 10).await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].status_code, Some(429));
    assert_eq!(requests[0].error_code.as_deref(), Some("http_429"));
}

#[tokio::test]
async fn generation_archive_is_written_only_by_the_admitted_idempotency_owner() {
    let fixture = generation_api_fixture(
        "archive-owner",
        "comfyui",
        "job",
        KeyPolicy {
            allowed_models: vec!["generation-api-model-archive-owner".to_owned()],
            max_concurrency: 1,
            ..KeyPolicy::default()
        },
        Decimal::TEN,
    )
    .await;
    let first = post_generation(
        &fixture,
        Some("archive-owner-key"),
        json!({"parameters": {"prompt": "cat"}}),
    )
    .await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    assert_eq!(file_count(&fixture.archive_path), 1);

    let replay = post_generation(
        &fixture,
        Some("archive-owner-key"),
        json!({"parameters": {"prompt": "cat"}}),
    )
    .await;
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(file_count(&fixture.archive_path), 1);

    let mismatch = post_generation(
        &fixture,
        Some("archive-owner-key"),
        json!({"parameters": {"prompt": "dog"}}),
    )
    .await;
    assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);
    assert_eq!(file_count(&fixture.archive_path), 1);

    let concurrency_rejected = post_generation(
        &fixture,
        Some("archive-concurrency-key"),
        json!({"parameters": {"prompt": "owl"}}),
    )
    .await;
    assert_eq!(concurrency_rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(file_count(&fixture.archive_path), 1);
    assert_eq!(
        fixture
            .state
            .db
            .list_generation_jobs(fixture.key_id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn generation_quota_and_rpm_rejections_do_not_write_the_archive() {
    let no_balance = generation_api_fixture(
        "no-balance",
        "comfyui",
        "job",
        KeyPolicy {
            allowed_models: vec!["generation-api-model-no-balance".to_owned()],
            ..KeyPolicy::default()
        },
        Decimal::ZERO,
    )
    .await;
    let rejected = post_generation(
        &no_balance,
        Some("no-balance-key"),
        json!({"parameters": {"prompt": "cat"}}),
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(file_count(&no_balance.archive_path), 0);

    let rpm = generation_api_fixture(
        "rpm",
        "comfyui",
        "job",
        KeyPolicy {
            allowed_models: vec!["generation-api-model-rpm".to_owned()],
            requests_per_minute: 1,
            max_concurrency: 8,
            ..KeyPolicy::default()
        },
        Decimal::TEN,
    )
    .await;
    let admitted = post_generation(
        &rpm,
        Some("rpm-first"),
        json!({"parameters": {"prompt": "cat"}}),
    )
    .await;
    assert_eq!(admitted.status(), StatusCode::ACCEPTED);
    assert_eq!(file_count(&rpm.archive_path), 1);
    let rejected = post_generation(
        &rpm,
        Some("rpm-second"),
        json!({"parameters": {"prompt": "dog"}}),
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(file_count(&rpm.archive_path), 1);
}

#[tokio::test]
async fn generation_archive_and_attach_failures_refund_and_never_become_worker_visible() {
    let archive_failure = generation_api_fixture(
        "archive-failure",
        "comfyui",
        "job",
        KeyPolicy {
            allowed_models: vec!["generation-api-model-archive-failure".to_owned()],
            ..KeyPolicy::default()
        },
        Decimal::TEN,
    )
    .await;
    std::fs::remove_dir(&archive_failure.archive_path).unwrap();
    std::fs::write(&archive_failure.archive_path, b"not a directory").unwrap();
    let failed = post_generation(
        &archive_failure,
        Some("archive-failure-key"),
        json!({"parameters": {"prompt": "cat"}}),
    )
    .await;
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let jobs = archive_failure
        .state
        .db
        .list_generation_jobs(archive_failure.key_id, 10)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, "failed");
    assert_eq!(
        jobs[0].error_code.as_deref(),
        Some("generation_archive_failed")
    );
    assert_eq!(generation_fixture_balance(&archive_failure).await, "10");
    assert!(
        archive_failure
            .state
            .db
            .claim_generation_job("archive-failure-worker")
            .await
            .unwrap()
            .is_none()
    );

    let attach_failure = generation_api_fixture(
        "attach-failure",
        "comfyui",
        "job",
        KeyPolicy {
            allowed_models: vec!["generation-api-model-attach-failure".to_owned()],
            ..KeyPolicy::default()
        },
        Decimal::TEN,
    )
    .await;
    let pool = sqlx::AnyPool::connect(&attach_failure.database_url)
        .await
        .unwrap();
    sqlx::query(
            "CREATE TRIGGER fail_generation_archive_attach BEFORE UPDATE OF status ON generation_jobs WHEN OLD.status = 'preparing' AND NEW.status = 'queued' BEGIN SELECT RAISE(FAIL, 'forced archive attach failure'); END",
        )
        .execute(&pool)
        .await
        .unwrap();
    let failed = post_generation(
        &attach_failure,
        Some("attach-failure-key"),
        json!({"parameters": {"prompt": "cat"}}),
    )
    .await;
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(file_count(&attach_failure.archive_path), 1);
    let jobs = attach_failure
        .state
        .db
        .list_generation_jobs(attach_failure.key_id, 10)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, "failed");
    assert_eq!(
        jobs[0].error_code.as_deref(),
        Some("generation_archive_attach_failed")
    );
    assert_eq!(generation_fixture_balance(&attach_failure).await, "10");
    assert!(
        attach_failure
            .state
            .db
            .claim_generation_job("attach-failure-worker")
            .await
            .unwrap()
            .is_none()
    );
    pool.close().await;
}

#[tokio::test]
async fn invalid_seedance_duration_is_rejected_before_admission_or_archive() {
    let fixture = generation_api_fixture(
        "seedance-duration",
        "volcengine-seedance",
        "second",
        KeyPolicy {
            allowed_models: vec!["generation-api-model-seedance-duration".to_owned()],
            ..KeyPolicy::default()
        },
        Decimal::TEN,
    )
    .await;
    for (index, input) in [
        json!({"duration": 5.0, "content": [{"type": "text", "text": "cat"}]}),
        json!({"duration": "5", "content": [{"type": "text", "text": "cat"}]}),
        json!({"duration": true, "content": [{"type": "text", "text": "cat"}]}),
        json!({"content": [{"type": "text", "text": "cat --dur 1 --dur 60"}]}),
        json!({"duration": 5, "content": [{"type": "text", "text": "cat --dur 60"}]}),
        json!({"content": [{"type": "text", "text": "cat --dur nope"}]}),
    ]
    .into_iter()
    .enumerate()
    {
        let rejected =
            post_generation(&fixture, Some(&format!("invalid-duration-{index}")), input).await;
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    }
    assert_eq!(file_count(&fixture.archive_path), 0);
    assert!(
        fixture
            .state
            .db
            .list_generation_jobs(fixture.key_id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture
            .state
            .db
            .claim_generation_job("invalid-duration-worker")
            .await
            .unwrap()
            .is_none()
    );
}

#[test]
fn generation_asset_ranges_are_single_bounded_and_rfc_compatible() {
    assert_eq!(parse_byte_range(None, 10), Ok(None));
    assert_eq!(parse_byte_range(Some("bytes=2-6"), 10), Ok(Some(2..7)));
    assert_eq!(parse_byte_range(Some("bytes=7-"), 10), Ok(Some(7..10)));
    assert_eq!(parse_byte_range(Some("bytes=-3"), 10), Ok(Some(7..10)));
    assert_eq!(parse_byte_range(Some("bytes=-99"), 10), Ok(Some(0..10)));
    for invalid in [
        "items=0-1",
        "bytes=",
        "bytes=0-1,3-4",
        "bytes=10-11",
        "bytes=6-2",
        "bytes=-0",
    ] {
        assert_eq!(parse_byte_range(Some(invalid), 10), Err(()), "{invalid}");
    }
    assert_eq!(parse_byte_range(Some("bytes=0-0"), 0), Err(()));
}

#[test]
fn upstream_image_idempotency_is_secret_and_scoped_to_stable_identity() {
    let tenant = Uuid::from_u128(1);
    let route = Uuid::from_u128(2);
    let first = scoped_upstream_image_idempotency(
        b"test-pepper",
        tenant,
        Uuid::from_u128(3),
        route,
        "/v1/images/generations",
        "shared-low-entropy-key",
    );
    let replay = scoped_upstream_image_idempotency(
        b"test-pepper",
        tenant,
        Uuid::from_u128(3),
        route,
        "/v1/images/generations",
        "shared-low-entropy-key",
    );
    let other_identity = scoped_upstream_image_idempotency(
        b"test-pepper",
        tenant,
        Uuid::from_u128(4),
        route,
        "/v1/images/generations",
        "shared-low-entropy-key",
    );
    assert_eq!(first, replay);
    assert_ne!(first, other_identity);
    assert!(first.starts_with("mtc-img-"));
    assert!(!first.contains("shared-low-entropy-key"));
}

#[tokio::test]
async fn all_synchronous_image_paths_share_exactly_two_response_permits() {
    let both = IMAGE_RESPONSE_PERMITS
        .acquire_many(2)
        .await
        .expect("acquire both image permits");
    assert!(
        tokio::time::timeout(Duration::from_millis(25), IMAGE_RESPONSE_PERMITS.acquire())
            .await
            .is_err(),
        "a third synchronous image response must wait"
    );
    drop(both);
    let _released =
        tokio::time::timeout(Duration::from_millis(100), IMAGE_RESPONSE_PERMITS.acquire())
            .await
            .expect("permit becomes available")
            .expect("semaphore remains open");
}

#[tokio::test]
async fn queued_image_permit_wait_heartbeats_and_stops_when_claim_is_lost() {
    let semaphore = tokio::sync::Semaphore::new(0);
    let heartbeats = std::sync::atomic::AtomicUsize::new(0);
    let (permit, ()) = tokio::join!(
        acquire_image_permit_with_heartbeat(&semaphore, Duration::from_millis(5), || {
            heartbeats.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(Ok(true))
        },),
        async {
            tokio::time::sleep(Duration::from_millis(24)).await;
            semaphore.add_permits(1);
        }
    );
    assert!(permit.expect("heartbeat wait succeeds").is_some());
    assert!(
        heartbeats.load(std::sync::atomic::Ordering::SeqCst) >= 3,
        "a queued request must renew repeatedly before a permit is released"
    );

    let lost = tokio::sync::Semaphore::new(0);
    let result = acquire_image_permit_with_heartbeat(&lost, Duration::from_millis(1), || {
        std::future::ready(Ok(false))
    })
    .await
    .expect("lost claim is a controlled outcome");
    assert!(
        result.is_none(),
        "a lost owner must never reach the upstream"
    );
}

#[tokio::test]
async fn image_response_limit_accepts_exactly_16_mib_and_rejects_one_more_byte() {
    let server = MockServer::start().await;
    Mock::given(path("/exact"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; MAX_IMAGE_RESPONSE]))
        .mount(&server)
        .await;
    Mock::given(path("/over"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; MAX_IMAGE_RESPONSE + 1]))
        .mount(&server)
        .await;
    let client = reqwest::Client::new();
    let exact = read_image_response_bounded(
        client
            .get(format!("{}/exact", server.uri()))
            .send()
            .await
            .expect("exact response"),
    )
    .await
    .expect("exact boundary accepted");
    assert_eq!(exact.len(), MAX_IMAGE_RESPONSE);
    let over = read_image_response_bounded(
        client
            .get(format!("{}/over", server.uri()))
            .send()
            .await
            .expect("oversized response"),
    )
    .await;
    assert_eq!(over, Err(ImageResponseReadError::TooLarge));
}

#[test]
fn responses_tool_requires_exactly_one_valid_base64_image() {
    assert!(has_one_valid_bounded_image(&[STANDARD.encode(b"png")]));
    assert!(!has_one_valid_bounded_image(&[] as &[&str]));
    assert!(!has_one_valid_bounded_image(&[String::new()]));
    assert!(!has_one_valid_bounded_image(&[
        STANDARD.encode(b"one"),
        STANDARD.encode(b"two"),
    ]));
    assert!(!has_one_valid_bounded_image(&["not-base64".to_owned()]));
}

#[test]
fn standard_image_results_match_requested_count_and_contain_real_data() {
    assert!(
        openai_image_urls(
            &json!({"data": [{"b64_json": STANDARD.encode(b"png") }]}),
            1
        )
        .is_ok()
    );
    assert!(
        openai_image_urls(
            &json!({"data": [{"url": "https://example.test/image.png"}]}),
            1
        )
        .is_ok()
    );
    assert!(
        openai_image_urls(
            &json!({"data": [{"b64_json": STANDARD.encode(b"png") }]}),
            2
        )
        .is_err()
    );
    assert!(openai_image_urls(&json!({"data": [{"b64_json": ""}]}), 1).is_err());
    assert!(openai_image_urls(&json!({"data": [{"b64_json": "not-base64"}]}), 1).is_err());
    assert!(openai_image_urls(&json!({"data": [{"url": ""}]}), 1).is_err());
}

#[test]
fn image_success_response_is_a_whitelist_and_never_replays_provider_secrets() {
    let request_id = Uuid::now_v7();
    let asset_id = Uuid::now_v7();
    let asset = crate::model::ArchivedGenerationAsset {
        asset_id,
        index: 0,
        object_locator: format!("staging/synchronous/{request_id}/asset-0"),
        mime_type: "image/png".to_owned(),
        size_bytes: 3,
        filename: "asset-0.png".to_owned(),
    };
    let upstream = json!({
        "id": "provider-secret-response-id",
        "debug_url": "https://provider.invalid/debug?token=must-not-leak",
        "created": 42,
        "data": [{
            "url": "https://provider.invalid/download/SECRET_TOKEN.png?sig=must-not-leak",
            "revised_prompt": "safe prompt",
            "provider_trace": "must-not-leak"
        }],
        "usage": {
            "total_tokens": 7,
            "provider_debug": "must-not-leak",
            "input_tokens_details": {"image_tokens": 3, "secret": "must-not-leak"}
        }
    });

    let sanitized = sanitize_openai_image_response(&upstream, request_id, &[asset])
        .expect("valid provider response");
    assert_eq!(sanitized["created"], 42);
    assert_eq!(
        sanitized["data"][0]["url"],
        format!("/self/v1/requests/{request_id}/assets/{asset_id}")
    );
    assert_eq!(
        sanitized["data"][0]["archived_asset"]["filename"],
        "asset-0.png"
    );
    assert_eq!(sanitized["usage"]["total_tokens"], 7);
    assert_eq!(
        sanitized["usage"]["input_tokens_details"]["image_tokens"],
        3
    );
    let rendered = sanitized.to_string();
    assert!(!rendered.contains("must-not-leak"));
    assert!(!rendered.contains("SECRET_TOKEN"));
    assert!(!rendered.contains("provider-secret-response-id"));
    assert!(!rendered.contains("provider_trace"));
    assert!(!rendered.contains("provider_debug"));
}

#[tokio::test]
async fn gateway_and_control_have_disjoint_route_surfaces() {
    let (state, _directory) = test_state().await;
    let gateway = router_for_role(state.clone(), RuntimeRole::Gateway);
    let control = router_for_role(state, RuntimeRole::Control);

    let gateway_internal = gateway
        .clone()
        .oneshot(json_post("/internal/v1/keys"))
        .await
        .unwrap();
    assert_eq!(gateway_internal.status(), StatusCode::NOT_FOUND);
    let control_internal = control
        .clone()
        .oneshot(
            Request::post("/internal/v1/keys")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"principal_external_id":"probe","alias":"probe"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(control_internal.status(), StatusCode::UNAUTHORIZED);

    let control_model = control
        .oneshot(json_post("/v1/chat/completions"))
        .await
        .unwrap();
    assert_eq!(control_model.status(), StatusCode::NOT_FOUND);
    let gateway_model = gateway
        .oneshot(json_post("/v1/chat/completions"))
        .await
        .unwrap();
    assert_eq!(gateway_model.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn retired_bridge_is_absent_and_native_codex_rejects_raw_credentials() {
    let (state, _directory) = test_state().await;
    let control = router_for_role(state, RuntimeRole::Control);
    let providers = control
        .clone()
        .oneshot(
            Request::get("/internal/v1/provider-types")
                .header(header::AUTHORIZATION, "Bearer test-service-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(providers.status(), StatusCode::OK);
    let providers: Value = serde_json::from_slice(
        &axum::body::to_bytes(providers.into_body(), 64 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    let ids = providers
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|provider| provider.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(ids.contains(&"openai-codex"));
    assert!(!ids.iter().any(|driver| driver.starts_with("cpa-")));

    let direct = control
        .clone()
        .oneshot(
            Request::post("/internal/v1/upstreams")
                .header(header::AUTHORIZATION, "Bearer test-service-token")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{
                        "name":"raw-codex",
                        "driver":"openai-codex",
                        "config":{
                            "base_url":"https://chatgpt.com/backend-api/codex",
                            "network_scope":"public",
                            "reservation_token_bounds":{}
                        },
                        "credential":{
                            "type":"oauth",
                            "access_token":"must-not-install",
                            "refresh_token":"must-not-install",
                            "expires_at":4102444800000,
                            "adapter_state":{"schema":"openai-codex-oauth-v1","account_id":"account-test"}
                        }
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(direct.status(), StatusCode::BAD_REQUEST);

    for retired_path in [
        "/internal/v1/oauth/subscription-bridge/start",
        "/internal/v1/oauth/subscription-bridge/poll",
        "/internal/v1/imports/cpa/subscription-accounts",
    ] {
        let response = control
            .clone()
            .oneshot(
                Request::post(retired_path)
                    .header(header::AUTHORIZATION, "Bearer test-service-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn authentication_rejects_requests_before_json_body_parsing() {
    let (state, _directory) = test_state().await;
    let control = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::post("/internal/v1/keys")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("not-json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(control.status(), StatusCode::UNAUTHORIZED);

    let gateway = router_for_role(state, RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("not-json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(gateway.status(), StatusCode::UNAUTHORIZED);

    let (state, _directory) = test_state().await;
    let pending_body =
        Body::from_stream(futures_util::stream::pending::<Result<Bytes, Infallible>>());
    let unauthenticated = tokio::time::timeout(
        Duration::from_millis(100),
        router_for_role(state, RuntimeRole::Gateway).oneshot(
            Request::post("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(pending_body)
                .unwrap(),
        ),
    )
    .await
    .expect("unauthenticated request must not read a pending body")
    .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn authenticated_control_rejects_declared_oversized_body_before_reading_it() {
    let (state, _directory) = test_state().await;
    let service_token = state.config.service_token.clone();
    let response = tokio::time::timeout(
        Duration::from_millis(100),
        router_for_role(state, RuntimeRole::Control).oneshot(
            Request::post("/internal/v1/keys")
                .header(header::AUTHORIZATION, format!("Bearer {service_token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, MAX_DEFAULT_REQUEST_BODY + 1)
                .body(Body::from_stream(futures_util::stream::pending::<
                    Result<Bytes, Infallible>,
                >()))
                .unwrap(),
        ),
    )
    .await
    .expect("declared oversized control request is rejected before reading")
    .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn public_responses_include_browser_security_headers() {
    let (state, _directory) = test_state().await;
    let response = router_for_role(state, RuntimeRole::Gateway)
        .oneshot(
            Request::get("/healthz")
                .body(Body::empty())
                .expect("health request"),
        )
        .await
        .expect("health response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::X_CONTENT_TYPE_OPTIONS),
        Some(&HeaderValue::from_static("nosniff"))
    );
    assert_eq!(
        response.headers().get(header::X_FRAME_OPTIONS),
        Some(&HeaderValue::from_static("DENY"))
    );
    assert_eq!(
        response.headers().get(header::REFERRER_POLICY),
        Some(&HeaderValue::from_static("no-referrer"))
    );
}

#[tokio::test]
async fn quiet_request_event_streams_disconnect_promptly_and_are_service_bounded() {
    let (state, _directory) = test_state().await;
    let service_token = state.config.service_token.clone();
    let application = router_for_role(state.clone(), RuntimeRole::Control);
    let open_stream = || {
        application.clone().oneshot(
            Request::get("/internal/v1/request-events")
                .header(header::AUTHORIZATION, format!("Bearer {service_token}"))
                .body(Body::empty())
                .expect("request event stream"),
        )
    };

    let mut streams = Vec::new();
    for _ in 0..crate::request_event_stream::REQUEST_EVENT_STREAMS_PER_SERVICE {
        let response = open_stream().await.expect("open request event stream");
        assert_eq!(response.status(), StatusCode::OK);
        streams.push(response);
    }
    let rejected = open_stream().await.expect("bounded request event stream");
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(streams);

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if state.request_event_streams.global_available_permits()
                == crate::request_event_stream::GLOBAL_REQUEST_EVENT_STREAMS
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dropped quiet streams stop their poll tasks");

    for _ in 0..4 {
        let mut dropped_batch = Vec::new();
        for _ in 0..crate::request_event_stream::REQUEST_EVENT_STREAMS_PER_SERVICE {
            let response = open_stream().await.expect("reopened request event stream");
            assert_eq!(response.status(), StatusCode::OK);
            dropped_batch.push(response);
        }
        drop(dropped_batch);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state.request_event_streams.global_available_permits()
                    == crate::request_event_stream::GLOBAL_REQUEST_EVENT_STREAMS
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("immediately dropped stream bodies release every permit");
    }
}

#[tokio::test]
async fn plugin_provider_can_contribute_an_oauth_adapter_route() {
    let (mut state, _directory) = test_state().await;
    state
        .providers
        .extend([crate::provider::ProviderType {
            id: "plugin-provider".to_owned(),
            display_name: "Plugin provider".to_owned(),
            protocols: vec!["openai".to_owned()],
            modalities: vec!["text".to_owned()],
            config_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["base_url", "network_scope"],
                "properties": {
                    "base_url": {"type": "string", "format": "uri"},
                    "network_scope": {"const": "private"}
                }
            }),
            credential_schema: json!({"type": "object"}),
            oauth_adapter: Some(crate::provider::OAuthAdapterContribution {
                api_version: "oauth-adapter-v1".to_owned(),
                flow_kind: crate::provider::OAuthFlowKind::CursorPkce,
                login_url: "http://oauth-adapter.default.svc/login".to_owned(),
                poll_url: "http://oauth-adapter.default.svc/poll".to_owned(),
                refresh_url: "http://oauth-adapter.default.svc/refresh".to_owned(),
            }),
            managed_oauth_adapter: None,
            component_adapter: None,
            source: "plugin:test@1.0.0".to_owned(),
        }])
        .unwrap();
    let response = router_for_role(state, RuntimeRole::Control)
            .oneshot(
                Request::post("/internal/v1/oauth/provider-adapter/start")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer test-service-token")
                    .body(Body::from(
                        r#"{"account_name":"plugin-primary","provider_driver":"plugin-provider","provider_config":{"base_url":"http://plugin-upstream.default.svc","network_scope":"private"}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[test]
fn seedance_duration_is_strict_and_canonical_before_admission() {
    let mut missing = json!({"content": [{"type": "text", "text": "a fox"}]});
    assert_eq!(normalize_seedance_duration(&mut missing).unwrap(), 5);
    assert_eq!(missing["duration"], json!(5));

    let mut content_only =
        json!({"content": [{"type": "text", "text": "a fox --dur 7 in the wind"}]});
    assert_eq!(normalize_seedance_duration(&mut content_only).unwrap(), 7);
    assert_eq!(content_only["duration"], json!(7));
    assert_eq!(content_only["content"][0]["text"], "a fox in the wind");
    assert!(!content_only.to_string().contains("--dur"));

    let mut matching = json!({
        "duration": 9,
        "content": [{"type": "text", "text": "a fox --dur 9"}]
    });
    assert_eq!(normalize_seedance_duration(&mut matching).unwrap(), 9);
    assert_eq!(matching["duration"], json!(9));
    assert_eq!(matching["content"][0]["text"], "a fox");

    for invalid in [json!(5.0), json!("5"), json!(true), Value::Null] {
        let mut input = json!({"duration": invalid});
        assert!(matches!(
            normalize_seedance_duration(&mut input),
            Err(AppError::BadRequest(_))
        ));
    }
    for invalid in [0, 61] {
        let mut input = json!({"duration": invalid});
        assert!(matches!(
            normalize_seedance_duration(&mut input),
            Err(AppError::BadRequest(_))
        ));
    }
}

#[test]
fn seedance_content_duration_rejects_ambiguity_and_malformed_options() {
    for text in [
        "fox --dur 1 --dur 60",
        "fox --dur",
        "fox --dur five",
        "fox --dur 5.0",
        "fox --dur -1",
        "fox --dur=5",
    ] {
        let mut input = json!({"content": [{"type": "text", "text": text}]});
        assert!(
            matches!(
                normalize_seedance_duration(&mut input),
                Err(AppError::BadRequest(_))
            ),
            "content option should be rejected: {text}"
        );
    }

    let mut repeated_across_items = json!({"content": [
        {"type": "text", "text": "fox --dur 1"},
        {"type": "text", "text": "wind --dur 60"}
    ]});
    assert!(matches!(
        normalize_seedance_duration(&mut repeated_across_items),
        Err(AppError::BadRequest(_))
    ));

    let mut conflicting = json!({
        "duration": 5,
        "content": [{"type": "text", "text": "fox --dur 60"}]
    });
    assert!(matches!(
        normalize_seedance_duration(&mut conflicting),
        Err(AppError::BadRequest(_))
    ));
}
