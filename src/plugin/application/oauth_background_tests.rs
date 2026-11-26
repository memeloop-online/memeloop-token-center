use super::*;
use crate::{
    AppState,
    config::Config,
    db::{CreateUpstreamAccountInput, test_support::PostgresTestDatabase},
    provider::UpstreamCredential,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
#[ignore = "requires dedicated PostgreSQL with 32 primary connections"]
async fn postgres_cold_oauth_authority_uses_existing_pool_and_preserves_shared_authority() {
    Box::pin(cold_oauth_contract()).await;
}

async fn cold_oauth_contract() {
    let url = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL URL required");
    let isolated = PostgresTestDatabase::create(&url).await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("a");
    super::tests::write_inventory(&root, false);
    let preparation = Database::connect(isolated.url()).await.unwrap();
    preparation.migrate().await.unwrap();
    let inventory = super::tests::inventory(&preparation, &[("a", root)]);
    let inventory_path = directory.path().join("inventory.json");
    std::fs::write(&inventory_path, serde_json::to_vec(&inventory).unwrap()).unwrap();
    preparation.close().await;
    let server = MockServer::start().await;
    let mut config = Config::for_test(isolated.url().into());
    config.database_max_connections = 32;
    config.database_aux_pool_max_connections = 2;
    config.plugin_inventory_file = Some(inventory_path.to_str().unwrap().into());
    config.pricing_models_dev_url = format!("{}/prices", server.uri());
    config.pricing_litellm_url = format!("{}/prices", server.uri());
    config.pricing_openrouter_url = format!("{}/prices", server.uri());
    let state = Box::pin(AppState::initialize_for_maintenance(config))
        .await
        .unwrap();
    let authority = state.application_plugins.as_ref().unwrap();
    authority
        .publish(
            PublishApplicationPlugin {
                inventory_id: "a".into(),
                expected_revision: 0,
            },
            "oauth-cold-publish",
        )
        .await
        .unwrap();
    assert!(
        authority.current_snapshot().is_err(),
        "publisher remains cold"
    );
    Mock::given(method("POST"))
        .and(path("/refresh"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(2))
                .set_body_json(json!({"accessToken":"fresh-access"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "cold-oauth-fixture".into(),
                name: "natural-expiry".into(),
                driver: "http-json".into(),
                config: json!({"base_url":server.uri()}),
                credential: UpstreamCredential::OAuth {
                    access_token: "old-access".into(),
                    refresh_token: Some("fixture-refresh".into()),
                    expires_at: Some(1),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: None,
                    proxy_url: None,
                    proxy_network_scope: None,
                },
                oauth_session_id: Some(uuid::Uuid::now_v7()),
                oauth_driver: Some("cursor".into()),
                oauth_refresh_url: Some(format!("{}/refresh", server.uri())),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let holders = state.db.hold_group_snapshot_pool_for_tests().await;
    assert_eq!(holders.len(), 32);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            state.clone().pin_application_plugins()
        )
        .await
        .is_err(),
        "original cold path really waits on primary"
    );
    let worker = state.clone();
    let batch = tokio::spawn(Box::pin(async move {
        crate::worker::run_oauth_batch_for_test(&worker).await
    }));
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.url.path() == "/refresh")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cold authority and automatic claim reach mock with primary full");
    let auxiliary = tokio::time::timeout(
        Duration::from_millis(750),
        state
            .routing_persistence_db
            .hold_group_snapshot_pool_for_tests(),
    )
    .await
    .expect("no DB transaction held over exchange");
    assert_eq!(auxiliary.len(), 2);
    drop(auxiliary);
    tokio::time::timeout(Duration::from_secs(15), batch)
        .await
        .unwrap()
        .unwrap();
    let (updated, _) = state
        .routing_persistence_db
        .upstream_account_with_credential(account.id, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert_eq!(updated.credential_generation, 2);
    // A separate test observer reads the durable receipt, not a new product pool.
    use sqlx::Connection;
    let mut observer = sqlx::PgConnection::connect(isolated.url()).await.unwrap();
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM credential_rotation_replays WHERE idempotency_key = $1 AND response_ciphertext IS NOT NULL")
        .bind(format!("oauth-worker-{}-generation-1", account.id))
        .fetch_one(&mut observer).await.unwrap();
    assert_eq!(receipts, 1, "one committed generation receipt");
    observer.close().await.unwrap();
    Box::pin(crate::worker::run_oauth_batch_for_test(&state)).await;
    assert!(Arc::ptr_eq(
        authority,
        state.application_plugins.as_ref().unwrap()
    ));
    assert!(
        authority.current_snapshot().is_err(),
        "scoped pin did not replace global publisher state"
    );
    drop(holders);
    authority.refresh_current().await.unwrap();
    let warm = authority.current_snapshot().unwrap().unwrap();
    let scoped = authority
        .pin_for_oauth(&state.routing_persistence_db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        Arc::ptr_eq(&warm, &scoped),
        "warm and scoped callers share revision cache"
    );
    server.verify().await;
    drop(state);
    isolated.cleanup().await;
}
