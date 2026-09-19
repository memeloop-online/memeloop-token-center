use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{CreateServiceTokenInput, CreateUpstreamAccountInput},
    network::OutboundScope,
    provider::UpstreamCredential,
};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn get(state: &AppState, path: &str, token: &str) -> (StatusCode, Option<String>, Value) {
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::get(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let cache = response
        .headers()
        .get(header::CACHE_CONTROL)
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, cache, serde_json::from_slice(&bytes).unwrap())
}

async fn put(
    state: &AppState,
    path: &str,
    token: &str,
    key: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::put(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", key)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn explicit_proxy_read_preserves_full_url_but_excludes_other_secrets_and_requires_edit_authority()
 {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("proxy-settings.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap();
    let pepper = state.config.key_pepper.as_bytes();
    let mut tokens = Vec::new();
    for (name, scopes, tenant) in [
        (
            "writer",
            vec!["providers:read".into(), "providers:write".into()],
            None,
        ),
        ("reader", vec!["providers:read".into()], None),
        (
            "scoped",
            vec!["providers:write".into()],
            Some("proxy-tenant".into()),
        ),
    ] {
        tokens.push(
            state
                .db
                .create_service_token(
                    CreateServiceTokenInput {
                        name: name.into(),
                        scopes,
                        tenant_external_id: tenant,
                    },
                    pepper,
                )
                .await
                .unwrap()
                .token,
        );
    }
    let proxy_url = "socks5h://proxy-user:proxy-password@100.64.0.16:1080";
    for (index, credential) in [
        UpstreamCredential::ProxiedApiKey {
            value: "API_KEY_CANARY".into(),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            proxy_url: proxy_url.into(),
            proxy_network_scope: OutboundScope::Private,
        },
        UpstreamCredential::OAuth {
            access_token: "ACCESS_CANARY".into(),
            refresh_token: Some("REFRESH_CANARY".into()),
            expires_at: None,
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: Some(json!({"private": "ADAPTER_CANARY"})),
            proxy_url: Some(proxy_url.into()),
            proxy_network_scope: Some(OutboundScope::Private),
        },
        UpstreamCredential::None,
    ]
    .into_iter()
    .enumerate()
    {
        let configured = credential.proxy().is_some();
        let mut expected_credential = serde_json::to_value(&credential).unwrap();
        let account = state
            .db
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: "proxy-tenant".into(),
                    name: format!("Proxy settings fixture {index}"),
                    driver: "http-json".into(),
                    config: json!({"base_url": "https://93.184.216.34", "network_scope": "public"}),
                    credential,
                    oauth_session_id: None,
                    oauth_driver: None,
                    oauth_refresh_url: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let path = format!(
            "/internal/v1/upstreams/{}/transport-proxy?tenant_external_id=proxy-tenant",
            account.id
        );
        let (status, cache, body) = get(&state, &path, &tokens[0]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cache.as_deref(), Some("private, no-store"));
        assert_eq!(body.as_object().unwrap().len(), 6);
        assert_eq!(body["supported"], configured);
        assert_eq!(body["account_id"], account.id.to_string());
        assert_eq!(body["updated_at"], account.updated_at);
        assert_eq!(body["credential_generation"], account.credential_generation);
        assert_eq!(
            body["proxy_url"],
            if configured {
                json!(proxy_url)
            } else {
                Value::Null
            }
        );
        assert_eq!(
            body["proxy_network_scope"],
            if configured {
                json!("private")
            } else {
                Value::Null
            }
        );
        for secret in [
            "API_KEY_CANARY",
            "ACCESS_CANARY",
            "REFRESH_CANARY",
            "ADAPTER_CANARY",
        ] {
            assert!(!body.to_string().contains(secret));
        }
        for token in &tokens[1..] {
            assert_eq!(get(&state, &path, token).await.0, StatusCode::FORBIDDEN);
        }
        assert_eq!(
            get(
                &state,
                &path.replace("proxy-tenant", "other-tenant"),
                &tokens[0]
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            get(&state, &path, "invalid-service-token").await.0,
            StatusCode::UNAUTHORIZED
        );
        let (_, _, list) = get(
            &state,
            "/internal/v1/upstreams?tenant_external_id=proxy-tenant",
            &tokens[0],
        )
        .await;
        for secret in [
            proxy_url,
            "proxy-password",
            "API_KEY_CANARY",
            "ACCESS_CANARY",
            "REFRESH_CANARY",
        ] {
            assert!(!list.to_string().contains(secret));
        }
        let (unchanged, _) = state
            .db
            .upstream_account_with_credential(account.id, pepper)
            .await
            .unwrap();
        assert_eq!(unchanged.updated_at, account.updated_at);
        assert_eq!(
            unchanged.credential_generation,
            account.credential_generation
        );
        // Disable this fixture before editing so no background model probe runs.
        let account = state
            .db
            .set_upstream_account_status(account.id, "proxy-tenant", "disabled", account.updated_at)
            .await
            .unwrap();
        let replacement = "socks5://next-user:next-password@100.64.0.17:1080";
        let update = json!({"tenant_external_id": "proxy-tenant", "proxy_url": replacement,
            "expected_updated_at": account.updated_at, "expected_credential_generation": account.credential_generation});
        let edit_path = path.split('?').next().unwrap();
        let edit_key = format!("edit-{}", account.id);
        assert_eq!(
            put(&state, edit_path, &tokens[2], "scoped-edit", &update)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let (status, edited) = put(&state, edit_path, &tokens[0], &edit_key, &update).await;
        if !configured {
            assert_eq!(status, StatusCode::BAD_REQUEST);
            continue;
        }
        assert_eq!(status, StatusCode::OK, "{edited}");
        assert_eq!(edited["can_update_transport_proxy"], true);
        assert!(!edited.to_string().contains("next-password"));
        let (stored_account, stored) = state
            .db
            .upstream_account_with_credential(account.id, pepper)
            .await
            .unwrap();
        expected_credential["proxy_url"] = json!(replacement);
        expected_credential["proxy_network_scope"] = json!("private");
        assert_eq!(
            serde_json::to_value(stored).unwrap(),
            expected_credential,
            "only proxy fields may change"
        );
        assert_eq!(
            stored_account.credential_generation,
            account.credential_generation + 1
        );
        assert_eq!(
            get(&state, &path, &tokens[0]).await.2["proxy_url"],
            replacement
        );
        let replay = put(&state, edit_path, &tokens[0], &edit_key, &update).await;
        assert_eq!(replay.0, StatusCode::OK);
        assert_eq!(
            replay.1["credential_generation"],
            stored_account.credential_generation
        );
        assert_eq!(
            put(&state, edit_path, &tokens[0], "stale-edit", &update)
                .await
                .0,
            StatusCode::CONFLICT
        );
        // A later configuration change makes fresh transport validation fail
        // deterministically (public scope cannot reach private addresses). A committed
        // replay must still return its original result without that validation.
        state
            .db
            .update_upstream_account(
                account.id,
                "proxy-tenant",
                memeloop_token_center::db::UpdateUpstreamAccountInput {
                    name: stored_account.name.clone(),
                    config: json!({"base_url": "https://10.1.2.3", "network_scope": "public"}),
                    expected_updated_at: stored_account.updated_at,
                    expected_credential_generation: None,
                    credential: None,
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let replay = put(&state, edit_path, &tokens[0], &edit_key, &update).await;
        assert_eq!(replay.0, StatusCode::OK);
        assert_eq!(replay.1, edited);
        assert_eq!(
            put(&state, edit_path, &tokens[0], "fresh-denied", &update)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            put(&state, edit_path, &tokens[2], &edit_key, &update)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
}
