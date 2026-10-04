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
