use memeloop_token_center::{
    AppState, api,
    config::Config,
    crypto,
    db::{
        CreateGroupInput, CreateModelRouteInput, CreateServiceTokenInput,
        CreateUpstreamAccountInput, DiscoveredUpstreamModel, GroupKind,
        ReplaceCredentialRoutingInput, ReplaceGroupMembersInput,
    },
    provider::UpstreamCredential,
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tempfile::TempDir;
use tokio::task::JoinHandle;
use uuid::Uuid;

const WEBHOOK_SECRET: &str = "test-memeloop-cloud-webhook-secret-long-enough";
static FIXTURE_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

struct Fixture {
    _permit: tokio::sync::SemaphorePermit<'static>,
    _directory: TempDir,
    database_url: String,
    client: Client,
    base_url: String,
    state: AppState,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        // Production deliberately admits at most four webhook bodies at once.
        // Keep this integration-test process within that same global budget so
        // parallel test fixtures cannot steal permits from one another.
        let permit = FIXTURE_PERMITS.acquire().await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("cloud-entitlements.db").display()
        );
        let mut config = Config::for_test(database_url.clone());
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
            _permit: permit,
            _directory: directory,
            database_url,
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

fn framed_digest(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((*part).len().to_be_bytes());
        digest.update(part);
    }
    format!("{:x}", digest.finalize())
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

async fn seed_route(state: &AppState, tenant: &str, model: &str, priority: i64) -> Uuid {
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.into(),
                name: format!("cloud-upstream-{priority}"),
                driver: "http-json".into(),
                config: json!({
                    "base_url": format!("http://127.0.0.1:{}", 18_000 + priority),
                    "network_scope": "private"
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
    let lease_id = Uuid::now_v7();
    assert!(
        state
            .db
            .claim_upstream_model_catalog_sync(
                account.id,
                tenant,
                account.credential_generation,
                lease_id,
            )
            .await
            .unwrap()
    );
    state
        .db
        .replace_upstream_model_catalog(
            account.id,
            tenant,
            account.credential_generation,
            lease_id,
            "component",
            &[DiscoveredUpstreamModel {
                model_id: model.into(),
                protocol: "openai".into(),
                context_window: Some(128_000),
                reservation_token_bound: Some(16_384),
                reservation_bound_source: Some("test".into()),
            }],
        )
        .await
        .unwrap();
    state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.into(),
            public_model: model.into(),
            upstream_account_id: account.id,
            upstream_model: model.into(),
            protocol: "openai".into(),
            priority,
        })
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn cloud_grants_are_snapshot_scoped_and_cancel_fails_closed() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-routing";
    let principal = "cloud-routing-user";
    let subscription = "cloud-routing-subscription";
    let original_route = seed_route(&fixture.state, tenant, "gpt-5", 1).await;

    let first: Value = fixture
        .send(
            "cloud-routing-1",
            &active(tenant, principal, subscription, "cycle", "10", 1, 60),
        )
        .await
        .json()
        .await
        .unwrap();
    let key_id = Uuid::parse_str(first["credential"]["key_id"].as_str().unwrap()).unwrap();
    let first_routing = fixture
        .state
        .db
        .credential_routing(key_id, tenant)
        .await
        .unwrap();
    assert_eq!(first_routing.effective_route_ids, vec![original_route]);

    // Legacy `allowed_models` is converted only once. A future route with the
    // same public model must not be implicitly authorized.
    let future_route = seed_route(&fixture.state, tenant, "gpt-5", 2).await;
    let after_future_route = fixture
        .state
        .db
        .credential_routing(key_id, tenant)
        .await
        .unwrap();
    assert_eq!(after_future_route.effective_route_ids, vec![original_route]);

    let mut invalid_grant = active(tenant, principal, subscription, "cycle", "11", 2, 70);
    invalid_grant["route_ids"] = json!([Uuid::now_v7()]);
    assert_eq!(
        fixture
            .send("cloud-routing-2", &invalid_grant)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let after_failed_snapshot = fixture
        .state
        .db
        .list_entitlements(Some(tenant), Some("memeloop-cloud"), Some(subscription))
        .await
        .unwrap();
    assert_eq!(after_failed_snapshot[0].version, 1);
    assert_eq!(after_failed_snapshot[0].remaining, "10");
    assert_eq!(
        fixture
            .state
            .db
            .credential_routing(key_id, tenant)
            .await
            .unwrap()
            .effective_route_ids,
        vec![original_route]
    );

    let route_group = fixture
        .state
        .db
        .create_group(
            GroupKind::Route,
            CreateGroupInput {
                tenant_external_id: tenant.into(),
                name: "Cloud plan routes".into(),
            },
        )
        .await
        .unwrap();
    let route_group = fixture
        .state
        .db
        .replace_group_members(
            GroupKind::Route,
            route_group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: tenant.into(),
                member_ids: vec![future_route],
                expected_updated_at: route_group.updated_at,
            },
        )
        .await
        .unwrap();
    let mut explicit = active(tenant, principal, subscription, "cycle", "11", 2, 70);
    explicit["route_ids"] = json!([]);
    explicit["route_group_ids"] = json!([route_group.id]);
    // The failed full transaction did not consume its event identity.
    let response = fixture.send("cloud-routing-2", &explicit).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let explicit_routing = fixture
        .state
        .db
        .credential_routing(key_id, tenant)
        .await
        .unwrap();
    let operator_revision_before_cloud_cancel = explicit_routing.grant_revision;
    assert_eq!(explicit_routing.effective_route_ids, vec![future_route]);
    assert_eq!(explicit_routing.route_group_ids, vec![route_group.id]);
    assert_eq!(
        fixture.send("cloud-routing-2", &explicit).await.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        fixture
            .state
            .db
            .credential_routing(key_id, tenant)
            .await
            .unwrap()
            .grant_revision,
        operator_revision_before_cloud_cancel,
        "an idempotent replay must not create a phantom grant revision"
    );
    let tenant_id = fixture
        .state
        .db
        .list_model_routes(Some(tenant))
        .await
        .unwrap()[0]
        .tenant_id;
    let available_before = fixture
        .state
        .db
        .granted_available_models(key_id, tenant_id)
        .await
        .unwrap();
    let credential_group = fixture
        .state
        .db
        .create_group(
            GroupKind::Credential,
            CreateGroupInput {
                tenant_external_id: tenant.into(),
                name: "Cloud customers".into(),
            },
        )
        .await
        .unwrap();
    let credential_group = fixture
        .state
        .db
        .replace_group_members(
            GroupKind::Credential,
            credential_group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: tenant.into(),
                member_ids: vec![key_id],
                expected_updated_at: credential_group.updated_at,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        fixture
            .state
            .db
            .credential_routing(key_id, tenant)
            .await
            .unwrap()
            .effective_route_ids,
        vec![future_route],
        "credential groups are presentation-only and cannot alter grants"
    );
    assert_eq!(
        fixture
            .state
            .db
            .granted_available_models(key_id, tenant_id)
            .await
            .unwrap(),
        available_before,
        "credential group membership cannot alter available models"
    );
    fixture
        .state
        .db
        .replace_group_members(
            GroupKind::Credential,
            credential_group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: tenant.into(),
                member_ids: vec![],
                expected_updated_at: credential_group.updated_at,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        fixture
            .state
            .db
            .credential_routing(key_id, tenant)
            .await
            .unwrap()
            .effective_route_ids,
        vec![future_route]
    );

    let cancellation = fixture
        .send(
            "cloud-routing-3",
            &cancelled(tenant, principal, subscription, "cycle", 3),
        )
        .await;
    assert_eq!(cancellation.status(), StatusCode::OK);
    assert!(
        fixture
            .state
            .db
            .credential_routing(key_id, tenant)
            .await
            .unwrap()
            .effective_route_ids
            .is_empty()
    );
    let after_cancel_routing = fixture
        .state
        .db
        .credential_routing(key_id, tenant)
        .await
        .unwrap();
    assert!(after_cancel_routing.grant_revision > operator_revision_before_cloud_cancel);
    assert!(matches!(
        fixture
            .state
            .db
            .replace_credential_routing(
                key_id,
                ReplaceCredentialRoutingInput {
                    tenant_external_id: tenant.into(),
                    route_ids: vec![original_route],
                    route_group_ids: vec![],
                    expected_grant_revision: operator_revision_before_cloud_cancel,
                },
            )
            .await,
        Err(memeloop_token_center::error::AppError::Conflict(_))
    ));

    // Presence of an empty normalized contract remains deny-all on
    // reactivation; it must not fall back to the legacy model list.
    let mut denied = active(tenant, principal, subscription, "cycle", "4", 4, 40);
    denied["route_ids"] = json!([]);
    let denied_response = fixture.send("cloud-routing-4", &denied).await;
    assert_eq!(denied_response.status(), StatusCode::CREATED);
    assert!(
        fixture
            .state
            .db
            .credential_routing(key_id, tenant)
            .await
            .unwrap()
            .effective_route_ids
            .is_empty()
    );
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

#[tokio::test]
async fn concurrent_first_cloud_events_cannot_leave_a_losing_principal_key() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-concurrent-first";
    let subscription = "shared-subscription";
    let first = active(tenant, "principal-a", subscription, "cycle", "10", 1, 10);
    let second = active(tenant, "principal-b", subscription, "cycle", "20", 2, 20);
    let (first, second) = tokio::join!(
        fixture.send("concurrent-first-a", &first),
        fixture.send("concurrent-first-b", &second),
    );
    assert!(first.status().is_success() ^ second.status().is_success());
    assert!(matches!(
        (first.status(), second.status()),
        (
            StatusCode::CREATED,
            StatusCode::FORBIDDEN | StatusCode::CONFLICT
        ) | (
            StatusCode::FORBIDDEN | StatusCode::CONFLICT,
            StatusCode::CREATED
        )
    ));
    let keys = fixture
        .state
        .db
        .list_managed_keys(Some(tenant), None)
        .await
        .unwrap();
    assert_eq!(
        keys.len(),
        1,
        "the losing transaction must not orphan a key"
    );
}

#[tokio::test]
async fn late_audit_conflict_rolls_back_entitlement_policy_routing_and_replay_state() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-atomic-rollback";
    let principal = "atomic-member";
    let subscription = "atomic-subscription";
    let original_route = seed_route(&fixture.state, tenant, "gpt-5", 11).await;
    let replacement_route = seed_route(&fixture.state, tenant, "gpt-5", 12).await;

    let mut initial = active(tenant, principal, subscription, "cycle", "10", 1, 10);
    initial["route_ids"] = json!([original_route]);
    let initial: Value = fixture
        .send("cloud-atomic-initial", &initial)
        .await
        .json()
        .await
        .unwrap();
    let key_id = Uuid::parse_str(initial["credential"]["key_id"].as_str().unwrap()).unwrap();

    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let original_audit = sqlx::query(
        "SELECT event_key_hash, request_hash FROM memeloop_cloud_subscription_events WHERE key_id = $1",
    )
    .bind(key_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let original_event_hash: String = original_audit.try_get("event_key_hash").unwrap();
    let original_request_hash: String = original_audit.try_get("request_hash").unwrap();

    // Reserve the next webhook audit identity with conflicting metadata. The
    // collision is detected only after entitlement, policy, and routing work,
    // so a 409 proves that every earlier mutation shares the same transaction.
    let retry_event_id = "cloud-atomic-late-audit-conflict";
    let conflicting_event_hash = framed_digest(&[tenant.as_bytes(), retry_event_id.as_bytes()]);
    let changed = sqlx::query(
        "UPDATE memeloop_cloud_subscription_events SET event_key_hash = $1, request_hash = $2 WHERE key_id = $3",
    )
    .bind(&conflicting_event_hash)
    .bind("f".repeat(64))
    .bind(key_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(changed.rows_affected(), 1);

    let mut update = active(tenant, principal, subscription, "cycle", "20", 2, 20);
    update["route_ids"] = json!([replacement_route]);
    assert_eq!(
        fixture.send(retry_event_id, &update).await.status(),
        StatusCode::CONFLICT
    );

    let keys = fixture
        .state
        .db
        .list_managed_keys(Some(tenant), Some(principal))
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].available_balance, "10");
    assert_eq!(keys[0].policy.requests_per_minute, 10);
    let entitlement = fixture
        .state
        .db
        .list_entitlements(Some(tenant), Some("memeloop-cloud"), Some(subscription))
        .await
        .unwrap();
    assert_eq!(entitlement[0].version, 1);
    assert_eq!(entitlement[0].remaining, "10");
    let routing = fixture
        .state
        .db
        .credential_routing(key_id, tenant)
        .await
        .unwrap();
    assert_eq!(routing.effective_route_ids, vec![original_route]);

    // Restore the pre-existing audit row and retry the exact failed event. It
    // must execute as a fresh version-2 snapshot, proving the reconciliation
    // replay row was rolled back with the rest of the transaction.
    sqlx::query(
        "UPDATE memeloop_cloud_subscription_events SET event_key_hash = $1, request_hash = $2 WHERE key_id = $3",
    )
    .bind(original_event_hash)
    .bind(original_request_hash)
    .bind(key_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let retried = fixture.send(retry_event_id, &update).await;
    assert_eq!(retried.status(), StatusCode::CREATED);
    let retried: Value = retried.json().await.unwrap();
    assert_eq!(retried["entitlement"]["version"], 2);
    assert_eq!(retried["entitlement"]["remaining"], "20");
    assert_eq!(retried["policy"]["requests_per_minute"], 20);
    let routing = fixture
        .state
        .db
        .credential_routing(key_id, tenant)
        .await
        .unwrap();
    assert_eq!(routing.effective_route_ids, vec![replacement_route]);
    pool.close().await;
}

#[tokio::test]
async fn entitlement_and_event_queries_are_bound_to_service_tenant_or_authenticated_key() {
    let fixture = Fixture::new().await;
    let first_tenant = "cloud-query-a";
    let second_tenant = "cloud-query-b";
    let first: Value = fixture
        .send(
            "cloud-query-event-a",
            &active(
                first_tenant,
                "member-a",
                "subscription-a",
                "cycle-a",
                "7",
                1,
                7,
            ),
        )
        .await
        .json()
        .await
        .unwrap();
    let second: Value = fixture
        .send(
            "cloud-query-event-b",
            &active(
                second_tenant,
                "member-b",
                "subscription-b",
                "cycle-b",
                "9",
                1,
                9,
            ),
        )
        .await
        .json()
        .await
        .unwrap();
    let first_key = first["credential"]["key"].as_str().unwrap();
    let first_key_id = first["credential"]["key_id"].as_str().unwrap();
    let second_key_id = second["credential"]["key_id"].as_str().unwrap();

    let self_view = fixture
        .client
        .get(format!(
            "{}/self/v1/entitlements?tenant_external_id={second_tenant}&key_id={second_key_id}",
            fixture.base_url
        ))
        .bearer_auth(first_key)
        .send()
        .await
        .unwrap();
    assert_eq!(self_view.status(), StatusCode::OK);
    assert_eq!(self_view.headers()["cache-control"], "no-store");
    let self_view: Value = self_view.json().await.unwrap();
    assert_eq!(self_view["entitlements"].as_array().unwrap().len(), 1);
    assert_eq!(
        self_view["entitlements"][0]["tenant_external_id"],
        first_tenant
    );
    assert_eq!(self_view["events"].as_array().unwrap().len(), 1);
    assert_eq!(self_view["events"][0]["key_id"], first_key_id);
    let serialized = serde_json::to_string(&self_view).unwrap();
    assert!(!serialized.contains("event_key_hash"));
    assert!(!serialized.contains("request_hash"));

    let parsed_first_key_id = Uuid::parse_str(first_key_id).unwrap();
    let rotated = fixture
        .state
        .db
        .rotate_key(
            parsed_first_key_id,
            "cloud-query-self-service-rotation",
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(rotated.key_id, parsed_first_key_id);
    let rotated_view: Value = fixture
        .client
        .get(format!("{}/self/v1/entitlements", fixture.base_url))
        .bearer_auth(&rotated.key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rotated_view["events"][0]["key_id"], first_key_id);
    assert_eq!(
        rotated_view["entitlements"][0]["account_id"],
        first["credential"]["account_id"]
    );
    assert_eq!(
        fixture
            .client
            .get(format!("{}/self/v1/entitlements", fixture.base_url))
            .bearer_auth(first_key)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let scoped = fixture
        .state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "cloud-query-a-reader".into(),
                scopes: vec!["entitlements:read".into()],
                tenant_external_id: Some(first_tenant.into()),
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let own_events = fixture
        .client
        .get(format!(
            "{}/internal/v1/integrations/memeloop-cloud/events",
            fixture.base_url
        ))
        .bearer_auth(&scoped.token)
        .send()
        .await
        .unwrap();
    assert_eq!(own_events.status(), StatusCode::OK);
    let own_events: Value = own_events.json().await.unwrap();
    assert_eq!(own_events.as_array().unwrap().len(), 1);
    assert_eq!(own_events[0]["tenant_external_id"], first_tenant);

    let wrong_scope = fixture
        .state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "cloud-query-wrong-scope".into(),
                scopes: vec!["requests:read".into()],
                tenant_external_id: Some(first_tenant.into()),
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(
        fixture
            .client
            .get(format!(
                "{}/internal/v1/integrations/memeloop-cloud/events",
                fixture.base_url
            ))
            .bearer_auth(&wrong_scope.token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );

    let cross_tenant = fixture
        .client
        .get(format!(
            "{}/internal/v1/integrations/memeloop-cloud/events?tenant_external_id={second_tenant}",
            fixture.base_url
        ))
        .bearer_auth(&scoped.token)
        .send()
        .await
        .unwrap();
    assert_eq!(cross_tenant.status(), StatusCode::FORBIDDEN);

    let foreign_key_filter = fixture
        .client
        .get(format!(
            "{}/internal/v1/integrations/memeloop-cloud/events?key_id={second_key_id}",
            fixture.base_url
        ))
        .bearer_auth(&scoped.token)
        .send()
        .await
        .unwrap();
    assert_eq!(foreign_key_filter.status(), StatusCode::OK);
    assert!(
        foreign_key_filter
            .json::<Value>()
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn suspended_and_revoked_cloud_identity_rejects_active_refills_but_allows_cancellation() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-disabled-identity";
    let principal = "disabled-member";
    let subscription = "disabled-subscription";
    let route_id = seed_route(&fixture.state, tenant, "gpt-5", 3).await;
    let mut first = active(tenant, principal, subscription, "cycle", "10", 1, 10);
    first["route_ids"] = json!([route_id]);
    let first: Value = fixture
        .send("disabled-1", &first)
        .await
        .json()
        .await
        .unwrap();
    let key_id = Uuid::parse_str(first["credential"]["key_id"].as_str().unwrap()).unwrap();

    fixture
        .state
        .db
        .set_key_status(key_id, "suspended")
        .await
        .unwrap();
    let mut suspended_update = active(tenant, principal, subscription, "cycle", "12", 2, 20);
    suspended_update["route_ids"] = json!([route_id]);
    assert_eq!(
        fixture.send("disabled-2", &suspended_update).await.status(),
        StatusCode::CONFLICT
    );
    let suspended = fixture
        .state
        .db
        .list_managed_keys(Some(tenant), Some(principal))
        .await
        .unwrap();
    assert_eq!(suspended[0].status, "suspended");
    assert_eq!(suspended[0].available_balance, "10");
    assert_eq!(suspended[0].policy.requests_per_minute, 10);
    let entitlement = fixture
        .state
        .db
        .list_entitlements(Some(tenant), Some("memeloop-cloud"), Some(subscription))
        .await
        .unwrap();
    assert_eq!(entitlement[0].version, 1);
    assert_eq!(entitlement[0].remaining, "10");

    let cancelled_response: Value = fixture
        .send(
            "disabled-3",
            &cancelled(tenant, principal, subscription, "cycle", 3),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(cancelled_response["entitlement"]["status"], "cancelled");
    assert_eq!(cancelled_response["entitlement"]["remaining"], "0");
    assert_eq!(
        cancelled_response["policy"]["requests_per_minute"],
        json!(10),
        "cancellation must not replace the persisted policy with payload filler"
    );

    fixture
        .state
        .db
        .set_key_status(key_id, "revoked")
        .await
        .unwrap();
    let mut revoked_update = active(tenant, principal, subscription, "cycle", "5", 4, 30);
    revoked_update["route_ids"] = json!([route_id]);
    assert_eq!(
        fixture.send("disabled-4", &revoked_update).await.status(),
        StatusCode::CONFLICT
    );
    let revoked = fixture
        .state
        .db
        .list_managed_keys(Some(tenant), Some(principal))
        .await
        .unwrap();
    assert_eq!(revoked[0].status, "revoked");
    assert_eq!(revoked[0].available_balance, "0");
    assert_eq!(revoked[0].policy.requests_per_minute, 10);
    assert_eq!(
        fixture
            .send(
                "disabled-5",
                &cancelled(tenant, principal, subscription, "cycle", 5),
            )
            .await
            .status(),
        StatusCode::OK
    );
    assert!(
        fixture
            .state
            .db
            .credential_routing(key_id, tenant)
            .await
            .unwrap()
            .effective_route_ids
            .is_empty()
    );
    let events = fixture
        .state
        .db
        .list_cloud_subscription_events(Some(tenant), Some(principal), Some(key_id), 100)
        .await
        .unwrap();
    let mut versions = events.iter().map(|event| event.version).collect::<Vec<_>>();
    versions.sort_unstable();
    assert_eq!(versions, vec![1, 3, 5]);
    assert!(events.iter().all(|event| event.key_id == key_id));
}
