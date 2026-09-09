use super::*;

#[tokio::test]
async fn subscription_then_ensure_preserves_credit_policy_and_routing() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-subscription-first";
    let principal = "cloud-subscription-first-member";
    let route_id = seed_route(&fixture.state, tenant, "cloud-subscription-first-model", 71).await;
    let mut snapshot = active(
        tenant,
        principal,
        "cloud-subscription-first-subscription",
        "cycle",
        "10",
        1,
        73,
    );
    snapshot["route_ids"] = json!([route_id]);
    let subscription: Value = fixture
        .send("cloud-subscription-first-event", &snapshot)
        .await
        .json()
        .await
        .unwrap();
    let service = fixture
        .state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "cloud-subscription-first-writer".into(),
                scopes: vec!["keys:write".into()],
                tenant_external_id: Some(tenant.into()),
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let ensured: Value = fixture
        .client
        .post(format!(
            "{}/internal/v1/integrations/memeloop-cloud/principals/ensure",
            fixture.base_url
        ))
        .bearer_auth(&service.token)
        .json(&json!({
            "tenant_external_id": tenant,
            "principal_external_id": principal,
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
    assert_eq!(ensured["key_id"], subscription["credential"]["key_id"]);
    assert_eq!(
        ensured["account_id"],
        subscription["credential"]["account_id"]
    );
    let key_id = Uuid::parse_str(ensured["key_id"].as_str().unwrap()).unwrap();
    let managed = fixture
        .state
        .db
        .list_managed_keys(Some(tenant), Some(principal))
        .await
        .unwrap();
    assert_eq!(managed[0].available_balance, "10");
    assert_eq!(managed[0].policy.requests_per_minute, 73);
    assert_eq!(
        fixture
            .state
            .db
            .credential_routing(key_id, tenant)
            .await
            .unwrap()
            .effective_route_ids,
        vec![route_id]
    );
}
