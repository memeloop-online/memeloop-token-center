use super::*;
use crate::{
    config::Config,
    db::{CreateUpstreamAccountInput, test_support::PostgresTestDatabase},
    provider::UpstreamCredential,
};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
#[ignore = "requires a dedicated CI PostgreSQL service and 32 primary pool connections"]
async fn postgres_oauth_auto_recovery_uses_existing_pool_while_primary_is_exhausted() {
    let url =
        std::env::var("MTC_TEST_POSTGRES_URL").expect("real PostgreSQL contract requires URL");
    let isolated = PostgresTestDatabase::create(&url).await;
    let server = MockServer::start().await;
    let mut config = Config::for_test(isolated.url().into());
    config.database_max_connections = 32;
    config.database_aux_pool_max_connections = 2;
    config.pricing_models_dev_url = format!("{}/prices", server.uri());
    config.pricing_litellm_url = format!("{}/prices", server.uri());
    config.pricing_openrouter_url = format!("{}/prices", server.uri());
    // Use the real worker batch entry below, without unrelated scheduled work.
    let state = AppState::initialize_for_maintenance(config).await.unwrap();
    Mock::given(method("POST"))
        .and(path("/refresh"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(2))
                .set_body_json(json!({"accessToken":"fresh-access"})),
        )
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1internal:retrieveUserQuotaSummary"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    let mut ids = Vec::new();
    for (name, driver, expires_at) in [
        ("natural-expiry", "http-json", 1),
        (
            "rejected-generation",
            crate::provider::antigravity::DRIVER,
            crate::db::unix_millis() + 86_400_000,
        ),
    ] {
        let account = state.db.create_upstream_account(CreateUpstreamAccountInput {
            tenant_external_id:"oauth-pool-contract".into(), name:name.into(), driver:driver.into(),
            config:json!({"base_url":server.uri(),"control_url":server.uri(),"project_id":"fixture"}),
            credential:UpstreamCredential::OAuth {
                access_token:"old-access".into(), refresh_token:Some("fixture-refresh".into()),
                expires_at:Some(expires_at), header:"authorization".into(), prefix:"Bearer ".into(),
                adapter_state:None, proxy_url:None, proxy_network_scope:None,
            }, oauth_session_id:Some(Uuid::now_v7()), oauth_driver:Some("cursor".into()),
            oauth_refresh_url:Some(format!("{}/refresh",server.uri())),
        }, state.config.key_pepper.as_bytes()).await.unwrap();
        ids.push(account.id);
    }
    let primary_holders = state.db.hold_group_snapshot_pool_for_tests().await;
    assert_eq!(
        primary_holders.len(),
        32,
        "all configured primary slots held"
    );
    // Establish that the test actually blocks primary acquisition, not merely a different pool.
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            state
                .db
                .list_managed_oauth_refresh_candidates(crate::db::unix_millis(), 20)
        )
        .await
        .is_err()
    );
    state.upstream_quota.oauth_rejections.submit(ids[1], 1);
    tokio::time::timeout(
        Duration::from_secs(5),
        state.upstream_quota.oauth_rejections.flush(&state),
    )
    .await
    .expect("evidence delivery must progress independently");
    assert!(
        state
            .routing_persistence_db
            .has_rejected_oauth_generation(ids[1], 1)
            .await
            .unwrap()
    );
    let (_stop, shutdown) = watch::channel(false);
    let worker_state = state.clone();
    let worker_shutdown = shutdown.clone();
    let batch = tokio::spawn(async move {
        refresh_oauth_batch(&worker_state, &worker_shutdown, &BlockingTasks::new()).await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
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
    .expect("automatic due scan and claim must reach the mock with primary still full");
    // The remote response is deliberately pending. No DB connection or transaction
    // may be held across the exchange; both existing auxiliary slots remain acquirable.
    let auxiliary_holders = tokio::time::timeout(
        Duration::from_millis(750),
        state
            .routing_persistence_db
            .hold_group_snapshot_pool_for_tests(),
    )
    .await
    .expect("OAuth external I/O must not retain a database transaction");
    assert_eq!(auxiliary_holders.len(), 2);
    drop(auxiliary_holders);
    tokio::time::timeout(Duration::from_secs(20), batch)
        .await
        .expect("renewal must finish without releasing the primary holders")
        .unwrap()
        .unwrap();
    for id in &ids {
        let (account, credential) = state
            .routing_persistence_db
            .upstream_account_with_credential(*id, state.config.key_pepper.as_bytes())
            .await
            .unwrap();
        assert_eq!(account.credential_generation, 2);
        assert!(
            matches!(credential, UpstreamCredential::OAuth { access_token, .. } if access_token=="fresh-access")
        );
    }
    tokio::time::timeout(
        Duration::from_secs(10),
        crate::upstream_quota::oauth_recovery::verify_batch(&state),
    )
    .await
    .expect("post-renewal read and receipt must use the independent handle")
    .unwrap();
    state.upstream_quota.oauth_rejections.flush(&state).await;
    assert!(
        state
            .routing_persistence_db
            .list_rejected_oauth_verifications(crate::db::unix_millis(), 20)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !state
            .routing_persistence_db
            .has_rejected_oauth_generation(ids[1], 2)
            .await
            .unwrap(),
        "the post-renewal 401 must not enqueue a second exchange"
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        refresh_oauth_batch(&state, &shutdown, &BlockingTasks::new()),
    )
    .await
    .unwrap()
    .unwrap();
    server.verify().await;
    drop(primary_holders);
    // Shared control state was never rebound: its original pool becomes usable again.
    assert_eq!(
        state
            .db
            .upstream_account_with_credential(ids[0], state.config.key_pepper.as_bytes())
            .await
            .unwrap()
            .0
            .credential_generation,
        2
    );
    state.db.close().await;
    state.routing_persistence_db.close().await;
    isolated.cleanup().await;
}
