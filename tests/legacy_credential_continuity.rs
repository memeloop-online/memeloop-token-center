use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::Config,
    crypto,
    db::{CreateKeyInput, FinishRequest, NewRequest},
    model::KeyPolicy,
};
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::AnyPool;
use tower::ServiceExt;
use uuid::Uuid;

async fn request(
    state: &AppState,
    method: &str,
    path: &str,
    bearer: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    let body = if let Some(value) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(serde_json::to_vec(&value).unwrap())
    } else {
        Body::empty()
    };
    let response = api::router(state.clone())
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

#[tokio::test]
async fn opaque_normal_credential_preserves_identity_history_policy_and_balance() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("legacy-continuity.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url.clone()))
        .await
        .unwrap();
    let expected_policy = KeyPolicy {
        allowed_models: vec!["legacy-model".into()],
        requests_per_minute: 7,
        tokens_per_minute: 7000,
        max_concurrency: 2,
        daily_budget: Some("5".into()),
        weekly_budget: Some("20".into()),
        lifetime_budget: Some("50".into()),
    };
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "legacy-fixture-tenant".into(),
                principal_external_id: "legacy-linux-codex".into(),
                alias: "legacy fixture".into(),
                currency: "USD".into(),
                policy: expected_policy.clone(),
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let native = state
        .db
        .authenticate_key(&issued.key, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let price = state
        .db
        .upsert_model_price("legacy-model", "USD", Decimal::ZERO, Decimal::ZERO)
        .await
        .unwrap();
    let reservation = state
        .db
        .reserve_usage(&native, &price, 11, 7)
        .await
        .unwrap();
    let historical_request_id = Uuid::now_v7();
    state
        .db
        .record_request_started(NewRequest {
            request_id: historical_request_id,
            key_id: native.key_id,
            tenant_id: native.tenant_id,
            protocol: "openai-responses".into(),
            model: "legacy-model".into(),
            request_object: "gap://legacy-fixture/request".into(),
            reservation_id: reservation.id,
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let cost_micros = state.db.settle_usage(&reservation, 11, 7).await.unwrap();
    state
        .db
        .record_request_finished(FinishRequest {
            request_id: historical_request_id,
            status_code: 200,
            duration_ms: 42,
            input_tokens: 11,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 7,
            service_tier: None,
            cost_micros,
            error_code: None,
            response_object: "gap://legacy-fixture/response".into(),
        })
        .await
        .unwrap();

    let (status, before_key) = request(&state, "GET", "/self/v1/key", &issued.key, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, before_stats) = request(&state, "GET", "/self/v1/stats", &issued.key, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(before_stats["summary"]["total_requests"], 1);

    let opaque = "fixture-only-opaque-cpa-client-key-0001";
    let (secret_hash, fingerprint) =
        crypto::hash_credential(opaque, state.config.key_pepper.as_bytes());
    let pool = AnyPool::connect(&database_url).await.unwrap();
    sqlx::query(
        "UPDATE key_credentials SET secret_hash=$1,fingerprint=$2 WHERE key_id=$3 AND generation=1 AND revoked_at IS NULL",
    )
    .bind(secret_hash)
    .bind(fingerprint)
    .bind(issued.key_id.to_string())
    .execute(&pool)
    .await
    .unwrap();

    let (status, opaque_key) = request(&state, "GET", "/self/v1/key", opaque, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(opaque_key, before_key);
    assert_eq!(opaque_key["key_id"], issued.key_id.to_string());
    assert_eq!(
        opaque_key["policy"],
        serde_json::to_value(expected_policy).unwrap()
    );
    assert_eq!(opaque_key["available_balance"], "10");

    let (status, opaque_stats) = request(&state, "GET", "/self/v1/stats", opaque, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(opaque_stats, before_stats);
    assert_eq!(opaque_stats["summary"]["total_requests"], 1);
    assert_eq!(opaque_stats["summary"]["input_tokens"], 11);
    assert_eq!(opaque_stats["summary"]["output_tokens"], 7);

    let (status, requests) = request(&state, "GET", "/self/v1/requests", opaque, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(requests.as_array().map(Vec::len), Some(1));
    assert_eq!(requests[0]["request_id"], historical_request_id.to_string());
}
