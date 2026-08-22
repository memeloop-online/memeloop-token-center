use super::TokenCenterWorld;
use cucumber::{then, when};
use memeloop_token_center::{crypto, db::unix_millis};
use reqwest::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

const WEBHOOK_SECRET: &str = "test-memeloop-cloud-webhook-secret-long-enough";

fn policy(rpm: u64) -> Value {
    json!({
        "requests_per_minute": rpm,
        "tokens_per_minute": rpm * 1_000,
        "max_concurrency": 4,
        "daily_budget": null,
        "weekly_budget": null,
        "lifetime_budget": null
    })
}

fn active(version: i64, desired: &str, rpm: u64) -> Value {
    json!({
        "tenant_external_id": "cucumber-cloud-tenant",
        "principal_external_id": "cucumber-cloud-member",
        "external_subscription_id": "cucumber-cloud-subscription",
        "external_cycle_id": "cucumber-cloud-cycle",
        "period_start": 1_700_000_000_000_i64,
        "period_end": 4_100_000_000_000_i64,
        "currency": "USD",
        "desired": desired,
        "version": version,
        "status": "active",
        "policy": policy(rpm),
        "proration": {"plan": "cucumber-pro"}
    })
}

fn cancelled(version: i64) -> Value {
    json!({
        "tenant_external_id": "cucumber-cloud-tenant",
        "principal_external_id": "cucumber-cloud-member",
        "external_subscription_id": "cucumber-cloud-subscription",
        "external_cycle_id": "cucumber-cloud-cycle",
        "period_start": null,
        "period_end": null,
        "currency": "USD",
        "desired": null,
        "version": version,
        "status": "cancelled",
        "policy": policy(1),
        "proration": null
    })
}

async fn send(world: &mut TokenCenterWorld, event_id: &str, body: Value) {
    let bytes = serde_json::to_vec(&body).expect("Cloud webhook JSON");
    let timestamp = (unix_millis() / 1_000).to_string();
    let signature = crypto::sign_webhook_payload(WEBHOOK_SECRET.as_bytes(), &timestamp, &bytes);
    let response = world
        .client
        .put(format!(
            "{}/internal/v1/integrations/memeloop-cloud/subscription",
            world.service_url
        ))
        .header("content-type", "application/json")
        .header("idempotency-key", event_id)
        .header("x-mtc-webhook-timestamp", timestamp)
        .header("x-mtc-webhook-signature", signature)
        .body(bytes)
        .send()
        .await
        .expect("signed Cloud webhook response");
    world.status = Some(response.status());
    world.response = response.json().await.expect("Cloud webhook response JSON");
}

#[when("MemeLoop Cloud signs an initial subscription snapshot")]
async fn initial_subscription(world: &mut TokenCenterWorld) {
    send(world, "cucumber-cloud-event-1", active(1, "10", 60)).await;
    assert_eq!(
        world.status,
        Some(StatusCode::CREATED),
        "{}",
        world.response
    );
    world.current_key = world.response["credential"]["key"]
        .as_str()
        .expect("issued stable credential")
        .to_owned();
    world.stable_key_id = Some(
        Uuid::parse_str(
            world.response["credential"]["key_id"]
                .as_str()
                .expect("stable key id"),
        )
        .expect("key UUID"),
    );
    world.stable_account_id = Some(
        Uuid::parse_str(
            world.response["credential"]["account_id"]
                .as_str()
                .expect("stable account id"),
        )
        .expect("account UUID"),
    );
}

#[then("the Cloud snapshot creates one stable credential with exact quota and policy")]
async fn initial_snapshot_is_exact(world: &mut TokenCenterWorld) {
    assert_eq!(world.response["entitlement"]["remaining"], "10");
    assert_eq!(world.response["policy"]["requests_per_minute"], 60);
    let state = world.state.as_ref().expect("test state");
    let key = state
        .db
        .authenticate_key(&world.current_key, state.config.key_pepper.as_bytes())
        .await
        .expect("issued Cloud key authenticates");
    assert_eq!(Some(key.key_id), world.stable_key_id);
    assert_eq!(Some(key.account_id), world.stable_account_id);
}

#[when("MemeLoop Cloud retries the same signed event")]
async fn retry_same_event(world: &mut TokenCenterWorld) {
    let stable_key_id = world.stable_key_id;
    let stable_account_id = world.stable_account_id;
    let original_key = world.current_key.clone();
    send(world, "cucumber-cloud-event-1", active(1, "10", 60)).await;
    assert_eq!(world.status, Some(StatusCode::CREATED));
    assert_eq!(
        world.response["credential"]["key_id"],
        stable_key_id.expect("stable key id").to_string()
    );
    assert_eq!(
        world.response["credential"]["account_id"],
        stable_account_id.expect("stable account id").to_string()
    );
    assert_eq!(world.response["credential"]["key"], original_key);
}

#[then("the Cloud retry does not duplicate credit or credential history")]
async fn retry_does_not_duplicate(world: &mut TokenCenterWorld) {
    assert_eq!(world.response["entitlement"]["desired"], "10");
    assert_eq!(world.response["entitlement"]["ledger_delta"], "10");
    let state = world.state.as_ref().expect("test state");
    let keys = state
        .db
        .list_managed_keys(Some("cucumber-cloud-tenant"), Some("cucumber-cloud-member"))
        .await
        .expect("Cloud principal keys");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].available_balance, "10");
}

#[when("MemeLoop Cloud applies version 3 and then delivers version 2")]
async fn out_of_order_delivery(world: &mut TokenCenterWorld) {
    send(world, "cucumber-cloud-event-3", active(3, "30", 300)).await;
    assert_eq!(world.status, Some(StatusCode::CREATED));
    send(world, "cucumber-cloud-event-2", active(2, "20", 200)).await;
}

#[then("the stale Cloud event is rejected without rolling quota or policy back")]
async fn stale_is_rejected(world: &mut TokenCenterWorld) {
    assert_eq!(world.status, Some(StatusCode::CONFLICT));
    let state = world.state.as_ref().expect("test state");
    let authenticated = state
        .db
        .authenticate_key(&world.current_key, state.config.key_pepper.as_bytes())
        .await
        .expect("stable Cloud credential");
    assert_eq!(authenticated.policy.requests_per_minute, 300);
    let view = state
        .db
        .key_view(&authenticated)
        .await
        .expect("stable key view");
    assert_eq!(view.available_balance, "30");
}

#[when("MemeLoop Cloud signs a newer cancellation snapshot")]
async fn cancel_subscription(world: &mut TokenCenterWorld) {
    send(world, "cucumber-cloud-event-4", cancelled(4)).await;
}

#[then("only the unconsumed subscription remainder is withdrawn")]
async fn cancellation_withdraws_remainder(world: &mut TokenCenterWorld) {
    assert_eq!(world.status, Some(StatusCode::OK));
    assert_eq!(world.response["entitlement"]["status"], "cancelled");
    assert_eq!(world.response["entitlement"]["remaining"], "0");
    assert_eq!(world.response["entitlement"]["ledger_delta"], "-30");
    assert_eq!(
        world.response["credential"]["key_id"],
        world.stable_key_id.expect("stable key id").to_string()
    );
}
