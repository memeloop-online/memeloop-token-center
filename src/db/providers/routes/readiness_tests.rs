use super::*;

use std::time::Duration;

const PEPPER: &[u8] = b"route-readiness-test-pepper-at-least-32-bytes";
const TENANT: &str = "route-readiness";

#[tokio::test]
async fn postgres_retirement_waits_for_concurrent_account_activation_when_configured() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let tenant = format!("retirement-activation-{}", Uuid::now_v7());
    let account = database
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "retirement-concurrent".into(),
                driver: "http-json".into(),
                config: serde_json::json!({"base_url":"http://127.0.0.1:1"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    let route = database
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.clone(),
            public_model: "retirement-concurrent".into(),
            upstream_account_id: account.id,
            upstream_model: "retirement-concurrent".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    let route = database
        .set_model_route_enabled(route.id, &tenant, false, route.updated_at)
        .await
        .unwrap();
    database
        .set_upstream_account_status(account.id, &tenant, "disabled", account.updated_at)
        .await
        .unwrap();
    let before = database.route_routing(route.id, &tenant).await.unwrap();
    let mut activation = database.begin_write_transaction().await.unwrap();
    sqlx::query(
        "UPDATE upstream_accounts SET status = 'active', updated_at = updated_at + 1 WHERE id = $1",
    )
    .bind(account.id.to_string())
    .execute(&mut *activation)
    .await
    .unwrap();
    let concurrent_database = database.clone();
    let concurrent_tenant = tenant.clone();
    let retirement = tokio::spawn(async move {
        concurrent_database
            .retire_model_route_upstreams(
                route.id,
                &concurrent_tenant,
                vec![account.id],
                route.updated_at,
                before.grant_revision,
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !retirement.is_finished(),
        "retirement must wait for the account status writer"
    );
    activation.commit().await.unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), retirement)
            .await
            .unwrap()
            .unwrap(),
        Err(AppError::Conflict(_))
    ));
    let after = database.route_routing(route.id, &tenant).await.unwrap();
    assert_eq!(after.upstream_account_ids, vec![account.id]);
    assert_eq!(after.updated_at, route.updated_at);
    assert_eq!(after.grant_revision, before.grant_revision);
}

#[tokio::test]
async fn retirement_preserves_route_grants_and_allows_zero_candidate_account_deletion() {
    let (_directory, database, account_id) = sqlite_database().await;
    let route = database
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: TENANT.to_owned(),
            public_model: "retired-public".to_owned(),
            upstream_account_id: account_id,
            upstream_model: "retired-upstream".to_owned(),
            protocol: "openai".to_owned(),
            priority: 0,
        })
        .await
        .unwrap();
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: TENANT.to_owned(),
                principal_external_id: "retained-customer".to_owned(),
                alias: "retained-access".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    let original_key_routing = database
        .credential_routing(issued.key_id, TENANT)
        .await
        .unwrap();
    database
        .replace_credential_routing(
            issued.key_id,
            ReplaceCredentialRoutingInput {
                tenant_external_id: TENANT.to_owned(),
                route_ids: vec![route.id],
                route_group_ids: vec![],
                expected_grant_revision: original_key_routing.grant_revision,
            },
        )
        .await
        .unwrap();
    let current = database
        .list_model_routes(Some(TENANT))
        .await
        .unwrap()
        .into_iter()
        .find(|item| item.id == route.id)
        .unwrap();
    let routing = database.route_routing(route.id, TENANT).await.unwrap();
    assert!(matches!(
        database
            .retire_model_route_upstreams(
                route.id,
                TENANT,
                vec![account_id],
                current.updated_at,
                routing.grant_revision,
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    let disabled_route = database
        .set_model_route_enabled(route.id, TENANT, false, current.updated_at)
        .await
        .unwrap();
    assert!(matches!(
        database
            .retire_model_route_upstreams(
                route.id,
                TENANT,
                vec![account_id],
                disabled_route.updated_at,
                routing.grant_revision,
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    let account = database
        .list_upstream_accounts(TENANT)
        .await
        .unwrap()
        .into_iter()
        .find(|item| item.id == account_id)
        .unwrap();
    let disabled_account = database
        .set_upstream_account_status(account_id, TENANT, "disabled", account.updated_at)
        .await
        .unwrap();
    assert!(matches!(
        database
            .retire_model_route_upstreams(
                route.id,
                TENANT,
                vec![account_id],
                disabled_route.updated_at - 1,
                routing.grant_revision,
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    assert!(matches!(
        database
            .retire_model_route_upstreams(
                route.id,
                TENANT,
                vec![account_id, Uuid::new_v4()],
                disabled_route.updated_at,
                routing.grant_revision,
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(
        database
            .route_routing(route.id, TENANT)
            .await
            .unwrap()
            .upstream_account_ids,
        vec![account_id]
    );
    assert!(matches!(
        database
            .retire_model_route_upstreams(
                route.id,
                TENANT,
                vec![account_id],
                disabled_route.updated_at,
                routing.grant_revision + 1,
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    let before = database
        .credential_routing(issued.key_id, TENANT)
        .await
        .unwrap();
    let retired = database
        .retire_model_route_upstreams(
            route.id,
            TENANT,
            vec![account_id],
            disabled_route.updated_at,
            routing.grant_revision,
        )
        .await
        .unwrap();
    assert!(retired.upstream_account_ids.is_empty());
    assert!(retired.candidate_upstream_account_ids.is_empty());
    assert_eq!(retired.granted_credential_ids, vec![issued.key_id]);
    assert_eq!(retired.grant_revision, routing.grant_revision);
    let after = database
        .credential_routing(issued.key_id, TENANT)
        .await
        .unwrap();
    assert_eq!(before.route_ids, after.route_ids);
    assert_eq!(before.grant_revision, after.grant_revision);
    assert!(
        database
            .set_model_route_enabled(route.id, TENANT, true, retired.updated_at)
            .await
            .is_err()
    );
    let readiness = database
        .upstream_deletion_readiness(account_id, TENANT)
        .await
        .unwrap();
    assert!(readiness.can_delete);
    database
        .delete_upstream_account(account_id, TENANT, disabled_account.updated_at)
        .await
        .unwrap();
    let retained = database
        .list_model_routes(Some(TENANT))
        .await
        .unwrap()
        .into_iter()
        .find(|item| item.id == route.id)
        .unwrap();
    assert!(!retained.enabled);
    assert_eq!(retained.upstream_account_id, Uuid::nil());
    assert_eq!(retained.public_model, "retired-public");
    assert_eq!(
        database
            .credential_routing(issued.key_id, TENANT)
            .await
            .unwrap()
            .route_ids,
        vec![route.id]
    );
}

async fn sqlite_database() -> (tempfile::TempDir, Database, Uuid) {
    let directory = tempfile::tempdir().expect("route readiness temporary directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("routes.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let account = database
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: TENANT.to_owned(),
                name: "readiness-upstream".to_owned(),
                driver: "http-json".to_owned(),
                config: serde_json::json!({"base_url": "http://127.0.0.1:1"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    (directory, database, account.id)
}

#[tokio::test]
async fn expired_credentials_cannot_create_update_or_activate_enabled_routes() {
    let (_directory, database, _upstream_account_id) = sqlite_database().await;
    let expired = database
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: TENANT.to_owned(),
                name: "expired-oauth-upstream".to_owned(),
                driver: "http-json".to_owned(),
                config: serde_json::json!({"base_url": "http://127.0.0.1:2"}),
                credential: UpstreamCredential::OAuth {
                    access_token: "expired-access-token".to_owned(),
                    refresh_token: None,
                    expires_at: Some(i64::MAX),
                    header: "authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                    adapter_state: None,
                    proxy_url: None,
                    proxy_network_scope: None,
                },
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            PEPPER,
        )
        .await
        .expect("create expired OAuth account");
    let route = database
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: TENANT.to_owned(),
            public_model: "expired-only-public".to_owned(),
            upstream_account_id: expired.id,
            upstream_model: "expired-only-upstream".to_owned(),
            protocol: "openai".to_owned(),
            priority: 0,
        })
        .await
        .expect("create route fixture");
    let disabled = database
        .set_model_route_enabled(route.id, TENANT, false, route.updated_at)
        .await
        .expect("disable route fixture");
    sqlx::query(
        "UPDATE upstream_credentials SET expires_at = $1
         WHERE upstream_account_id = $2",
    )
    .bind(unix_millis())
    .bind(expired.id.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    assert!(matches!(
        database
            .set_model_route_enabled(route.id, TENANT, true, disabled.updated_at)
            .await,
        Err(AppError::BadRequest(_))
    ));
    let persisted = database
        .list_model_routes(Some(TENANT))
        .await
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.id == route.id)
        .unwrap();
    assert!(!persisted.enabled);
    assert_eq!(persisted.updated_at, disabled.updated_at);

    assert!(matches!(
        database
            .create_model_route(CreateModelRouteInput {
                tenant_external_id: TENANT.to_owned(),
                public_model: "expired-create-public".to_owned(),
                upstream_account_id: expired.id,
                upstream_model: "expired-create-upstream".to_owned(),
                protocol: "openai".to_owned(),
                priority: 1,
            })
            .await,
        Err(AppError::BadRequest(_))
    ));

    let (_directory_two, second_database, healthy_account_id) = sqlite_database().await;
    let healthy_route = second_database
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: TENANT.to_owned(),
            public_model: "healthy-update-public".to_owned(),
            upstream_account_id: healthy_account_id,
            upstream_model: "healthy-update-upstream".to_owned(),
            protocol: "openai".to_owned(),
            priority: 0,
        })
        .await
        .unwrap();
    let expired = second_database
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: TENANT.to_owned(),
                name: "expired-update-upstream".to_owned(),
                driver: "http-json".to_owned(),
                config: serde_json::json!({"base_url": "http://127.0.0.1:3"}),
                credential: UpstreamCredential::OAuth {
                    access_token: "expired-update-token".to_owned(),
                    refresh_token: None,
                    expires_at: Some(i64::MAX),
                    header: "authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                    adapter_state: None,
                    proxy_url: None,
                    proxy_network_scope: None,
                },
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE upstream_credentials SET expires_at = $1 WHERE upstream_account_id = $2")
        .bind(unix_millis())
        .bind(expired.id.to_string())
        .execute(&second_database.pool)
        .await
        .unwrap();
    assert!(matches!(
        second_database
            .update_model_route(
                healthy_route.id,
                TENANT,
                UpdateModelRouteInput {
                    public_model: "expired-update-public".to_owned(),
                    upstream_account_id: expired.id,
                    upstream_model: "expired-update-upstream".to_owned(),
                    protocol: "openai".to_owned(),
                    priority: 0,
                    expected_updated_at: healthy_route.updated_at,
                },
            )
            .await,
        Err(AppError::BadRequest(_))
    ));
    let still_healthy = second_database
        .list_model_routes(Some(TENANT))
        .await
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.id == healthy_route.id)
        .unwrap();
    assert_eq!(still_healthy.upstream_account_id, healthy_account_id);
    assert_eq!(still_healthy.public_model, "healthy-update-public");
}

#[tokio::test]
async fn postgres_activation_is_fenced_against_credential_expiry_when_configured() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let tenant = format!("route-expiry-fence-{}", Uuid::now_v7());
    let account = database
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "expiry-fence-upstream".to_owned(),
                driver: "http-json".to_owned(),
                config: serde_json::json!({"base_url": "http://127.0.0.1:4"}),
                credential: UpstreamCredential::OAuth {
                    access_token: "expiry-fence-token".to_owned(),
                    refresh_token: None,
                    expires_at: Some(i64::MAX),
                    header: "authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                    adapter_state: None,
                    proxy_url: None,
                    proxy_network_scope: None,
                },
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    let route = database
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.clone(),
            public_model: "expiry-fence-public".to_owned(),
            upstream_account_id: account.id,
            upstream_model: "expiry-fence-upstream".to_owned(),
            protocol: "openai".to_owned(),
            priority: 0,
        })
        .await
        .unwrap();
    let disabled = database
        .set_model_route_enabled(route.id, &tenant, false, route.updated_at)
        .await
        .unwrap();
    let route_id = route.id;

    let mut rotation = database.begin_write_transaction().await.unwrap();
    // Hold the exact current credential row that activation locks while it
    // validates eligibility. Locking only the account lets PostgreSQL retain
    // an already-selected credential version in this join, which turns this
    // expiry fence into a scheduler-dependent test rather than a concurrent
    // credential-write test.
    sqlx::query(
        "SELECT credential.id FROM upstream_credentials credential \
         JOIN upstream_accounts account ON account.id = credential.upstream_account_id \
         WHERE account.id = $1 \
           AND credential.generation = account.credential_generation \
           AND credential.revoked_at IS NULL \
         FOR UPDATE OF credential",
    )
    .bind(account.id.to_string())
    .fetch_one(&mut *rotation)
    .await
    .unwrap();
    let activation_database = database.clone();
    let activation_tenant = tenant.clone();
    let activation = tokio::spawn(async move {
        activation_database
            .set_model_route_enabled(route_id, &activation_tenant, true, disabled.updated_at)
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !activation.is_finished(),
        "activation must wait for the credential-generation writer"
    );
    sqlx::query(
        "UPDATE upstream_credentials SET expires_at = $1
         WHERE upstream_account_id = $2 AND revoked_at IS NULL",
    )
    // The activation query binds its eligibility timestamp before waiting on
    // this row lock. Use an unambiguously past value so the post-lock
    // re-evaluation cannot treat the just-written expiry as future relative
    // to that already-bound timestamp.
    .bind(0_i64)
    .bind(account.id.to_string())
    .execute(&mut *rotation)
    .await
    .unwrap();
    rotation.commit().await.unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), activation)
            .await
            .unwrap()
            .unwrap(),
        Err(AppError::BadRequest(_))
    ));
    let persisted = database
        .list_model_routes(Some(&tenant))
        .await
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.id == route_id)
        .unwrap();
    assert!(!persisted.enabled);
}
