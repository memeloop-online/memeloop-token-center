use memeloop_token_center::{AppState, api, config::Config, crypto, db::unix_millis};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

const WEBHOOK_SECRET: &str = "postgres-cloud-webhook-secret-longer-than-32-bytes";

fn snapshot(tenant: &str, principal: &str, version: i64, desired: &str, rpm: u64) -> Value {
    json!({
        "tenant_external_id": tenant,
        "principal_external_id": principal,
        "external_subscription_id": "postgres-cloud-subscription",
        "external_cycle_id": "postgres-cloud-cycle",
        "period_start": 1_700_000_000_000_i64,
        "period_end": 4_100_000_000_000_i64,
        "currency": "USD",
        "desired": desired,
        "version": version,
        "status": "active",
        "policy": {
            "allowed_models": ["gpt-5"],
            "requests_per_minute": rpm,
            "tokens_per_minute": rpm * 1_000,
            "max_concurrency": 2,
            "daily_budget": null,
            "weekly_budget": null,
            "lifetime_budget": null
        },
        "proration": {"test": "postgres"}
    })
}

async fn send(client: &Client, url: &str, event_id: &str, body: &Value) -> reqwest::Response {
    let bytes = serde_json::to_vec(body).unwrap();
    let timestamp = (unix_millis() / 1_000).to_string();
    let signature = crypto::sign_webhook_payload(WEBHOOK_SECRET.as_bytes(), &timestamp, &bytes);
    client
        .put(url)
        .header("content-type", "application/json")
        .header("idempotency-key", event_id)
        .header("x-mtc-webhook-timestamp", timestamp)
        .header("x-mtc-webhook-signature", signature)
        .body(bytes)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn postgres_cloud_events_serialize_versions_and_replay_stable_identity() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let unique = Uuid::now_v7();
    let tenant = format!("postgres-cloud-{unique}");
    let principal = format!("member-{unique}");
    let mut config = Config::for_test(database_url);
    config.memeloop_cloud_webhook_secret = Some(WEBHOOK_SECRET.into());
    let state = AppState::initialize(config).await.unwrap();
    let test_state = state.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, api::router(state)).await });
    let url = format!("http://{address}/internal/v1/integrations/memeloop-cloud/subscription");
    let client = Client::new();

    let v1 = snapshot(&tenant, &principal, 1, "10", 10);
    let first = send(&client, &url, "postgres-cloud-event-1", &v1).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first: Value = first.json().await.unwrap();
    let key_id = first["credential"]["key_id"].clone();
    let account_id = first["credential"]["account_id"].clone();
    let key = first["credential"]["key"].as_str().unwrap().to_owned();
    let replay: Value = send(&client, &url, "postgres-cloud-event-1", &v1)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(replay["credential"]["key_id"], key_id);
    assert_eq!(replay["credential"]["account_id"], account_id);
    assert_eq!(replay["credential"]["key"], key);

    assert_eq!(
        send(
            &client,
            &url,
            "postgres-cloud-event-3",
            &snapshot(&tenant, &principal, 3, "30", 30),
        )
        .await
        .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        send(
            &client,
            &url,
            "postgres-cloud-event-2",
            &snapshot(&tenant, &principal, 2, "20", 20),
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let authenticated = test_state
        .db
        .authenticate_key(&key, test_state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert_eq!(authenticated.policy.requests_per_minute, 30);
    assert_eq!(
        test_state
            .db
            .key_view(&authenticated)
            .await
            .unwrap()
            .available_balance,
        "30"
    );
    server.abort();
}
