use super::*;

const PRIMARY: &str = "socks5h://10.20.30.40:1080";
const BACKUP: &str = "socks5h://10.20.30.41:1080";

async fn fixture(label: &str) -> CodexRouteFixture {
    let mut fixture = codex_route_fixture(label).await;
    let (account, _) = fixture
        .state
        .db
        .upstream_account_with_credential(
            fixture.upstream_account_id,
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    fixture
        .state
        .db
        .rotate_codex_transport_proxy(
            account.id,
            account.tenant_external_id.as_deref().unwrap(),
            PRIMARY.into(),
            account.updated_at,
            account.credential_generation,
            "proxy-group-fixture",
            None,
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    fixture.state.transport_proxy_groups = std::sync::Arc::new(
        crate::db::TransportProxyGroups::parse(
            &json!([{
                "account_id": account.id, "version": 1, "proxies": [PRIMARY, BACKUP]
            }])
            .to_string(),
            fixture.state.config.key_pepper.as_bytes(),
        )
        .unwrap(),
    );
    fixture
}

async fn selection(fixture: &CodexRouteFixture) -> (String, i64) {
    let (account, credential) = fixture
        .state
        .db
        .upstream_account_with_credential(
            fixture.upstream_account_id,
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let selected = fixture
        .state
        .transport_proxy_groups
        .select(account.id, account.credential_generation, &credential)
        .unwrap();
    (
        selected.credential.proxy().unwrap().0.to_owned(),
        selected.generation,
    )
}

#[tokio::test]
async fn managed_binding_stamp_reaches_send_stage_without_bad_request() {
    use crate::db::transport_proxy_management::{BindGroup, CONFIG_KEY, CreateGroup, MemberInput};

    let fixture = codex_route_fixture("managed-binding-stamp").await;
    let key = fixture.state.config.key_pepper.as_bytes();
    let (account, _) = fixture
        .state
        .db
        .upstream_account_with_credential(fixture.upstream_account_id, key)
        .await
        .unwrap();
    let tenant = account.tenant_external_id.as_ref().unwrap();
    let group = fixture
        .state
        .db
        .create_transport_group(
            CreateGroup {
                tenant_external_id: tenant.clone(),
                name: "managed-egress".into(),
                members: vec![MemberInput {
                    id: None,
                    label: "primary".into(),
                    proxy_url: Some(PRIMARY.into()),
                }],
            },
            None,
            key,
        )
        .await
        .unwrap();
    let group_id = Uuid::parse_str(group["id"].as_str().unwrap()).unwrap();
    let member_id = Uuid::parse_str(group["members"][0]["id"].as_str().unwrap()).unwrap();
    let binding = fixture
        .state
        .db
        .bind_transport_group(
            account.id,
            BindGroup {
                tenant_external_id: tenant.clone(),
                group_id,
                expected_group_version: group["version"].as_i64().unwrap(),
                initial_member_id: member_id,
                expected_binding_version: 0,
                expected_credential_generation: account.credential_generation,
                expected_updated_at: account.updated_at,
            },
            None,
            key,
        )
        .await
        .unwrap();
    let (bound_account, bound_credential) = fixture
        .state
        .db
        .upstream_account_with_credential(account.id, key)
        .await
        .unwrap();
    assert_eq!(
        bound_account.credential_generation,
        account.credential_generation + 1
    );
    assert_eq!(bound_credential.proxy().unwrap().0, PRIMARY);
    let stamp = &bound_account.config[CONFIG_KEY];
    assert_eq!(stamp["group_id"], group["id"]);
    assert_eq!(stamp["group_version"], group["version"]);
    assert_eq!(stamp["binding_version"], binding["binding_version"]);
    assert!(stamp["selection_version"].as_i64().unwrap() > 0);

    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    fixture
        .state
        .transport_proxy_groups
        .synchronize_managed_for_test(&pool)
        .await
        .unwrap();
    pool.close().await;

    let (response, remaining) = routing::with_test_send_failures(
        vec![ProxySendError::NonRetryableTransport(
            routing::TransportFailureKind::ConnectionReset,
        )],
        send_codex_route_to_endpoint(
            &fixture,
            codex_transport::BASE_URL.to_owned(),
            "/v1/responses",
            json!({"model": fixture.model, "input": "hello", "stream": false}),
        ),
    )
    .await;
    assert_eq!(
        remaining, 0,
        "the route must consume the injected send failure"
    );
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("upstream request failed"));
    for private_value in [
        "upstream-access-secret",
        "upstream-refresh-secret",
        PRIMARY,
        CONFIG_KEY,
    ] {
        assert!(!text.contains(private_value));
    }
}

#[tokio::test]
async fn connection_failures_visit_each_group_member_once() {
    let fixture = fixture("sticky-group-connect").await;
    let first = selection(&fixture).await;
    let (response, remaining) = routing::with_test_send_failures(
        vec![
            ProxySendError::RetryableConnection("connect"),
            ProxySendError::RetryableConnection("connect"),
        ],
        send_codex_route_to_endpoint(
            &fixture,
            codex_transport::BASE_URL.to_owned(),
            "/v1/responses",
            json!({"model": fixture.model, "input":"hello", "stream":false}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
    let selected = selection(&fixture).await;
    assert_eq!(selected.0, BACKUP);
    assert_eq!(selected.1, first.1 + 1);
}

#[tokio::test]
async fn ambiguous_delivery_never_replays_or_changes_group_selection() {
    for (index, error) in [
        ProxySendError::NonRetryableTransport(routing::TransportFailureKind::Timeout),
        ProxySendError::NonRetryableTransport(routing::TransportFailureKind::ConnectionReset),
        ProxySendError::NonRetryableTransport(routing::TransportFailureKind::Http2Reset),
        ProxySendError::NonRetryableTransport(routing::TransportFailureKind::Http2GoAway),
        ProxySendError::NonRetryableTransport(routing::TransportFailureKind::Body),
        ProxySendError::AmbiguousResponse("already_output"),
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = fixture(&format!("sticky-group-ambiguous-{index}")).await;
        let before = selection(&fixture).await;
        let (response, remaining) = routing::with_test_send_failures(
            vec![
                error,
                ProxySendError::RetryableConnection("must_not_replay"),
            ],
            send_codex_route_to_endpoint(
                &fixture,
                codex_transport::BASE_URL.to_owned(),
                "/v1/responses",
                json!({"model": fixture.model, "input":"hello", "stream":false}),
            ),
        )
        .await;
        assert!(!response.status().is_success());
        let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        assert_eq!(remaining, 1);
        assert_eq!(selection(&fixture).await, before);
    }
}
