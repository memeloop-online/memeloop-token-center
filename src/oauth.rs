use futures_util::StreamExt;

use crate::error::AppError;

mod adapter;
pub mod claude;
pub mod codex_device;
pub mod copilot;
mod cursor;
mod endpoint;
pub(crate) mod managed;

pub(crate) use adapter::{refresh_managed_oauth_credential, resolve_managed_oauth_refresh_adapter};
pub use cursor::{
    CursorOAuthEndpoints, CursorPollAuthority, CursorPollResult, DEFAULT_CURSOR_LOGIN_URL,
    DEFAULT_CURSOR_POLL_URL, DEFAULT_CURSOR_REFRESH_URL, OAuthLoginStart,
    OAuthReauthorizationTarget, ReadyCursorLogin, StartCursorLogin, cursor_account_id,
    poll_cursor_login, refresh_cursor_credential, start_cursor_login,
};
#[cfg(test)]
pub(crate) use endpoint::validate_oauth_endpoint;
pub(crate) use endpoint::{oauth_adapter_endpoint_scope, validate_oauth_adapter_endpoint};

const MAX_OAUTH_RESPONSE_BYTES: usize = 1024 * 1024;

async fn bounded_body(response: reqwest::Response) -> Result<Vec<u8>, AppError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_OAUTH_RESPONSE_BYTES as u64)
    {
        return Err(AppError::Upstream("OAuth response is too large".into()));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(AppError::from)?;
        if body.len().saturating_add(chunk.len()) > MAX_OAUTH_RESPONSE_BYTES {
            return Err(AppError::Upstream("OAuth response is too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
use crate::{
    db::Database,
    network::OutboundScope,
    provider::{ManagedOAuthAdapterBackend, ProviderCatalog, UpstreamCredential},
};
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use uuid::Uuid;
#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    async fn sqlite_database() -> (tempfile::TempDir, Database) {
        let directory = tempfile::tempdir().expect("OAuth test temporary directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("oauth.db").display()
        );
        let database = Database::connect(&database_url)
            .await
            .expect("connect OAuth test database");
        database
            .migrate()
            .await
            .expect("migrate OAuth test database");
        (directory, database)
    }

    #[tokio::test]
    async fn cursor_login_state_is_encrypted_and_contains_pkce_query() {
        let (_directory, database) = sqlite_database().await;
        let stable_account_id = Uuid::now_v7();
        let started = start_cursor_login(
            &database,
            StartCursorLogin {
                tenant_external_id: "default".to_owned(),
                account_name: "cursor-one".to_owned(),
                provider_driver: "http-json".to_owned(),
                provider_config: json!({"base_url": "https://provider.example"}),
                endpoints: CursorOAuthEndpoints::default(),
                oauth_driver: "cursor".to_owned(),
                reauthorize: Some(OAuthReauthorizationTarget {
                    account_id: stable_account_id,
                    expected_updated_at: 999,
                    expected_credential_generation: 7,
                }),
            },
            None,
            b"test material with at least 32 bytes",
            1_000,
        )
        .await
        .unwrap();
        assert!(started.login_url.contains("challenge="));
        assert!(started.login_url.contains("uuid="));
        assert!(!started.session_token.contains("cursor-one"));
        assert!(
            !started
                .session_token
                .contains(stable_account_id.to_string().as_str())
        );
        assert_eq!(started.expires_at, 601_000);
    }

    #[tokio::test]
    async fn provider_adapter_allows_private_cluster_http_but_rejects_public_http() {
        let (_directory, database) = sqlite_database().await;
        let private = start_cursor_login(
            &database,
            StartCursorLogin {
                tenant_external_id: "default".to_owned(),
                account_name: "plugin-oauth".to_owned(),
                provider_driver: "plugin-provider".to_owned(),
                provider_config: json!({
                    "base_url": "http://plugin-upstream.default.svc",
                    "network_scope": "private"
                }),
                endpoints: CursorOAuthEndpoints {
                    login_url: "http://oauth-adapter.default.svc/login".to_owned(),
                    poll_url: "http://oauth-adapter.default.svc/poll".to_owned(),
                    refresh_url: "http://oauth-adapter.default.svc/refresh".to_owned(),
                },
                oauth_driver: "provider_adapter".to_owned(),
                reauthorize: None,
            },
            None,
            b"test material with at least 32 bytes",
            1_000,
        )
        .await
        .unwrap();
        assert_eq!(private.driver, "provider_adapter");
        assert!(validate_oauth_endpoint("http://oauth.example.com/login", "login_url").is_err());
        assert!(
            oauth_adapter_endpoint_scope(
                "http://10.1.2.3:8080/poll",
                "poll_url",
                false,
                OutboundScope::Private,
            )
            .is_err()
        );
        assert!(
            oauth_adapter_endpoint_scope(
                "https://169.254.169.254/latest/meta-data",
                "poll_url",
                false,
                OutboundScope::Private,
            )
            .is_err()
        );
        assert_eq!(
            oauth_adapter_endpoint_scope(
                "http://oauth-adapter.default.svc/poll",
                "poll_url",
                false,
                OutboundScope::Private,
            )
            .unwrap()
            .1,
            OutboundScope::Private
        );
        assert_eq!(
            oauth_adapter_endpoint_scope(
                "https://oauth.example.com/poll",
                "poll_url",
                false,
                OutboundScope::Private,
            )
            .unwrap()
            .1,
            OutboundScope::Public,
            "a private upstream account must not grant private DNS authority to a public adapter"
        );
        assert!(
            oauth_adapter_endpoint_scope(
                "http://oauth.example.com/poll",
                "poll_url",
                false,
                OutboundScope::Private,
            )
            .is_err()
        );
        for endpoint in [
            "https://[::1]/oauth",
            "https://[fe80::1]/oauth",
            "https://[fc00::1]/oauth",
            "https://[::ffff:169.254.169.254]/oauth",
            "https://[64:ff9b::a9fe:a9fe]/oauth",
            "https://[2002:7f00:1::]/oauth",
        ] {
            assert!(
                oauth_adapter_endpoint_scope(
                    endpoint,
                    "adapter_url",
                    false,
                    OutboundScope::Public,
                )
                .is_err(),
                "interactive adapter accepted {endpoint}"
            );
        }
        assert_eq!(
            oauth_adapter_endpoint_scope(
                "https://[2606:4700:4700::1111]/oauth",
                "adapter_url",
                false,
                OutboundScope::Public,
            )
            .unwrap()
            .1,
            OutboundScope::Public
        );
    }

    #[tokio::test]
    async fn cursor_poll_checks_tenant_before_network_io() {
        let (_directory, database) = sqlite_database().await;
        let key_material = b"test material with at least 32 bytes";
        let started = start_cursor_login(
            &database,
            StartCursorLogin {
                tenant_external_id: "tenant-a".to_owned(),
                account_name: "cursor-one".to_owned(),
                provider_driver: "http-json".to_owned(),
                provider_config: json!({"base_url": "https://provider.example"}),
                endpoints: CursorOAuthEndpoints::default(),
                oauth_driver: "cursor".to_owned(),
                reauthorize: None,
            },
            None,
            key_material,
            1_000,
        )
        .await
        .unwrap();
        let providers = ProviderCatalog::builtins();

        let error = poll_cursor_login(
            &database,
            &providers,
            &reqwest::Client::new(),
            &started.session_token,
            key_material,
            2_000,
            CursorPollAuthority {
                required_tenant: Some("tenant-b"),
                operator_service_id: None,
                allow_test_loopback: true,
            },
        )
        .await
        .expect_err("cross-tenant polling must be rejected before I/O");
        assert!(matches!(error, AppError::Forbidden));
    }

    #[tokio::test]
    async fn cursor_poll_rejects_a_provider_removed_after_session_creation_before_network_io() {
        let server = MockServer::start().await;
        let (_directory, database) = sqlite_database().await;
        let now = crate::db::unix_millis();
        let key_material = b"test material with at least 32 bytes";
        let started = start_cursor_login(
            &database,
            StartCursorLogin {
                tenant_external_id: "retired-provider-poll".to_owned(),
                account_name: "retired-provider".to_owned(),
                provider_driver: "retired-plugin-provider".to_owned(),
                provider_config: json!({"base_url": "https://provider.example"}),
                endpoints: CursorOAuthEndpoints {
                    login_url: format!("{}/login", server.uri()),
                    poll_url: format!("{}/poll", server.uri()),
                    refresh_url: format!("{}/refresh", server.uri()),
                },
                oauth_driver: "provider_adapter".to_owned(),
                reauthorize: None,
            },
            None,
            key_material,
            now,
        )
        .await
        .unwrap();

        let error = poll_cursor_login(
            &database,
            &ProviderCatalog::builtins(),
            &crate::build_http_client().unwrap(),
            &started.session_token,
            key_material,
            now.saturating_add(1),
            CursorPollAuthority {
                required_tenant: Some("retired-provider-poll"),
                operator_service_id: None,
                allow_test_loopback: true,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid request: OAuth provider driver is no longer available"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cursor_poll_result_is_durable_single_use_and_replayable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/auth/poll"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "accessToken": "cursor-access-token",
                "refreshToken": "cursor-refresh-token",
                "accountId": "cursor-account-replay"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (_directory, database) = sqlite_database().await;
        let providers = ProviderCatalog::builtins();
        let now = crate::db::unix_millis();
        let key_material = b"test material with at least 32 bytes";
        let started = start_cursor_login(
            &database,
            StartCursorLogin {
                tenant_external_id: "cursor-replay".to_owned(),
                account_name: "cursor-primary".to_owned(),
                provider_driver: "http-json".to_owned(),
                provider_config: json!({"base_url": "https://provider.example"}),
                endpoints: CursorOAuthEndpoints {
                    login_url: format!("{}/auth/login", server.uri()),
                    poll_url: format!("{}/auth/poll", server.uri()),
                    refresh_url: format!("{}/auth/refresh", server.uri()),
                },
                oauth_driver: "cursor".to_owned(),
                reauthorize: None,
            },
            None,
            key_material,
            now,
        )
        .await
        .expect("start Cursor login");
        let (lease_owner, ready) = match poll_cursor_login(
            &database,
            &providers,
            &crate::build_http_client().expect("HTTP client"),
            &started.session_token,
            key_material,
            now.saturating_add(1),
            CursorPollAuthority {
                required_tenant: Some("cursor-replay"),
                operator_service_id: None,
                allow_test_loopback: true,
            },
        )
        .await
        .expect("poll Cursor login")
        {
            CursorPollResult::Ready { lease_owner, login } => (lease_owner, *login),
            other => panic!("unexpected Cursor result: {other:?}"),
        };
        let account = database
            .create_upstream_account(
                crate::db::CreateUpstreamAccountInput {
                    tenant_external_id: ready.tenant_external_id.clone(),
                    name: ready.account_name,
                    driver: ready.provider_driver,
                    config: ready.provider_config,
                    credential: ready.credential,
                    oauth_session_id: Some(ready.session_id),
                    oauth_driver: Some(ready.oauth_driver),
                    oauth_refresh_url: Some(ready.refresh_url),
                },
                key_material,
            )
            .await
            .expect("create Cursor upstream");
        database
            .finish_oauth_login_session(
                ready.session_id,
                lease_owner,
                account.id,
                now.saturating_add(2),
            )
            .await
            .expect("finish Cursor login");

        match poll_cursor_login(
            &database,
            &providers,
            &crate::build_http_client().expect("HTTP client"),
            &started.session_token,
            key_material,
            now.saturating_add(3),
            CursorPollAuthority {
                required_tenant: Some("cursor-replay"),
                operator_service_id: None,
                allow_test_loopback: true,
            },
        )
        .await
        .expect("replay consumed Cursor login")
        {
            CursorPollResult::Consumed { account_id, .. } => {
                assert_eq!(account_id, account.id);
            }
            other => panic!("unexpected replay result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cursor_refresh_transport_errors_never_echo_the_request_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let query_secret = "refresh-query-secret";
        let endpoint = format!("http://{address}/refresh?token={query_secret}");
        let credential = UpstreamCredential::OAuth {
            access_token: "old-access-secret".into(),
            refresh_token: Some("old-refresh-secret".into()),
            expires_at: Some(1),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: None,
            proxy_network_scope: None,
        };
        let error = refresh_cursor_credential(
            &crate::build_http_client().unwrap(),
            &endpoint,
            &credential,
            crate::db::unix_millis(),
            OutboundScope::Public,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "configured upstream is unavailable: Cursor OAuth refresh failed"
        );
        let rendered = format!("{error:?} {error}");
        let address = address.to_string();
        for secret in [query_secret, "old-refresh-secret", address.as_str()] {
            assert!(!rendered.contains(secret));
        }
    }

    #[test]
    fn managed_refresh_resolution_uses_current_catalog_and_fixed_mismatch_errors() {
        let catalog = ProviderCatalog::builtins();
        let adapter = resolve_managed_oauth_refresh_adapter(
            &catalog,
            "openai-codex",
            managed::codex::TOKEN_ENDPOINT,
        )
        .unwrap();
        assert_eq!(adapter.backend(), &ManagedOAuthAdapterBackend::Codex);

        let stored_secret_url = "https://stored-lifecycle-secret.invalid/token";
        let error =
            resolve_managed_oauth_refresh_adapter(&catalog, "openai-codex", stored_secret_url)
                .unwrap_err();
        assert_eq!(
            error.to_string(),
            "conflict: managed OAuth adapter lifecycle metadata no longer matches the active catalog"
        );
        assert!(!format!("{error:?} {error}").contains(stored_secret_url));
    }
}
