use super::*;
use crate::{
    AppState, config::Config, provider::UpstreamCredential,
    upstream_quota::observations::RoutingQuotaWindow,
};
use serde_json::json;

#[tokio::test]
async fn sqlite_quota_observations_are_scoped_fenced_and_bound_to_opt_in_groups() {
    let directory = tempfile::tempdir().unwrap();
    verify(Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("quota.db").display()
    )))
    .await;
}

#[tokio::test]
async fn postgres_quota_observations_are_scoped_fenced_and_bound_to_opt_in_groups() {
    if let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") {
        verify(Config::for_test(url)).await;
    }
}

async fn verify(config: Config) {
    let state = AppState::initialize(config).await.unwrap();
    let db = &state.db;
    let tenant = format!("quota-test-{}", Uuid::now_v7());
    let key = db
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.clone(),
                principal_external_id: "principal".into(),
                alias: "key".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let auth = db
        .authenticate_key(&key.key, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let account = db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "account".into(),
                driver: "http-json".into(),
                config: json!({"base_url":"http://127.0.0.1:18081","network_scope":"private"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let route = db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.clone(),
            public_model: "quota-model".into(),
            upstream_account_id: account.id,
            upstream_model: "quota-model".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    // Fixture only: no credential is used and no supplier request is made.
    sqlx::query("UPDATE upstream_accounts SET driver='kimi-oauth' WHERE id=$1")
        .bind(account.id.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    let group = Uuid::now_v7();
    sqlx::query("INSERT INTO route_groups (id,tenant_id,name,normalized_name,created_at,updated_at,routing_strategy,routing_priority,strategy_version) VALUES ($1,$2,'quota','quota',1,1,$3,0,1)")
        .bind(group.to_string()).bind(auth.tenant_id.to_string()).bind(json!({"plugin_id":"quota-fixture","config":{}}).to_string()).execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO model_route_group_memberships (tenant_id,route_group_id,model_route_id,created_at) VALUES ($1,$2,$3,1)")
        .bind(auth.tenant_id.to_string()).bind(group.to_string()).bind(route.id.to_string()).execute(&db.pool).await.unwrap();
    assert!(
        db.quota_observation_targets(&["not-enabled".into()], 100, 4)
            .await
            .unwrap()
            .is_empty()
    );
    let target = db
        .quota_observation_targets(&["quota-fixture".into()], 100, 4)
        .await
        .unwrap()
        .into_iter()
        .find(|target| target.account_id == account.id)
        .unwrap();
    let lease = Uuid::now_v7();
    assert!(
        db.claim_quota_observation(&target, lease, 100, 130)
            .await
            .unwrap()
    );
    assert!(
        !db.claim_quota_observation(&target, Uuid::now_v7(), 101, 131)
            .await
            .unwrap()
    );
    let value = RoutingQuotaObservation {
        account_id: account.id,
        generation: target.generation,
        config_revision: target.config_revision,
        provider: "kimi-oauth".into(),
        observed_at: 100,
        valid_until: 200,
        windows: vec![RoutingQuotaWindow {
            id: "summary".into(),
            period_seconds: Some(604800),
            reset_at: Some(190),
            reset_is_estimated: false,
            remaining_fraction: Some(0.5),
            exhausted: Some(false),
        }],
    };
    db.finish_quota_observation(&target, Uuid::now_v7(), Some(&value), 110)
        .await
        .unwrap();
    let candidate = AuthorizedUpstreamCandidate {
        route_id: route.id,
        account_id: account.id,
        driver: "kimi-oauth".into(),
        transport_revision: target.config_revision,
        credential_generation: target.generation,
    };
    let candidates = [candidate];
    assert!(
        db.routing_quota_observations(auth.tenant_id, &candidates, 105)
            .await
            .unwrap()
            .is_empty()
    );
    db.finish_quota_observation(&target, lease, Some(&value), 110)
        .await
        .unwrap();
    assert_eq!(
        db.routing_quota_observations(auth.tenant_id, &candidates, 105)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        db.routing_quota_observations(Uuid::now_v7(), &candidates, 105)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        db.routing_quota_observations(auth.tenant_id, &candidates, 190)
            .await
            .unwrap()
            .is_empty()
    );
    let failure_lease = Uuid::now_v7();
    assert!(
        db.claim_quota_observation(&target, failure_lease, 111, 141)
            .await
            .unwrap()
    );
    db.finish_quota_observation(&target, failure_lease, None, 150)
        .await
        .unwrap();
    assert!(
        db.routing_quota_observations(auth.tenant_id, &candidates, 112)
            .await
            .unwrap()
            .is_empty()
    );
    let stale_lease = Uuid::now_v7();
    assert!(
        db.claim_quota_observation(&target, stale_lease, 151, 181)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE upstream_accounts SET updated_at=updated_at+1,credential_generation=credential_generation+1 WHERE id=$1").bind(account.id.to_string()).execute(&db.pool).await.unwrap();
    db.finish_quota_observation(&target, stale_lease, Some(&value), 160)
        .await
        .unwrap();
    assert!(
        db.routing_quota_observations(auth.tenant_id, &candidates, 155)
            .await
            .unwrap()
            .is_empty()
    );
}
