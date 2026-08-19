use memeloop_token_center::{AppState, api, config::Config, crypto};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;

const WEBHOOK_SECRET: &str = "test-memeloop-cloud-webhook-secret-long-enough";

struct Fixture {
    _directory: TempDir,
    client: Client,
    base_url: String,
    state: AppState,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("cloud-entitlements.db").display()
        );
        let mut config = Config::for_test(database_url);
        config.memeloop_cloud_webhook_secret = Some(WEBHOOK_SECRET.to_owned());
        let state = AppState::initialize(config).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let served_state = state.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, api::router(served_state))
                .await
                .unwrap();
        });
        Self {
            _directory: directory,
            client: Client::new(),
            base_url: format!("http://{address}"),
            state,
            server,
        }
    }

    async fn send(&self, event_id: &str, body: &Value) -> reqwest::Response {
        self.send_at(
            event_id,
            body,
            memeloop_token_center::db::unix_millis() / 1_000,
        )
        .await
    }

    async fn send_at(&self, event_id: &str, body: &Value, timestamp: i64) -> reqwest::Response {
        let bytes = serde_json::to_vec(body).unwrap();
        let timestamp = timestamp.to_string();
        let signature = crypto::sign_webhook_payload(WEBHOOK_SECRET.as_bytes(), &timestamp, &bytes);
        self.client
            .put(format!(
                "{}/internal/v1/integrations/memeloop-cloud/subscription",
                self.base_url
            ))
            .header("content-type", "application/json")
            .header("idempotency-key", event_id)
            .header("x-mtc-webhook-timestamp", timestamp)
            .header("x-mtc-webhook-signature", signature)
            .body(bytes)
            .send()
            .await
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn policy(rpm: u64, models: &[&str]) -> Value {
    json!({
        "allowed_models": models,
        "requests_per_minute": rpm,
        "tokens_per_minute": rpm * 1_000,
        "max_concurrency": 4,
        "daily_budget": null,
        "weekly_budget": null,
        "lifetime_budget": null
    })
}

fn active(
    tenant: &str,
    principal: &str,
    subscription: &str,
    cycle: &str,
    desired: &str,
    version: i64,
    rpm: u64,
) -> Value {
    json!({
        "tenant_external_id": tenant,
        "principal_external_id": principal,
        "external_subscription_id": subscription,
        "external_cycle_id": cycle,
        "period_start": 1_700_000_000_000_i64,
        "period_end": 4_100_000_000_000_i64,
        "currency": "USD",
        "desired": desired,
        "version": version,
        "status": "active",
        "policy": policy(rpm, &["gpt-5"]),
        "proration": {"plan": "pro"}
    })
}

fn cancelled(
    tenant: &str,
    principal: &str,
    subscription: &str,
    cycle: &str,
    version: i64,
) -> Value {
    json!({
        "tenant_external_id": tenant,
        "principal_external_id": principal,
        "external_subscription_id": subscription,
        "external_cycle_id": cycle,
        "period_start": null,
        "period_end": null,
        "currency": "USD",
        "desired": null,
        "version": version,
        "status": "cancelled",
        "policy": policy(1, &[]),
        "proration": null
    })
}

#[tokio::test]
async fn signed_cloud_lifecycle_is_idempotent_ordered_and_keeps_stable_history() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-tenant-a";
    let principal = "cloud-user-42";
    let subscription = "subscription-42";

    let first_body = active(tenant, principal, subscription, "cycle-1", "10", 1, 60);
    let first = fixture.send("cloud-event-1", &first_body).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first: Value = first.json().await.unwrap();
    let key_id = first["credential"]["key_id"].as_str().unwrap();
    let account_id = first["credential"]["account_id"].as_str().unwrap();
    let key = first["credential"]["key"].as_str().unwrap().to_owned();
    assert_eq!(first["entitlement"]["desired"], "10");
    assert_eq!(first["entitlement"]["remaining"], "10");

    let replay: Value = fixture
        .send("cloud-event-1", &first_body)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(replay["credential"]["key_id"], key_id);
    assert_eq!(replay["credential"]["account_id"], account_id);
    assert_eq!(replay["credential"]["key"], key);
    assert_eq!(replay["entitlement"], first["entitlement"]);

    let mut conflicting_replay = first_body.clone();
    conflicting_replay["policy"] = policy(61, &["gpt-5"]);
    assert_eq!(
        fixture
            .send("cloud-event-1", &conflicting_replay)
            .await
            .status(),
        StatusCode::CONFLICT
    );

    let forged_principal = active(
        tenant,
        "another-cloud-user",
        subscription,
        "cycle-1",
        "20",
        2,
        200,
    );
    assert_eq!(
        fixture
            .send("cloud-event-forged-principal", &forged_principal)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        fixture
            .state
            .db
            .list_managed_keys(Some(tenant), Some("another-cloud-user"))
            .await
            .unwrap()
            .is_empty(),
        "a rejected cross-principal event must not leave an orphan credential"
    );

    let upgraded = fixture
        .send(
            "cloud-event-3",
            &active(tenant, principal, subscription, "cycle-1", "30", 3, 300),
        )
        .await;
    assert_eq!(upgraded.status(), StatusCode::CREATED);
    let upgraded: Value = upgraded.json().await.unwrap();
    assert_eq!(upgraded["entitlement"]["remaining"], "30");
    assert_eq!(upgraded["policy"]["requests_per_minute"], 300);

    // A delayed v2 event is rejected and cannot roll quota or policy back.
    assert_eq!(
        fixture
            .send(
                "cloud-event-2-late",
                &active(tenant, principal, subscription, "cycle-1", "20", 2, 200),
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let authenticated = fixture
        .state
        .db
        .authenticate_key(&key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert_eq!(authenticated.key_id.to_string(), key_id);
    assert_eq!(authenticated.account_id.to_string(), account_id);
    assert_eq!(authenticated.policy.requests_per_minute, 300);

    let renewed = fixture
        .send(
            "cloud-event-4",
            &active(tenant, principal, subscription, "cycle-2", "20", 4, 200),
        )
        .await;
    assert_eq!(renewed.status(), StatusCode::CREATED);
    let renewed: Value = renewed.json().await.unwrap();
    assert_eq!(renewed["entitlement"]["external_cycle_id"], "cycle-2");
    assert_eq!(renewed["entitlement"]["remaining"], "20");
    assert_eq!(renewed["entitlement"]["ledger_delta"], "-10");

    let downgraded: Value = fixture
        .send(
            "cloud-event-5",
            &active(tenant, principal, subscription, "cycle-2", "5", 5, 50),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(downgraded["entitlement"]["remaining"], "5");
    assert_eq!(downgraded["entitlement"]["ledger_delta"], "-15");

    let cancellation = fixture
        .send(
            "cloud-event-6",
            &cancelled(tenant, principal, subscription, "cycle-2", 6),
        )
        .await;
    assert_eq!(cancellation.status(), StatusCode::OK);
    let cancellation: Value = cancellation.json().await.unwrap();
    assert_eq!(cancellation["entitlement"]["status"], "cancelled");
    assert_eq!(cancellation["entitlement"]["remaining"], "0");
    assert_eq!(cancellation["entitlement"]["ledger_delta"], "-5");

    let reactivated: Value = fixture
        .send(
            "cloud-event-7",
            &active(tenant, principal, subscription, "cycle-2", "8", 7, 80),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(reactivated["credential"]["key_id"], key_id);
    assert_eq!(reactivated["credential"]["account_id"], account_id);
    assert_eq!(reactivated["entitlement"]["status"], "active");
    assert_eq!(reactivated["entitlement"]["remaining"], "8");
    fixture
        .state
        .db
        .authenticate_key(&key, fixture.state.config.key_pepper.as_bytes())
        .await
        .expect("the original credential and its history remain attached");

    let rotated = fixture
        .state
        .db
        .rotate_key(
            authenticated.key_id,
            "cloud-credential-rotation",
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let after_rotation: Value = fixture
        .send(
            "cloud-event-8",
            &active(tenant, principal, subscription, "cycle-2", "9", 8, 90),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(after_rotation["credential"]["key_id"], key_id);
    assert_eq!(after_rotation["credential"]["account_id"], account_id);
    assert_eq!(after_rotation["credential"]["credential_generation"], 2);
    assert!(after_rotation["credential"]["key"].is_null());
    assert!(
        fixture
            .state
            .db
            .authenticate_key(&key, fixture.state.config.key_pepper.as_bytes())
            .await
            .is_err()
    );
    let rotated_auth = fixture
        .state
        .db
        .authenticate_key(&rotated.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert_eq!(rotated_auth.key_id.to_string(), key_id);
    assert_eq!(rotated_auth.account_id.to_string(), account_id);
    assert_eq!(rotated_auth.policy.requests_per_minute, 90);

    let other_tenant: Value = fixture
        .send(
            "cloud-event-other-tenant",
            &active(
                "cloud-tenant-b",
                principal,
                "subscription-42-b",
                "cycle-b",
                "3",
                1,
                30,
            ),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_ne!(other_tenant["credential"]["key_id"], key_id);
    assert_ne!(other_tenant["credential"]["account_id"], account_id);
}

#[tokio::test]
async fn cloud_webhook_fails_closed_for_invalid_signatures_and_stale_timestamps() {
    let fixture = Fixture::new().await;
    let body = active("cloud-auth", "member", "subscription", "cycle", "1", 1, 10);
    let bytes = serde_json::to_vec(&body).unwrap();
    let timestamp = (memeloop_token_center::db::unix_millis() / 1_000).to_string();
    let invalid = fixture
        .client
        .put(format!(
            "{}/internal/v1/integrations/memeloop-cloud/subscription",
            fixture.base_url
        ))
        .header("content-type", "application/json")
        .header("idempotency-key", "invalid-signature")
        .header("x-mtc-webhook-timestamp", &timestamp)
        .header("x-mtc-webhook-signature", "v1=invalid")
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);

    let stale = fixture
        .send_at(
            "stale-timestamp",
            &body,
            memeloop_token_center::db::unix_millis() / 1_000 - 301,
        )
        .await;
    assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn invalid_or_unknown_snapshots_leave_no_orphan_credential() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-invalid";
    let principal = "invalid-member";
    let mut invalid_policy = active(tenant, principal, "subscription", "cycle", "1", 1, 10);
    invalid_policy["policy"]["requests_per_minute"] = json!(0);
    assert_eq!(
        fixture
            .send("invalid-policy", &invalid_policy)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(
        fixture
            .state
            .db
            .list_managed_keys(Some(tenant), Some(principal))
            .await
            .unwrap()
            .is_empty()
    );

    let mut missing_cycle = active(tenant, principal, "subscription", "cycle", "1", 1, 10);
    missing_cycle["external_cycle_id"] = Value::Null;
    assert_eq!(
        fixture.send("missing-cycle", &missing_cycle).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert!(
        fixture
            .state
            .db
            .list_managed_keys(Some(tenant), Some(principal))
            .await
            .unwrap()
            .is_empty()
    );

    assert_eq!(
        fixture
            .send(
                "unknown-cancellation",
                &cancelled(tenant, principal, "subscription", "cycle", 2),
            )
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        fixture
            .state
            .db
            .list_managed_keys(Some(tenant), Some(principal))
            .await
            .unwrap()
            .is_empty()
    );
}
