use super::*;

async fn scoped_writer(
    fixture: &Fixture,
    tenant: &str,
    name: &str,
) -> memeloop_token_center::model::IssuedServiceToken {
    // Tenant-scoped service authentication requires an existing active tenant.
    fixture.state.db.create_tenant(tenant, None).await.unwrap();
    fixture
        .state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: name.into(),
                scopes: vec!["keys:write".into()],
                tenant_external_id: Some(tenant.into()),
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap()
}

fn ensure_url(fixture: &Fixture) -> String {
    format!(
        "{}/internal/v1/integrations/memeloop-cloud/principals/ensure",
        fixture.base_url
    )
}

fn ensure_request(tenant: &str, principal: &str) -> Value {
    json!({
        "tenant_external_id": tenant,
        "principal_external_id": principal,
        "currency": "USD"
    })
}

#[tokio::test]
async fn service_ensure_replays_without_entitlement_or_credit() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-ensure-tenant";
    let principal = "cloud-ensure-principal";
    let service = scoped_writer(&fixture, tenant, "cloud-ensure-writer").await;
    let url = ensure_url(&fixture);
    let request = ensure_request(tenant, principal);
    let first_response = fixture
        .client
        .post(&url)
        .bearer_auth(&service.token)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(first_response.headers()["cache-control"], "no-store");
    let first: Value = first_response
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let second: Value = fixture
        .client
        .post(&url)
        .bearer_auth(&service.token)
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["key_id"], second["key_id"]);
    assert_eq!(first["account_id"], second["account_id"]);
    let key_id = Uuid::parse_str(first["key_id"].as_str().unwrap()).unwrap();
    let account_id = Uuid::parse_str(first["account_id"].as_str().unwrap()).unwrap();
    let managed = fixture
        .state
        .db
        .list_managed_keys(Some(tenant), Some(principal))
        .await
        .unwrap();
    assert_eq!(managed.len(), 1);
    assert_eq!(managed[0].available_balance, "0");
    assert_eq!(
        serde_json::to_value(&managed[0].policy).unwrap(),
        serde_json::to_value(memeloop_token_center::model::KeyPolicy::default()).unwrap()
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
    assert!(
        fixture
            .state
            .db
            .list_account_ledger(account_id, 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture
            .state
            .db
            .list_entitlements(Some(tenant), Some("memeloop-cloud"), None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn generic_key_creation_cannot_claim_the_cloud_provisioning_namespace() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-reserved-provisioning-namespace";
    let principal = "cloud-reserved-provisioning-principal";
    let service = scoped_writer(&fixture, tenant, "cloud-reserved-provisioning-writer").await;
    let provisioning_key = format!(
        "memeloop-cloud-principal:{}",
        framed_digest(&[tenant.as_bytes(), principal.as_bytes()])
    );
    let response = fixture
        .client
        .post(format!("{}/internal/v1/keys", fixture.base_url))
        .bearer_auth(&service.token)
        .header("idempotency-key", provisioning_key)
        .json(&json!({
            "tenant_external_id": tenant,
            "principal_external_id": principal,
            "alias": "MemeLoop Cloud",
            "currency": "USD"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
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
async fn legacy_generic_namespace_squat_cannot_capture_cloud_credit_or_secret() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-legacy-provisioning-squat";
    let principal = "cloud-legacy-provisioning-principal";
    let service = scoped_writer(&fixture, tenant, "cloud-legacy-provisioning-writer").await;
    let squatter: Value = fixture
        .client
        .post(format!("{}/internal/v1/keys", fixture.base_url))
        .bearer_auth(&service.token)
        .header("idempotency-key", "legacy-generic-provisioning")
        .json(&json!({
            "tenant_external_id": tenant,
            "principal_external_id": principal,
            "alias": "MemeLoop Cloud",
            "currency": "USD"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let squatter_key = squatter["key"].as_str().unwrap().to_owned();
    let squatter_key_id = squatter["key_id"].as_str().unwrap();
    let provisioning_key = format!(
        "memeloop-cloud-principal:{}",
        framed_digest(&[tenant.as_bytes(), principal.as_bytes()])
    );
    let inspection = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE key_records SET provisioning_idempotency_key = $1 WHERE id = $2")
        .bind(provisioning_key)
        .bind(squatter_key_id)
        .execute(&inspection)
        .await
        .unwrap();

    let ensure = fixture
        .client
        .post(ensure_url(&fixture))
        .bearer_auth(&service.token)
        .json(&ensure_request(tenant, principal))
        .send()
        .await
        .unwrap();
    assert_eq!(ensure.status(), StatusCode::CONFLICT);
    assert!(!ensure.text().await.unwrap().contains(&squatter_key));

    let subscription = fixture
        .send(
            "cloud-legacy-provisioning-squat-event",
            &active(
                tenant,
                principal,
                "cloud-legacy-provisioning-squat-subscription",
                "cycle",
                "10",
                1,
                10,
            ),
        )
        .await;
    assert_eq!(subscription.status(), StatusCode::CONFLICT);
    assert!(!subscription.text().await.unwrap().contains(&squatter_key));
    let managed = fixture
        .state
        .db
        .list_managed_keys(Some(tenant), Some(principal))
        .await
        .unwrap();
    assert_eq!(managed.len(), 1);
    assert_eq!(managed[0].available_balance, "0");
    assert!(
        fixture
            .state
            .db
            .list_entitlements(Some(tenant), Some("memeloop-cloud"), None)
            .await
            .unwrap()
            .is_empty()
    );
    inspection.close().await;
}

#[tokio::test]
async fn cloud_ciphertext_replay_is_bound_to_its_stable_key_row() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-ciphertext-binding";
    let service = scoped_writer(&fixture, tenant, "cloud-ciphertext-binding-writer").await;
    let first: Value = fixture
        .client
        .post(ensure_url(&fixture))
        .bearer_auth(&service.token)
        .json(&ensure_request(tenant, "ciphertext-owner-one"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let second: Value = fixture
        .client
        .post(ensure_url(&fixture))
        .bearer_auth(&service.token)
        .json(&ensure_request(tenant, "ciphertext-owner-two"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let first_secret = first["key"].as_str().unwrap().to_owned();
    let second_secret = second["key"].as_str().unwrap().to_owned();
    let inspection = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let first_ciphertext: String =
        sqlx::query_scalar("SELECT issued_key_ciphertext FROM key_records WHERE id = $1")
            .bind(first["key_id"].as_str().unwrap())
            .fetch_one(&inspection)
            .await
            .unwrap();
    sqlx::query("UPDATE key_records SET issued_key_ciphertext = $1 WHERE id = $2")
        .bind(first_ciphertext)
        .bind(second["key_id"].as_str().unwrap())
        .execute(&inspection)
        .await
        .unwrap();

    let response = fixture
        .client
        .post(ensure_url(&fixture))
        .bearer_auth(&service.token)
        .json(&ensure_request(tenant, "ciphertext-owner-two"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = response.text().await.unwrap();
    assert!(!body.contains(&first_secret));
    assert!(!body.contains(&second_secret));
    inspection.close().await;
}

#[tokio::test]
async fn ensure_and_subscription_share_identity_without_mutation() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-ensure-subscription";
    let principal = "cloud-ensure-member";
    let service = scoped_writer(&fixture, tenant, "cloud-ensure-subscription-writer").await;
    let url = ensure_url(&fixture);
    let request = ensure_request(tenant, principal);
    let ensured: Value = fixture
        .client
        .post(&url)
        .bearer_auth(&service.token)
        .json(&request)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let key_id = Uuid::parse_str(ensured["key_id"].as_str().unwrap()).unwrap();
    let subscription: Value = fixture
        .send(
            "cloud-ensure-subscription",
            &active(tenant, principal, "ensure-sub", "cycle", "0", 1, 10),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(subscription["credential"]["key_id"], ensured["key_id"]);
    assert_eq!(
        subscription["credential"]["account_id"],
        ensured["account_id"]
    );
    let after_subscription: Value = fixture
        .client
        .post(&url)
        .bearer_auth(&service.token)
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after_subscription["key_id"], ensured["key_id"]);
    let managed = fixture
        .state
        .db
        .list_managed_keys(Some(tenant), Some(principal))
        .await
        .unwrap();
    assert_eq!(managed[0].available_balance, "0");
    assert_eq!(managed[0].policy.requests_per_minute, 10);
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
async fn padded_webhook_before_ensure_cannot_fork_the_normalized_identity() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-padded-webhook-first";
    let principal = "cloud-padded-webhook-first-member";
    assert_eq!(
        fixture
            .send(
                "cloud-padded-webhook-first-event",
                &active(
                    &format!(" {tenant}"),
                    principal,
                    "cloud-padded-webhook-first-subscription",
                    "cycle",
                    "10",
                    1,
                    10,
                ),
            )
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let service = scoped_writer(&fixture, tenant, "cloud-padded-webhook-first-writer").await;
    let ensured: Value = fixture
        .client
        .post(ensure_url(&fixture))
        .bearer_auth(&service.token)
        .json(&ensure_request(tenant, principal))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let managed = fixture
        .state
        .db
        .list_managed_keys(Some(tenant), Some(principal))
        .await
        .unwrap();
    assert_eq!(managed.len(), 1);
    assert_eq!(
        managed[0].key_id,
        Uuid::parse_str(ensured["key_id"].as_str().unwrap()).unwrap()
    );
    assert!(
        fixture
            .state
            .db
            .list_entitlements(Some(tenant), Some("memeloop-cloud"), None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture
            .state
            .db
            .list_cloud_subscription_events(Some(tenant), Some(principal), None, 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn padded_webhook_after_ensure_cannot_fork_the_normalized_identity() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-padded-ensure-first";
    let principal = "cloud-padded-ensure-first-member";
    let service = scoped_writer(&fixture, tenant, "cloud-padded-ensure-first-writer").await;
    let ensured: Value = fixture
        .client
        .post(ensure_url(&fixture))
        .bearer_auth(&service.token)
        .json(&ensure_request(tenant, principal))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        fixture
            .send(
                "cloud-padded-ensure-first-event",
                &active(
                    tenant,
                    &format!("{principal} "),
                    "cloud-padded-ensure-first-subscription",
                    "cycle",
                    "10",
                    1,
                    10,
                ),
            )
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let managed = fixture
        .state
        .db
        .list_managed_keys(Some(tenant), Some(principal))
        .await
        .unwrap();
    assert_eq!(managed.len(), 1);
    assert_eq!(
        managed[0].key_id,
        Uuid::parse_str(ensured["key_id"].as_str().unwrap()).unwrap()
    );
    assert_eq!(managed[0].available_balance, "0");
    assert!(
        fixture
            .state
            .db
            .list_entitlements(Some(tenant), Some("memeloop-cloud"), None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture
            .state
            .db
            .list_cloud_subscription_events(Some(tenant), Some(principal), None, 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn ensure_enforces_scope_tenant_currency_and_identity_shape() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-ensure-auth";
    let principal = "cloud-ensure-auth-member";
    let service = scoped_writer(&fixture, tenant, "cloud-ensure-auth-writer").await;
    let url = ensure_url(&fixture);
    let request = ensure_request(tenant, principal);
    assert_eq!(
        fixture
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let reader = fixture
        .state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "cloud-ensure-reader".into(),
                scopes: vec!["keys:read".into()],
                tenant_external_id: Some(tenant.into()),
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(
        fixture
            .client
            .post(&url)
            .bearer_auth(&reader.token)
            .json(&request)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fixture
            .client
            .post(&url)
            .bearer_auth(&service.token)
            .json(&json!({
                "tenant_external_id": "other-tenant",
                "principal_external_id": principal,
                "currency": "USD"
            }))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fixture
            .client
            .post(&url)
            .bearer_auth(&service.token)
            .json(&json!({
                "tenant_external_id": format!(" {tenant}"),
                "principal_external_id": principal,
                "currency": "USD"
            }))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let first = fixture
        .client
        .post(&url)
        .bearer_auth(&service.token)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let conflict = fixture
        .client
        .post(&url)
        .bearer_auth(&service.token)
        .json(&json!({
            "tenant_external_id": tenant,
            "principal_external_id": principal,
            "currency": "CNY"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    assert_eq!(
        fixture
            .client
            .post(&url)
            .bearer_auth(&service.token)
            .json(&json!({
                "tenant_external_id": tenant,
                "principal_external_id": principal,
                "currency": "EUR"
            }))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
}
