use memeloop_token_center::{
    AppState, api,
    config::Config,
    crypto,
    db::{CreateServiceTokenInput, unix_millis},
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const WEBHOOK_SECRET: &str = "postgres-cloud-principal-ensure-secret-longer-than-32-bytes";

fn ensure_request(tenant: &str, principal: &str) -> Value {
    json!({
        "tenant_external_id": tenant,
        "principal_external_id": principal,
        "currency": "USD"
    })
}

fn subscription(tenant: &str, principal: &str) -> Value {
    json!({
        "tenant_external_id": tenant,
        "principal_external_id": principal,
        "external_subscription_id": "postgres-principal-ensure-subscription",
        "external_cycle_id": "postgres-principal-ensure-cycle",
        "period_start": 1_700_000_000_000_i64,
        "period_end": 4_100_000_000_000_i64,
        "currency": "USD",
        "desired": "10",
        "version": 1,
        "status": "active",
        "policy": {
            "requests_per_minute": 17,
            "tokens_per_minute": 17_000,
            "max_concurrency": 2,
            "daily_budget": null,
            "weekly_budget": null,
            "lifetime_budget": null
        },
        "proration": {"test": "postgres-principal-ensure"}
    })
}

async fn send_subscription(client: &Client, url: &str, body: &Value) -> reqwest::Response {
    let bytes = serde_json::to_vec(body).unwrap();
    let timestamp = (unix_millis() / 1_000).to_string();
    let signature = crypto::sign_webhook_payload(WEBHOOK_SECRET.as_bytes(), &timestamp, &bytes);
    client
        .put(url)
        .header("content-type", "application/json")
        .header(
            "idempotency-key",
            "postgres-principal-ensure-subscription-event",
        )
        .header("x-mtc-webhook-timestamp", timestamp)
        .header("x-mtc-webhook-signature", signature)
        .body(bytes)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn postgres_principal_ensure_is_concurrent_stable_and_subscription_preserving() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let unique = Uuid::now_v7();
    let tenant = format!("postgres-principal-ensure-{unique}");
    let principal = format!("member-{unique}");
    let mut config = Config::for_test(database_url.clone());
    config.memeloop_cloud_webhook_secret = Some(WEBHOOK_SECRET.into());
    let state = AppState::initialize(config).await.unwrap();

    // Scoped service tokens authenticate only against an existing active tenant.
    state.db.create_tenant(&tenant, None).await.unwrap();
    let service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: format!("postgres-principal-ensure-writer-{unique}"),
                scopes: vec!["keys:write".into()],
                tenant_external_id: Some(tenant.clone()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served_state = state.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, api::router(served_state))
            .await
            .unwrap();
    });
    let client = Client::new();
    let ensure_url =
        format!("http://{address}/internal/v1/integrations/memeloop-cloud/principals/ensure");
    let body = ensure_request(&tenant, &principal);

    let first = client
        .post(&ensure_url)
        .bearer_auth(&service.token)
        .json(&body)
        .send();
    let second = client
        .post(&ensure_url)
        .bearer_auth(&service.token)
        .json(&body)
        .send();
    let (first, second) = tokio::join!(first, second);
    let first: Value = first
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let second: Value = second
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["key_id"], second["key_id"]);
    assert_eq!(first["account_id"], second["account_id"]);

    let key_id = Uuid::parse_str(first["key_id"].as_str().unwrap()).unwrap();
    let account_id = Uuid::parse_str(first["account_id"].as_str().unwrap()).unwrap();
    let managed = state
        .db
        .list_managed_keys(Some(&tenant), Some(&principal))
        .await
        .unwrap();
    assert_eq!(managed.len(), 1);
    assert_eq!(managed[0].key_id, key_id);
    assert_eq!(managed[0].account_id, account_id);
    assert_eq!(managed[0].available_balance, "0");
    assert!(
        state
            .db
            .list_account_ledger(account_id, 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        state
            .db
            .list_entitlements(Some(&tenant), Some("memeloop-cloud"), None)
            .await
            .unwrap()
            .is_empty()
    );

    let inspection = PgPool::connect(&database_url).await.unwrap();
    let account_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM credit_accounts a JOIN principals p ON p.id = a.principal_id JOIN tenants t ON t.id = a.tenant_id WHERE t.external_id = $1 AND p.external_id = $2",
    )
    .bind(&tenant)
    .bind(&principal)
    .fetch_one(&inspection)
    .await
    .unwrap();
    assert_eq!(account_count, 1);

    let subscription_url =
        format!("http://{address}/internal/v1/integrations/memeloop-cloud/subscription");
    let snapshot: Value = send_subscription(
        &client,
        &subscription_url,
        &subscription(&tenant, &principal),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(snapshot["credential"]["key_id"], first["key_id"]);
    assert_eq!(snapshot["credential"]["account_id"], first["account_id"]);

    let replay: Value = client
        .post(&ensure_url)
        .bearer_auth(&service.token)
        .json(&body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay["key_id"], first["key_id"]);
    assert_eq!(replay["account_id"], first["account_id"]);
    let managed = state
        .db
        .list_managed_keys(Some(&tenant), Some(&principal))
        .await
        .unwrap();
    assert_eq!(managed.len(), 1);
    assert_eq!(managed[0].available_balance, "10");
    assert_eq!(managed[0].policy.requests_per_minute, 17);
    let account_count_after_subscription: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM credit_accounts a JOIN principals p ON p.id = a.principal_id JOIN tenants t ON t.id = a.tenant_id WHERE t.external_id = $1 AND p.external_id = $2",
    )
    .bind(&tenant)
    .bind(&principal)
    .fetch_one(&inspection)
    .await
    .unwrap();
    assert_eq!(account_count_after_subscription, 1);
    assert_eq!(
        state
            .db
            .list_entitlements(
                Some(&tenant),
                Some("memeloop-cloud"),
                Some("postgres-principal-ensure-subscription"),
            )
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        send_subscription(
            &client,
            &subscription_url,
            &subscription(&tenant, &principal),
        )
        .await
        .status(),
        StatusCode::CREATED
    );

    inspection.close().await;
    server.abort();
}
