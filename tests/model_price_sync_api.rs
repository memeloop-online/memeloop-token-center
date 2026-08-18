use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn fixture(server: &MockServer, database_name: &str) -> (tempfile::TempDir, AppState) {
    let directory = tempfile::tempdir().expect("model price sync test directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join(database_name).display()
    );
    let mut config = Config::for_test(database_url);
    config.pricing_models_dev_url = format!("{}/models-dev", server.uri());
    config.pricing_litellm_url = format!("{}/litellm", server.uri());
    config.pricing_openrouter_url = format!("{}/openrouter", server.uri());
    let state = AppState::initialize(config)
        .await
        .expect("model price sync application");
    (directory, state)
}

async fn sync_request(state: &AppState, models: &[&str]) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/internal/v1/model-prices/sync")
        .header(header::AUTHORIZATION, "Bearer test-service-token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"models": models, "currency": "USD"})).unwrap(),
        ))
        .unwrap();
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request)
        .await
        .expect("model price sync response");
    let status = response.status();
    let body = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .expect("bounded model price sync response");
    let value = serde_json::from_slice(&body).expect("model price sync JSON");
    (status, value)
}

async fn mount_fixture(server: &MockServer, request_path: &str, body: &'static str) {
    Mock::given(method("GET"))
        .and(path(request_path))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_raw(body, "application/json"),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn management_sync_http_endpoint_fetches_all_sources_in_priority_order_and_persists() {
    let server = MockServer::start().await;
    mount_fixture(
        &server,
        "/models-dev",
        include_str!("fixtures/pricing/models-dev.json"),
    )
    .await;
    mount_fixture(
        &server,
        "/litellm",
        include_str!("fixtures/pricing/litellm.json"),
    )
    .await;
    mount_fixture(
        &server,
        "/openrouter",
        include_str!("fixtures/pricing/openrouter.json"),
    )
    .await;
    let (_directory, state) = fixture(&server, "successful-sync.db").await;

    let (status, body) = sync_request(
        &state,
        &[
            "openai/gpt-priority",
            "openai/gpt-fallback",
            "openai/gpt-openrouter",
            "gpt-conflict",
            "gpt-missing-output",
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source"], "multi");
    assert_eq!(
        body["sources"],
        json!(["models.dev", "litellm", "openrouter"])
    );
    assert_eq!(body["imported"], 3);
    assert_eq!(
        body["matched"],
        json!([
            "openai/gpt-fallback",
            "openai/gpt-openrouter",
            "openai/gpt-priority"
        ])
    );
    assert_eq!(
        body["sourceResults"]
            .as_array()
            .expect("source result audit")
            .iter()
            .map(|result| (
                result["source"].as_str().unwrap(),
                result["models"].as_u64().unwrap(),
                result["skipped"].as_u64().unwrap(),
                result.get("error")
            ))
            .collect::<Vec<_>>(),
        vec![
            ("models.dev", 4, 1, None),
            ("litellm", 3, 1, None),
            ("openrouter", 2, 1, None),
        ]
    );
    assert_eq!(body["candidates"][0]["model"], "gpt-conflict");
    assert_eq!(
        body["candidates"][0]["candidates"].as_array().map(Vec::len),
        Some(2)
    );
    assert_eq!(body["unmatched"], json!(["gpt-missing-output"]));

    let priority = state
        .db
        .model_price_view("openai/gpt-priority", "USD")
        .await
        .expect("models.dev priority price persisted");
    assert_eq!(priority.source, "models.dev");
    assert_eq!(priority.input_per_million, "1.25");
    assert_eq!(priority.tiers[0].cached_input_per_million, "0.25");
    assert_eq!(priority.tiers[0].cache_write_per_million, "1.5");
    assert!(!priority.tiers[0].cache_price_estimated);

    let fallback = state
        .db
        .model_price_view("openai/gpt-fallback", "USD")
        .await
        .expect("LiteLLM fallback price persisted");
    assert_eq!(fallback.source, "litellm");
    assert_eq!(fallback.input_per_million, "5");
    assert_eq!(fallback.output_per_million, "6");

    let openrouter = state
        .db
        .model_price_view("openai/gpt-openrouter", "USD")
        .await
        .expect("OpenRouter last-choice price persisted");
    assert_eq!(openrouter.source, "openrouter");
    assert_eq!(openrouter.input_per_million, "13");
    assert_eq!(openrouter.output_per_million, "14");
}

#[tokio::test]
async fn management_sync_http_endpoint_writes_nothing_when_every_source_fails() {
    let server = MockServer::start().await;
    for request_path in ["/models-dev", "/litellm", "/openrouter"] {
        Mock::given(method("GET"))
            .and(path(request_path))
            .respond_with(ResponseTemplate::new(503).set_body_string("not a catalog"))
            .expect(1)
            .mount(&server)
            .await;
    }
    let (_directory, state) = fixture(&server, "failed-sync.db").await;
    state
        .db
        .upsert_model_price(
            "openai/gpt-priority",
            "USD",
            Decimal::from(100),
            Decimal::from(200),
        )
        .await
        .expect("last known manual price");
    let before = serde_json::to_value(
        state
            .db
            .list_model_prices("USD")
            .await
            .expect("prices before failed sync"),
    )
    .unwrap();

    let (status, body) = sync_request(&state, &["openai/gpt-priority"]).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["error"]["code"], "upstream_error");
    assert_eq!(
        body["error"]["message"],
        "configured upstream is unavailable"
    );

    let after = serde_json::to_value(
        state
            .db
            .list_model_prices("USD")
            .await
            .expect("prices after failed sync"),
    )
    .unwrap();
    assert_eq!(after, before, "failed all-source sync must be read-only");
}
