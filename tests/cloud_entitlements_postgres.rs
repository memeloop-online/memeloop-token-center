use memeloop_token_center::{AppState, api, config::Config, crypto, db::unix_millis};
use memeloop_token_center::{
    db::{
        CreateGroupInput, CreateModelRouteInput, CreateUpstreamAccountInput, GroupKind,
        ReplaceGroupMembersInput,
    },
    provider::UpstreamCredential,
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

const WEBHOOK_SECRET: &str = "postgres-cloud-webhook-secret-longer-than-32-bytes";

fn framed_digest(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((*part).len().to_be_bytes());
        digest.update(part);
    }
    format!("{:x}", digest.finalize())
}

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

fn cancelled_snapshot(tenant: &str, principal: &str, version: i64) -> Value {
    json!({
        "tenant_external_id": tenant,
        "principal_external_id": principal,
        "external_subscription_id": "postgres-cloud-subscription",
        "external_cycle_id": "postgres-cloud-cycle",
        "period_start": null,
        "period_end": null,
        "currency": "USD",
        "desired": null,
        "version": version,
        "status": "cancelled",
        "policy": {
            "requests_per_minute": 1,
            "tokens_per_minute": 1,
            "max_concurrency": 1,
            "daily_budget": null,
            "weekly_budget": null,
            "lifetime_budget": null
        },
        "proration": null
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

async fn seed_route(state: &AppState, tenant: &str, model: &str, priority: i64) -> Uuid {
    let upstream = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.into(),
                name: format!("postgres-cloud-upstream-{priority}"),
                driver: "http-json".into(),
                config: json!({"base_url": "https://api.example.test"}),
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
            tenant_external_id: tenant.into(),
            public_model: model.into(),
            upstream_account_id: upstream.id,
            upstream_model: model.into(),
            protocol: "openai".into(),
            priority,
        })
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn postgres_cloud_events_serialize_versions_and_replay_stable_identity() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let unique = Uuid::now_v7();
    let tenant = format!("postgres-cloud-{unique}");
    let principal = format!("member-{unique}");
    let mut config = Config::for_test(database_url.clone());
    config.memeloop_cloud_webhook_secret = Some(WEBHOOK_SECRET.into());
    let state = AppState::initialize(config).await.unwrap();
    let test_state = state.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, api::router(state)).await });
    let url = format!("http://{address}/internal/v1/integrations/memeloop-cloud/subscription");
    let client = Client::new();
    let first_route = seed_route(&test_state, &tenant, "gpt-5", 10).await;

    let mut v1 = snapshot(&tenant, &principal, 1, "10", 10);
    v1["route_ids"] = json!([first_route]);
    let first = send(&client, &url, "postgres-cloud-event-1", &v1).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first: Value = first.json().await.unwrap();
    let key_id = first["credential"]["key_id"].clone();
    let account_id = first["credential"]["account_id"].clone();
    let key = first["credential"]["key"].as_str().unwrap().to_owned();
    let parsed_key_id = Uuid::parse_str(key_id.as_str().unwrap()).unwrap();
    assert_eq!(
        test_state
            .db
            .credential_routing(parsed_key_id, &tenant)
            .await
            .unwrap()
            .effective_route_ids,
        vec![first_route]
    );
    let future_route = seed_route(&test_state, &tenant, "gpt-5", 11).await;
    // A later route sharing the same public model requires a newer Cloud
    // snapshot to grant that stable route id explicitly.
    assert_eq!(
        test_state
            .db
            .credential_routing(parsed_key_id, &tenant)
            .await
            .unwrap()
            .effective_route_ids,
        vec![first_route],
        "an explicit route grant must not authorize a future route"
    );
    assert!(
        !test_state
            .db
            .credential_routing(parsed_key_id, &tenant)
            .await
            .unwrap()
            .effective_route_ids
            .contains(&future_route)
    );
    let credential_group = test_state
        .db
        .create_group(
            GroupKind::Credential,
            CreateGroupInput {
                tenant_external_id: tenant.clone(),
                name: format!("postgres-cloud-credentials-{unique}"),
            },
        )
        .await
        .unwrap();
    test_state
        .db
        .replace_group_members(
            GroupKind::Credential,
            credential_group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: tenant.clone(),
                member_ids: vec![parsed_key_id],
                expected_updated_at: credential_group.updated_at,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        test_state
            .db
            .credential_routing(parsed_key_id, &tenant)
            .await
            .unwrap()
            .effective_route_ids,
        vec![first_route],
        "credential-group membership must not change Cloud grants"
    );
    let replay: Value = send(&client, &url, "postgres-cloud-event-1", &v1)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(replay["credential"]["key_id"], key_id);
    assert_eq!(replay["credential"]["account_id"], account_id);
    assert_eq!(replay["credential"]["key"], key);

    let mut normalized_v3 = snapshot(&tenant, &principal, 3, "30", 30);
    normalized_v3["route_ids"] = json!([future_route]);
    normalized_v3["route_group_ids"] = json!([]);
    assert_eq!(
        send(&client, &url, "postgres-cloud-event-3", &normalized_v3,)
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        test_state
            .db
            .credential_routing(parsed_key_id, &tenant)
            .await
            .unwrap()
            .effective_route_ids,
        vec![future_route]
    );
    let historical_replay = send(&client, &url, "postgres-cloud-event-1", &v1).await;
    assert_eq!(historical_replay.status(), StatusCode::CREATED);
    let historical_replay: Value = historical_replay.json().await.unwrap();
    assert_eq!(historical_replay["entitlement"], first["entitlement"]);
    assert_eq!(historical_replay["policy"]["requests_per_minute"], 30);
    assert_eq!(
        test_state
            .db
            .credential_routing(parsed_key_id, &tenant)
            .await
            .unwrap()
            .effective_route_ids,
        vec![future_route],
        "historical replay must not restore the old routing snapshot"
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
    let rotated = test_state
        .db
        .rotate_key(
            parsed_key_id,
            &format!("postgres-cloud-rotation-{unique}"),
            test_state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    assert!(
        test_state
            .db
            .authenticate_key(&key, test_state.config.key_pepper.as_bytes())
            .await
            .is_err()
    );
    assert_eq!(
        test_state
            .db
            .authenticate_key(&rotated.key, test_state.config.key_pepper.as_bytes())
            .await
            .unwrap()
            .key_id,
        parsed_key_id
    );
    let replay_after_rotation = send(&client, &url, "postgres-cloud-event-1", &v1).await;
    assert_eq!(replay_after_rotation.status(), StatusCode::CREATED);
    let replay_after_rotation: Value = replay_after_rotation.json().await.unwrap();
    assert_eq!(replay_after_rotation["entitlement"], first["entitlement"]);
    assert_eq!(replay_after_rotation["credential"]["key_id"], key_id);
    assert_eq!(
        replay_after_rotation["credential"]["credential_generation"],
        2
    );
    assert!(replay_after_rotation["credential"]["key"].is_null());
    assert_eq!(replay_after_rotation["policy"]["requests_per_minute"], 30);
    test_state
        .db
        .set_key_status(parsed_key_id, "suspended")
        .await
        .unwrap();
    assert_eq!(
        send(
            &client,
            &url,
            "postgres-cloud-event-4-suspended",
            &snapshot(&tenant, &principal, 4, "40", 40),
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        test_state
            .db
            .credential_routing(parsed_key_id, &tenant)
            .await
            .unwrap()
            .effective_route_ids,
        vec![future_route],
        "a rejected suspended update must preserve the prior routing snapshot"
    );
    assert_eq!(
        send(
            &client,
            &url,
            "postgres-cloud-event-5-cancelled",
            &cancelled_snapshot(&tenant, &principal, 5),
        )
        .await
        .status(),
        StatusCode::OK
    );
    test_state
        .db
        .set_key_status(parsed_key_id, "revoked")
        .await
        .unwrap();
    assert_eq!(
        send(
            &client,
            &url,
            "postgres-cloud-event-6-revoked-update",
            &snapshot(&tenant, &principal, 6, "6", 60),
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert!(
        test_state
            .db
            .credential_routing(parsed_key_id, &tenant)
            .await
            .unwrap()
            .effective_route_ids
            .is_empty()
    );
    assert_eq!(
        send(
            &client,
            &url,
            "postgres-cloud-event-7-revoked-cancel",
            &cancelled_snapshot(&tenant, &principal, 7),
        )
        .await
        .status(),
        StatusCode::OK
    );

    let concurrent_tenant = format!("postgres-cloud-concurrent-{unique}");
    let principal_a = format!("postgres-principal-a-{unique}");
    let principal_b = format!("postgres-principal-b-{unique}");
    let first = snapshot(&concurrent_tenant, &principal_a, 1, "1", 1);
    let second = snapshot(&concurrent_tenant, &principal_b, 2, "2", 2);
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let first_task = {
        let barrier = barrier.clone();
        let client = client.clone();
        let url = url.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            send(&client, &url, "postgres-concurrent-a", &first)
                .await
                .status()
        })
    };
    let second_task = {
        let barrier = barrier.clone();
        let client = client.clone();
        let url = url.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            send(&client, &url, "postgres-concurrent-b", &second)
                .await
                .status()
        })
    };
    let (first_status, second_status) = (first_task.await.unwrap(), second_task.await.unwrap());
    assert!(first_status.is_success() ^ second_status.is_success());
    assert_eq!(
        test_state
            .db
            .list_managed_keys(Some(&concurrent_tenant), None)
            .await
            .unwrap()
            .len(),
        1
    );

    let atomic_tenant = format!("postgres-cloud-atomic-{unique}");
    let atomic_principal = format!("postgres-cloud-atomic-member-{unique}");
    let original_route = seed_route(&test_state, &atomic_tenant, "gpt-5", 20).await;
    let replacement_route = seed_route(&test_state, &atomic_tenant, "gpt-5", 21).await;
    let mut initial = snapshot(&atomic_tenant, &atomic_principal, 1, "10", 10);
    initial["route_ids"] = json!([original_route]);
    let initial: Value = send(&client, &url, "postgres-cloud-atomic-initial", &initial)
        .await
        .json()
        .await
        .unwrap();
    let atomic_key_id = Uuid::parse_str(initial["credential"]["key_id"].as_str().unwrap()).unwrap();

    let pool = sqlx::PgPool::connect(&database_url).await.unwrap();
    let original_audit = sqlx::query(
        "SELECT event_key_hash, request_hash FROM memeloop_cloud_subscription_events WHERE key_id = $1",
    )
    .bind(atomic_key_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let original_event_hash: String = original_audit.try_get("event_key_hash").unwrap();
    let original_request_hash: String = original_audit.try_get("request_hash").unwrap();
    let retry_event_id = "postgres-cloud-atomic-late-audit-conflict";
    let conflicting_event_hash =
        framed_digest(&[atomic_tenant.as_bytes(), retry_event_id.as_bytes()]);
    let changed = sqlx::query(
        "UPDATE memeloop_cloud_subscription_events SET event_key_hash = $1, request_hash = $2 WHERE key_id = $3",
    )
    .bind(&conflicting_event_hash)
    .bind("f".repeat(64))
    .bind(atomic_key_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(changed.rows_affected(), 1);

    let mut update = snapshot(&atomic_tenant, &atomic_principal, 2, "20", 20);
    update["route_ids"] = json!([replacement_route]);
    assert_eq!(
        send(&client, &url, retry_event_id, &update).await.status(),
        StatusCode::CONFLICT
    );
    let atomic_keys = test_state
        .db
        .list_managed_keys(Some(&atomic_tenant), Some(&atomic_principal))
        .await
        .unwrap();
    assert_eq!(atomic_keys.len(), 1);
    assert_eq!(atomic_keys[0].available_balance, "10");
    assert_eq!(atomic_keys[0].policy.requests_per_minute, 10);
    let atomic_entitlement = test_state
        .db
        .list_entitlements(
            Some(&atomic_tenant),
            Some("memeloop-cloud"),
            Some("postgres-cloud-subscription"),
        )
        .await
        .unwrap();
    assert_eq!(atomic_entitlement[0].version, 1);
    assert_eq!(atomic_entitlement[0].remaining, "10");
    assert_eq!(
        test_state
            .db
            .credential_routing(atomic_key_id, &atomic_tenant)
            .await
            .unwrap()
            .effective_route_ids,
        vec![original_route]
    );

    sqlx::query(
        "UPDATE memeloop_cloud_subscription_events SET event_key_hash = $1, request_hash = $2 WHERE key_id = $3",
    )
    .bind(original_event_hash)
    .bind(original_request_hash)
    .bind(atomic_key_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let retried = send(&client, &url, retry_event_id, &update).await;
    assert_eq!(retried.status(), StatusCode::CREATED);
    let retried: Value = retried.json().await.unwrap();
    assert_eq!(retried["entitlement"]["version"], 2);
    assert_eq!(retried["entitlement"]["remaining"], "20");
    assert_eq!(retried["policy"]["requests_per_minute"], 20);
    assert_eq!(
        test_state
            .db
            .credential_routing(atomic_key_id, &atomic_tenant)
            .await
            .unwrap()
            .effective_route_ids,
        vec![replacement_route]
    );
    pool.close().await;
    server.abort();
}
