use super::*;

#[tokio::test]
async fn concurrent_service_ensures_share_one_cloud_identity() {
    let fixture = Fixture::new().await;
    let tenant = "cloud-concurrent-ensure";
    let service = fixture
        .state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "cloud-concurrent-ensure-writer".into(),
                scopes: vec!["keys:write".into()],
                tenant_external_id: Some(tenant.into()),
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let url = format!(
        "{}/internal/v1/integrations/memeloop-cloud/principals/ensure",
        fixture.base_url
    );
    let body = json!({
        "tenant_external_id": tenant,
        "principal_external_id": "concurrent-principal",
        "currency": "USD"
    });
    let first = fixture
        .client
        .post(&url)
        .bearer_auth(&service.token)
        .json(&body)
        .send();
    let second = fixture
        .client
        .post(&url)
        .bearer_auth(&service.token)
        .json(&body)
        .send();
    let (first, second) = tokio::join!(first, second);
    let first: Value = first
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let second: Value = second
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["key_id"], second["key_id"]);
    assert_eq!(first["account_id"], second["account_id"]);
    assert_eq!(
        fixture
            .state
            .db
            .list_managed_keys(Some(tenant), Some("concurrent-principal"))
            .await
            .unwrap()
            .len(),
        1
    );
}
