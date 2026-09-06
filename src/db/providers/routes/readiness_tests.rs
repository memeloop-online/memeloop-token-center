use super::*;

use std::time::Duration;

const PEPPER: &[u8] = b"route-readiness-test-pepper-at-least-32-bytes";
const TENANT: &str = "route-readiness";

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
    sqlx::query("SELECT id FROM upstream_accounts WHERE id = $1 FOR UPDATE")
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
    .bind(unix_millis())
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
