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
    use sqlx::Connection;
    let mut schema_observer = sqlx::PgConnection::connect(isolated.url()).await.unwrap();
    let maximum: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_migrations")
        .fetch_one(&mut schema_observer)
        .await
        .unwrap();
    let version_166: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations WHERE version = 166")
            .fetch_one(&mut schema_observer)
            .await
            .unwrap();
    assert_eq!(
        maximum, 166,
        "fixture must exercise schema166, not a newer migration registry"
    );
    assert_eq!(version_166, 1, "exactly one schema166 receipt");
    eprintln!("OAuth cold-flight fixture: schema_max={maximum}, schema166_receipts={version_166}");
    schema_observer.close().await.unwrap();
    let mut inventory = super::tests::inventory(&preparation, &[("a", root)]);
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
    let account_id = create_expired_account(&state, &server, "natural-expiry").await;
    // An ordinary caller owns the only flight before primary saturation.
    // Hold its compiler before validation, then make OAuth join it.
    let compilations_before = authority
        .compilations
        .load(std::sync::atomic::Ordering::Relaxed);
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    *authority.compile_gate.lock().unwrap() = Some((entered, released));
    let ordinary_authority = authority.clone();
    let ordinary = tokio::spawn(Box::pin(async move { ordinary_authority.pin().await }));
    tokio::time::timeout(Duration::from_secs(10), entering)
        .await
        .unwrap()
        .unwrap();
    let first_flight = authority.snapshots.lock().await.in_flight.clone().unwrap();
    let (joined, joining) = tokio::sync::oneshot::channel();
    *authority.snapshot_flight_joined.lock().unwrap() = Some(joined);
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
    tokio::time::timeout(Duration::from_secs(5), joining)
        .await
        .expect("OAuth must join ordinary caller's existing flight")
        .unwrap();
    assert!(Arc::ptr_eq(
        &first_flight,
        authority.snapshots.lock().await.in_flight.as_ref().unwrap()
    ));
    eprintln!("OAuth cold-flight fixture: primary_slots=32, ordinary_flight=1, oauth_joined=true");
    release.send(()).unwrap();
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
    let ordinary_snapshot = tokio::time::timeout(Duration::from_secs(5), ordinary)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        authority
            .compilations
            .load(std::sync::atomic::Ordering::Relaxed),
        compilations_before + 1,
        "exactly one shared cold compilation"
    );
    let scoped_snapshot = authority
        .pin_for_oauth(&state.routing_persistence_db)
        .await
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&ordinary_snapshot, &scoped_snapshot));
    let mut mismatched_receipt = scoped_snapshot.receipt.clone();
    mismatched_receipt.identity_digest.push_str("-mismatch");
    assert!(
        matches!(
            authority.pin_revision(mismatched_receipt).await,
            Err(AppError::Forbidden)
        ),
        "same cached revision cannot bypass its authoritative identity"
    );
    let (updated, _) = state
        .routing_persistence_db
        .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert_eq!(updated.credential_generation, 2);
    // A separate test observer reads the durable receipt, not a new product pool.
    let mut observer = sqlx::PgConnection::connect(isolated.url()).await.unwrap();
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM credential_rotation_replays WHERE idempotency_key = $1 AND response_ciphertext IS NOT NULL")
        .bind(format!("oauth-worker-{}-generation-1", account_id))
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
    // A new authoritative revision with revoked deployment grants must fail
    // closed, even though the earlier approved snapshot remains warm.
    authority
        .publish(
            PublishApplicationPlugin {
                inventory_id: "a".into(),
                expected_revision: 1,
            },
            "oauth-cold-second-publish",
        )
        .await
        .unwrap();
    let denied_id = create_expired_account(&state, &server, "revoked-grants").await;
    inventory.get_mut("a").unwrap().grants.clear();
    std::fs::write(&inventory_path, serde_json::to_vec(&inventory).unwrap()).unwrap();
    let denied_holders = state.db.hold_group_snapshot_pool_for_tests().await;
    assert_eq!(denied_holders.len(), 32);
    assert!(matches!(
        authority.pin_for_oauth(&state.routing_persistence_db).await,
        Err(AppError::Forbidden)
    ));
    tokio::time::timeout(
        Duration::from_secs(5),
        Box::pin(crate::worker::run_oauth_batch_for_test(&state)),
    )
    .await
    .unwrap();
    let (denied, _) = state
        .routing_persistence_db
        .upstream_account_with_credential(denied_id, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert_eq!(
        denied.credential_generation, 1,
        "invalid grants cannot dispatch renewal"
    );
    let mut observer = sqlx::PgConnection::connect(isolated.url()).await.unwrap();
    let denied_receipts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM credential_rotation_replays WHERE resource_id = $1",
    )
    .bind(denied_id.to_string())
    .fetch_one(&mut observer)
    .await
    .unwrap();
    assert_eq!(
        denied_receipts, 0,
        "authority failure occurs before durable renewal claim"
    );
    observer.close().await.unwrap();
    drop(denied_holders);
    server.verify().await;
    eprintln!(
        "OAuth cold-flight fixture: exchanges=1, generation=2, committed_receipts=1, revoked_grants_rejected=true"
    );
    drop(state);
    isolated.cleanup().await;
}

async fn create_expired_account(state: &AppState, server: &MockServer, name: &str) -> uuid::Uuid {
    state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "cold-oauth-fixture".into(),
                name: name.into(),
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
        .unwrap()
        .id
}
