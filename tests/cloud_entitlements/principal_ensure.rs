use super::*;

async fn scoped_writer(
    fixture: &Fixture,
    tenant: &str,
    name: &str,
) -> memeloop_token_center::model::IssuedServiceToken {
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
    let first: Value = fixture
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
