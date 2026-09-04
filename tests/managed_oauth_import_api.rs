use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use hmac::{Hmac, Mac};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{CreateServiceTokenInput, ImportManagedOAuthAccountInput, ManagedOAuthImportStatus},
    provider::{
        MANAGED_OAUTH_ADAPTER_API_VERSION, ManagedOAuthAdapterContribution, ProviderType,
        UpstreamCredential,
    },
};
use serde_json::{Value, json};
use sha2::Sha256;
use std::time::Duration;
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

const SOURCE_KEY_DOMAIN: &[u8] = b"memeloop:cpa-managed-oauth:source-key:v1\0";
const PAYLOAD_DIGEST_DOMAIN: &[u8] = b"memeloop:cpa-managed-oauth:payload-digest:v1\0";
const MAX_DOCUMENT: usize = 1024 * 1024;
const MAX_REQUEST: usize = MAX_DOCUMENT + 64 * 1024;

async fn test_state() -> (tempfile::TempDir, AppState, MockServer) {
    let directory = tempfile::tempdir().unwrap();
    let adapter = MockServer::start().await;
    let adapter_base = adapter.uri().replacen("127.0.0.1", "localhost", 1);
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("managed-import-api.db").display()
    );
    let mut state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap();
    state
        .providers
        .extend_for_test([ProviderType {
            id: "managed-api-test".into(),
            display_name: "Managed API test".into(),
            protocols: vec!["openai".into()],
            modalities: vec!["text".into()],
            config_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["base_url"],
                "properties": {
                    "base_url": {"type": "string", "format": "uri"},
                    "network_scope": {"type": "string", "enum": ["public", "private"]}
                }
            }),
            credential_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["type", "access_token"],
                "properties": {
                    "type": {"const": "oauth"},
                    "access_token": {"type": "string", "minLength": 1},
                    "refresh_token": {"type": ["string", "null"]},
                    "expires_at": {"type": "integer"},
                    "header": {"type": "string"},
                    "prefix": {"type": "string"},
                    "adapter_state": {}
                }
            }),
            oauth_adapter: None,
            managed_oauth_adapter: Some(ManagedOAuthAdapterContribution {
                api_version: MANAGED_OAUTH_ADAPTER_API_VERSION.into(),
                source_types: vec!["codex-account".into(), "gemini-account".into()],
                normalize_url: format!("{adapter_base}/normalize"),
                refresh_url: format!("{adapter_base}/refresh"),
            }),
            component_adapter: None,
            source: "test".into(),
        }])
        .unwrap();
    (directory, state, adapter)
}

async fn service_token(state: &AppState, scopes: &[&str], tenant: Option<&str>) -> String {
    state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: format!(
                    "managed import {} {}",
                    scopes.join("-"),
                    tenant.unwrap_or("global")
                ),
                scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
                tenant_external_id: tenant.map(str::to_owned),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap()
        .token
}

async fn call(
    state: &AppState,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if !body.is_empty() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 2 * MAX_REQUEST)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

async fn refresh_call(
    state: &AppState,
    account_id: uuid::Uuid,
    token: &str,
    idempotency_key: &str,
) -> (StatusCode, Vec<u8>) {
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/internal/v1/upstreams/{account_id}/oauth/refresh"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header("idempotency-key", idempotency_key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), MAX_REQUEST)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

fn import_request(tenant: &str, relative_path: &str, document: Value) -> Value {
    json!({
        "contract_version": 1,
        "tenant_external_id": tenant,
        "source": {"kind": "auth_file", "relative_path": relative_path},
        "source_type": "codex-account",
        "document": document,
    })
}

async fn seed_replay(
    state: &AppState,
    tenant: &str,
    relative_path: &str,
    source_type: &str,
    document: &Value,
) -> uuid::Uuid {
    seed_replay_with_expiry(
        state,
        tenant,
        relative_path,
        source_type,
        document,
        memeloop_token_center::db::unix_millis() + 3_600_000,
    )
    .await
}

async fn seed_replay_with_expiry(
    state: &AppState,
    tenant: &str,
    relative_path: &str,
    source_type: &str,
    document: &Value,
    expires_at: i64,
) -> uuid::Uuid {
    let source_key = source_key(state.config.key_pepper.as_bytes(), tenant, relative_path);
    let payload_digest = payload_digest(state.config.key_pepper.as_bytes(), source_type, document);
    state
        .db
        .import_cpa_managed_oauth_account(
            ImportManagedOAuthAccountInput {
                tenant_external_id: tenant.into(),
                source_key,
                payload_digest,
                contract_version: 1,
                account_name: format!("Imported {tenant}"),
                config: json!({"base_url": "https://api.example.test"}),
                credential: UpstreamCredential::OAuth {
                    access_token: "stored-access-secret".into(),
                    refresh_token: Some("stored-refresh-secret".into()),
                    expires_at: Some(expires_at),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: None,
                    proxy_url: None,
                    proxy_network_scope: None,
                },
                status: ManagedOAuthImportStatus::Active,
                adapter: state
                    .providers
                    .managed_oauth_adapter_for_source(source_type)
                    .unwrap(),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap()
        .account
        .id
}

#[tokio::test]
async fn managed_refresh_dispatches_through_catalog_with_scope_idempotency_and_worker_generation() {
    let (_directory, state, adapter) = test_state().await;
    let tenant = "managed-refresh-tenant";
    let account_id = seed_replay_with_expiry(
        &state,
        tenant,
        "auth/refresh.json",
        "codex-account",
        &json!({"source": "refresh-dispatch"}),
        memeloop_token_center::db::unix_millis() + 60_000,
    )
    .await;
    let tenant_token = service_token(&state, &["oauth:write"], Some(tenant)).await;
    let other_tenant_token =
        service_token(&state, &["oauth:write"], Some("managed-refresh-other")).await;
    let wrong_scope_token = service_token(&state, &["providers:write"], Some(tenant)).await;

    let candidates = state
        .db
        .list_managed_oauth_refresh_candidates(
            memeloop_token_center::db::unix_millis() + 5 * 60 * 1_000,
            20,
        )
        .await
        .unwrap();
    assert!(candidates.contains(&(account_id, 1)));

    Mock::given(method("POST"))
        .and(path("/refresh"))
        .and(body_partial_json(json!({
            "api_version": MANAGED_OAUTH_ADAPTER_API_VERSION,
            "credential": {
                "type": "oauth",
                "access_token": "stored-access-secret",
                "refresh_token": "stored-refresh-secret"
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "api_version": MANAGED_OAUTH_ADAPTER_API_VERSION,
            "credential": {
                "type": "oauth",
                "access_token": "refreshed-access-secret",
                "refresh_token": "refreshed-refresh-secret",
                "expires_at": memeloop_token_center::db::unix_millis() + 3_600_000,
                "header": "authorization",
                "prefix": "Bearer ",
                "adapter_state": {"generation": 2}
            }
        })))
        .expect(1)
        .mount(&adapter)
        .await;

    assert_eq!(
        refresh_call(
            &state,
            account_id,
            &other_tenant_token,
            "managed-refresh-cross-tenant"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        refresh_call(
            &state,
            account_id,
            &wrong_scope_token,
            "managed-refresh-wrong-scope"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    let idempotency_key = "managed-refresh-generation-1";
    let (status, body) = refresh_call(&state, account_id, &tenant_token, idempotency_key).await;
    assert_eq!(status, StatusCode::OK);
    let refreshed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(refreshed["id"], account_id.to_string());
    assert_eq!(refreshed["credential_generation"], 2);
    for secret in [
        "stored-access-secret",
        "stored-refresh-secret",
        "refreshed-access-secret",
        "refreshed-refresh-secret",
    ] {
        assert!(!String::from_utf8_lossy(&body).contains(secret));
    }

    let (replay_status, replay_body) =
        refresh_call(&state, account_id, &tenant_token, idempotency_key).await;
    assert_eq!(replay_status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&replay_body).unwrap()["credential_generation"],
        2
    );
    let (_, credential) = state
        .db
        .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let credential = serde_json::to_value(credential).unwrap();
    assert_eq!(credential["access_token"], "refreshed-access-secret");
    assert_eq!(credential["refresh_token"], "refreshed-refresh-secret");
    assert_eq!(credential["adapter_state"]["generation"], 2);
}

#[tokio::test]
async fn capabilities_and_import_are_global_only_and_dedicated_scope_only() {
    let (_directory, state, _adapter) = test_state().await;
    let global = service_token(&state, &["imports:cpa:write"], None).await;
    let wrong_scope = service_token(&state, &["providers:write"], None).await;
    let tenant = service_token(&state, &["imports:cpa:write"], Some("managed-auth-tenant")).await;
    let request = serde_json::to_vec(&import_request(
        "managed-auth-tenant",
        "auth/codex.json",
        json!({"token": "never-call-adapter"}),
    ))
    .unwrap();

    for path in [
        "/internal/v1/imports/cpa/managed-oauth/capabilities",
        "/internal/v1/imports/cpa/managed-oauth",
    ] {
        let method = if path.ends_with("capabilities") {
            "GET"
        } else {
            "POST"
        };
        let body = if method == "POST" {
            request.clone()
        } else {
            Vec::new()
        };
        assert_eq!(
            call(&state, method, path, None, body.clone()).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&state, method, path, Some(&wrong_scope), body.clone())
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&state, method, path, Some(&tenant), body).await.0,
            StatusCode::FORBIDDEN
        );
    }

    let (status, body) = call(
        &state,
        "GET",
        "/internal/v1/imports/cpa/managed-oauth/capabilities",
        Some(&global),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["contract_version"], 1);
    let source_types = value["source_types"].as_array().unwrap();
    assert!(
        source_types
            .windows(2)
            .all(|pair| pair[0].as_str() < pair[1].as_str())
    );
    assert!(source_types.contains(&json!("codex-account")));
    assert!(source_types.contains(&json!("gemini-account")));
    let encoded = String::from_utf8(body).unwrap();
    for forbidden in ["driver", "normalize", "refresh", "https://", ".test"] {
        assert!(!encoded.contains(forbidden));
    }
}

#[tokio::test]
async fn legacy_gemini_remains_importable_without_advertising_or_scheduling_refresh() {
    let (_directory, state, _adapter) = test_state().await;
    let tenant = "legacy-gemini-capability";
    let import_token = service_token(&state, &["imports:cpa:write"], None).await;
    let oauth_token = service_token(&state, &["oauth:write"], Some(tenant)).await;
    let (status, capabilities) = call(
        &state,
        "GET",
        "/internal/v1/imports/cpa/managed-oauth/capabilities",
        Some(&import_token),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        serde_json::from_slice::<Value>(&capabilities).unwrap()["source_types"]
            .as_array()
            .unwrap()
            .contains(&json!("gemini-legacy"))
    );

    let created = state
        .db
        .import_cpa_managed_oauth_account(
            ImportManagedOAuthAccountInput {
                tenant_external_id: tenant.into(),
                source_key: "a".repeat(64),
                payload_digest: "b".repeat(64),
                contract_version: 1,
                account_name: "Legacy Gemini".into(),
                config: json!({
                    "base_url": "https://cloudcode-pa.googleapis.com",
                    "network_scope": "public"
                }),
                credential: UpstreamCredential::OAuth {
                    access_token: "legacy-gemini-access-secret".into(),
                    refresh_token: Some("legacy-gemini-refresh-secret".into()),
                    expires_at: Some(4_070_908_800_000),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: Some(json!({
                        "schema": "cpa-gemini-cli-oauth-v1",
                        "project_id": "legacy-project-123"
                    })),
                    proxy_url: None,
                    proxy_network_scope: None,
                },
                status: ManagedOAuthImportStatus::Active,
                adapter: state
                    .providers
                    .managed_oauth_adapter_for_source("gemini-legacy")
                    .unwrap(),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(created.account.driver, "cpa-gemini-oauth-legacy");
    assert!(!created.account.can_refresh);
    let account_id = created.account.id;

    assert!(
        !state
            .db
            .list_managed_oauth_refresh_candidates(i64::MAX, 20)
            .await
            .unwrap()
            .iter()
            .any(|(candidate, _)| *candidate == account_id)
    );
    let (refresh_status, refresh_body) =
        refresh_call(&state, account_id, &oauth_token, "legacy-gemini-refresh").await;
    assert_eq!(refresh_status, StatusCode::BAD_REQUEST);
    let refresh_body = String::from_utf8(refresh_body).unwrap();
    assert!(!refresh_body.contains("legacy-gemini-refresh-secret"));
    assert!(!refresh_body.contains("oauth2.googleapis.com"));
    let (_, credential) = state
        .db
        .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(credential).unwrap()["access_token"],
        "legacy-gemini-access-secret"
    );
}

#[tokio::test]
async fn exact_replay_canonicalizes_objects_and_never_exposes_or_calls_adapter() {
    let (_directory, state, adapter) = test_state().await;
    Mock::given(method("POST"))
        .and(path("/normalize"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&adapter)
        .await;
    let token = service_token(&state, &["imports:cpa:write"], None).await;
    let tenant = "managed-replay-tenant";
    let relative_path = "deep/private-codex.json";
    let secret = "source-document-super-secret";
    let original = json!({
        "tokens": {"refresh_token": secret, "access_token": "access-secret"},
        "ordered": [1, 2, 3],
    });
    let account_id = seed_replay(&state, tenant, relative_path, "codex-account", &original).await;
    let reordered: Value = serde_json::from_str(&format!(
        r#"{{"ordered":[1,2,3],"tokens":{{"access_token":"access-secret","refresh_token":"{secret}"}}}}"#
    ))
    .unwrap();
    let body = serde_json::to_vec(&import_request(tenant, relative_path, reordered)).unwrap();
    let (status, response) = call(
        &state,
        "POST",
        "/internal/v1/imports/cpa/managed-oauth",
        Some(&token),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&response).unwrap();
    assert_eq!(value["disposition"], "replayed");
    assert_eq!(value["account"]["id"], account_id.to_string());
    let response = String::from_utf8(response).unwrap();
    for forbidden in [
        relative_path,
        secret,
        "access-secret",
        "stored-access-secret",
        "source_key",
        "payload_digest",
        "hmac",
    ] {
        assert!(!response.to_ascii_lowercase().contains(forbidden));
    }
}

#[tokio::test]
async fn changed_payload_and_source_type_conflict_before_catalog_resolution() {
    let (_directory, state, adapter) = test_state().await;
    Mock::given(method("POST"))
        .and(path("/normalize"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&adapter)
        .await;
    let token = service_token(&state, &["imports:cpa:write"], None).await;
    let tenant = "managed-conflict-tenant";
    let path = "auth/account.json";
    let original = json!({"access_token": "original", "config": {"region": "one"}});
    seed_replay(&state, tenant, path, "codex-account", &original).await;

    let mut variants = vec![import_request(
        tenant,
        path,
        json!({"access_token": "changed", "config": {"region": "one"}}),
    )];
    variants.push(import_request(
        tenant,
        path,
        json!({"access_token": "original", "config": {"region": "two"}}),
    ));
    let mut unsupported = import_request(tenant, path, original);
    unsupported["source_type"] = json!("currently-unsupported");
    variants.push(unsupported);

    for body in variants {
        let (status, response) = call(
            &state,
            "POST",
            "/internal/v1/imports/cpa/managed-oauth",
            Some(&token),
            serde_json::to_vec(&body).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let response = String::from_utf8(response).unwrap();
        assert!(!response.contains("changed"));
        assert!(!response.contains("currently-unsupported"));
        assert!(!response.contains(path));
    }
}

#[tokio::test]
async fn request_shape_paths_and_size_limits_fail_closed_without_leaks() {
    let (_directory, state, _adapter) = test_state().await;
    let token = service_token(&state, &["imports:cpa:write"], None).await;
    let endpoint = "/internal/v1/imports/cpa/managed-oauth";

    for injected in [
        ("driver", json!("http-json")),
        ("account_name", json!("attacker-selected")),
        (
            "provider_config",
            json!({"oauth": {"refresh_url": "https://attacker.example/refresh"}}),
        ),
        ("adapter_url", json!("https://attacker.example/normalize")),
        ("refresh_url", json!("https://attacker.example/refresh")),
    ] {
        let mut request = import_request(
            "managed-shape-tenant",
            "auth/shape.json",
            json!({"token": "shape-secret"}),
        );
        request[injected.0] = injected.1;
        assert_eq!(
            call(
                &state,
                "POST",
                endpoint,
                Some(&token),
                serde_json::to_vec(&request).unwrap(),
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }

    for path in [
        "/absolute/secret.json",
        "auth//secret.json",
        "auth/./secret.json",
        "auth/../secret.json",
        "auth\\secret.json",
        "auth/secret\0.json",
    ] {
        let (status, response) = call(
            &state,
            "POST",
            endpoint,
            Some(&token),
            serde_json::to_vec(&import_request(
                "managed-path-tenant",
                path,
                json!({"token": "path-secret"}),
            ))
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let response = String::from_utf8(response).unwrap();
        assert!(!response.contains(path));
        assert!(!response.contains("path-secret"));
    }

    let too_large_document = import_request(
        "managed-size-tenant",
        "auth/large.json",
        Value::String("s".repeat(MAX_DOCUMENT - 1)),
    );
    let encoded = serde_json::to_vec(&too_large_document).unwrap();
    assert!(encoded.len() < MAX_REQUEST);
    assert_eq!(
        call(&state, "POST", endpoint, Some(&token), encoded)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );

    let oversized_body = vec![b' '; MAX_REQUEST + 1];
    assert_eq!(
        call(&state, "POST", endpoint, Some(&token), oversized_body)
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn adapter_failures_and_invalid_results_are_static_and_write_nothing() {
    let (_directory, state, adapter) = test_state().await;
    let token = service_token(&state, &["imports:cpa:write"], None).await;
    let endpoint = "/internal/v1/imports/cpa/managed-oauth";
    let tenant = "managed-adapter-failure-tenant";

    Mock::given(method("POST"))
        .and(path("/normalize"))
        .and(body_partial_json(json!({"payload": {"case": "4xx"}})))
        .respond_with(ResponseTemplate::new(422).set_body_string("adapter-response-super-secret"))
        .expect(1)
        .mount(&adapter)
        .await;
    Mock::given(method("POST"))
        .and(path("/normalize"))
        .and(body_partial_json(
            json!({"payload": {"case": "invalid-json"}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("not-json-secret"))
        .expect(1)
        .mount(&adapter)
        .await;
    Mock::given(method("POST"))
        .and(path("/normalize"))
        .and(body_partial_json(json!({"payload": {"case": "oversize"}})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("oversize-response-secret".repeat(MAX_DOCUMENT / 24 + 2)),
        )
        .expect(1)
        .mount(&adapter)
        .await;
    Mock::given(method("POST"))
        .and(path("/normalize"))
        .and(body_partial_json(json!({"payload": {"case": "timeout"}})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(11))
                .set_body_string("delayed-response-secret"),
        )
        .expect(1)
        .mount(&adapter)
        .await;
    Mock::given(method("POST"))
        .and(path("/normalize"))
        .and(body_partial_json(json!({"payload": {"case": "ssrf"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "api_version": MANAGED_OAUTH_ADAPTER_API_VERSION,
            "account": {
                "account_name": "SSRF must fail",
                "config": {"base_url": "http://169.254.169.254/latest"},
                "enabled": true,
                "credential": {
                    "type": "oauth",
                    "access_token": "ssrf-access-secret"
                }
            }
        })))
        .expect(1)
        .mount(&adapter)
        .await;
    Mock::given(method("POST"))
        .and(path("/normalize"))
        .and(body_partial_json(
            json!({"payload": {"case": "expired-no-refresh"}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "api_version": MANAGED_OAUTH_ADAPTER_API_VERSION,
            "account": {
                "account_name": "Expired must fail",
                "config": {
                    "base_url": format!("{}/provider", adapter.uri()),
                    "network_scope": "private"
                },
                "enabled": true,
                "credential": {
                    "type": "oauth",
                    "access_token": "expired-access-secret",
                    "expires_at": 1
                }
            }
        })))
        .expect(1)
        .mount(&adapter)
        .await;

    for (case, expected) in [
        ("4xx", StatusCode::BAD_GATEWAY),
        ("invalid-json", StatusCode::BAD_GATEWAY),
        ("oversize", StatusCode::BAD_GATEWAY),
        ("timeout", StatusCode::BAD_GATEWAY),
        ("ssrf", StatusCode::BAD_GATEWAY),
        ("expired-no-refresh", StatusCode::BAD_REQUEST),
    ] {
        let (status, response) = call(
            &state,
            "POST",
            endpoint,
            Some(&token),
            serde_json::to_vec(&import_request(
                tenant,
                &format!("auth/{case}.json"),
                json!({"case": case, "source_secret": "request-document-secret"}),
            ))
            .unwrap(),
        )
        .await;
        assert_eq!(status, expected, "case {case}");
        let response = String::from_utf8(response).unwrap();
        for forbidden in [
            case,
            "request-document-secret",
            "adapter-response-super-secret",
            "not-json-secret",
            "oversize-response-secret",
            "delayed-response-secret",
            "169.254.169.254",
            "ssrf-access-secret",
            "expired-access-secret",
        ] {
            assert!(!response.contains(forbidden), "case {case}: {response}");
        }
        assert!(
            state
                .db
                .list_upstream_accounts(tenant)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn created_import_calls_adapter_once_and_replay_calls_it_zero_more_times() {
    let (_directory, state, adapter) = test_state().await;
    let token = service_token(&state, &["imports:cpa:write"], None).await;
    let tenant = "managed-created-tenant";
    Mock::given(method("POST"))
        .and(path("/normalize"))
        .and(body_partial_json(json!({"payload": {"case": "created"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "api_version": MANAGED_OAUTH_ADAPTER_API_VERSION,
            "account": {
                "account_name": "Created managed account",
                "config": {
                    "base_url": format!("{}/provider", adapter.uri()),
                    "network_scope": "private"
                },
                "enabled": true,
                "credential": {
                    "type": "oauth",
                    "access_token": "created-access-secret",
                    "refresh_token": "created-refresh-secret",
                    "expires_at": memeloop_token_center::db::unix_millis() + 3_600_000
                }
            }
        })))
        .expect(1)
        .mount(&adapter)
        .await;
    let body = serde_json::to_vec(&import_request(
        tenant,
        "auth/created.json",
        json!({"case": "created", "source_secret": "created-source-secret"}),
    ))
    .unwrap();

    let (created_status, created_body) = call(
        &state,
        "POST",
        "/internal/v1/imports/cpa/managed-oauth",
        Some(&token),
        body.clone(),
    )
    .await;
    assert_eq!(created_status, StatusCode::CREATED);
    let created: Value = serde_json::from_slice(&created_body).unwrap();
    assert_eq!(created["disposition"], "created");
    assert_eq!(created["account"]["status"], "active");

    let (replay_status, replay_body) = call(
        &state,
        "POST",
        "/internal/v1/imports/cpa/managed-oauth",
        Some(&token),
        body,
    )
    .await;
    assert_eq!(replay_status, StatusCode::OK);
    let replay: Value = serde_json::from_slice(&replay_body).unwrap();
    assert_eq!(replay["disposition"], "replayed");
    assert_eq!(replay["account"]["id"], created["account"]["id"]);

    for response in [created_body, replay_body] {
        let response = String::from_utf8(response).unwrap();
        for forbidden in [
            "created-source-secret",
            "created-access-secret",
            "created-refresh-secret",
            "auth/created.json",
            "source_key",
            "payload_digest",
        ] {
            assert!(!response.contains(forbidden));
        }
    }
}

#[tokio::test]
async fn concurrent_same_and_mixed_payloads_have_one_atomic_winner() {
    for mixed in [false, true] {
        let (_directory, state, adapter) = test_state().await;
        let token = service_token(&state, &["imports:cpa:write"], None).await;
        let tenant = if mixed {
            "managed-concurrent-mixed"
        } else {
            "managed-concurrent-same"
        };
        Mock::given(method("POST"))
            .and(path("/normalize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "api_version": MANAGED_OAUTH_ADAPTER_API_VERSION,
                "account": {
                    "account_name": "Concurrent managed account",
                    "config": {
                        "base_url": format!("{}/provider", adapter.uri()),
                        "network_scope": "private"
                    },
                    "enabled": true,
                    "credential": {
                        "type": "oauth",
                        "access_token": "concurrent-access-secret",
                        "refresh_token": "concurrent-refresh-secret",
                        "expires_at": memeloop_token_center::db::unix_millis() + 3_600_000
                    }
                }
            })))
            .mount(&adapter)
            .await;
        let left = serde_json::to_vec(&import_request(
            tenant,
            "auth/concurrent.json",
            json!({"access_token": "left-source-secret"}),
        ))
        .unwrap();
        let right = if mixed {
            serde_json::to_vec(&import_request(
                tenant,
                "auth/concurrent.json",
                json!({"access_token": "right-source-secret"}),
            ))
            .unwrap()
        } else {
            left.clone()
        };

        let (left_result, right_result) = tokio::join!(
            call(
                &state,
                "POST",
                "/internal/v1/imports/cpa/managed-oauth",
                Some(&token),
                left,
            ),
            call(
                &state,
                "POST",
                "/internal/v1/imports/cpa/managed-oauth",
                Some(&token),
                right,
            )
        );
        let mut statuses = [left_result.0, right_result.0];
        statuses.sort_unstable();
        assert_eq!(
            statuses,
            if mixed {
                [StatusCode::CREATED, StatusCode::CONFLICT]
            } else {
                [StatusCode::OK, StatusCode::CREATED]
            }
        );
        assert_eq!(
            state.db.list_upstream_accounts(tenant).await.unwrap().len(),
            1
        );
        let adapter_calls = adapter.received_requests().await.unwrap().len();
        assert!((1..=2).contains(&adapter_calls));
        for response in [left_result.1, right_result.1] {
            let response = String::from_utf8(response).unwrap();
            for forbidden in [
                "left-source-secret",
                "right-source-secret",
                "concurrent-access-secret",
                "concurrent-refresh-secret",
                "auth/concurrent.json",
            ] {
                assert!(!response.contains(forbidden));
            }
        }
    }
}

#[tokio::test]
async fn native_codex_upgrade_api_is_global_allowlisted_and_never_returns_proxy_material() {
    let (_directory, state, _adapter) = test_state().await;
    let account = state
        .db
        .import_cpa_managed_oauth_account(
            ImportManagedOAuthAccountInput {
                tenant_external_id: "native-upgrade".into(),
                source_key: "a".repeat(64),
                payload_digest: "b".repeat(64),
                contract_version: 1,
                account_name: "Native account".into(),
                config: json!({
                    "base_url": "https://chatgpt.com/backend-api/codex",
                    "network_scope": "public",
                    "reservation_token_bounds": {"gpt-5.6-sol": 128000}
                }),
                credential: UpstreamCredential::OAuth {
                    access_token: "native-upgrade-access-secret".into(),
                    refresh_token: Some("native-upgrade-refresh-secret".into()),
                    expires_at: Some(memeloop_token_center::db::unix_millis() + 3_600_000),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: Some(json!({
                        "schema": "openai-codex-oauth-v1",
                        "account_id": "native-account-123"
                    })),
                    proxy_url: Some("socks5://operator:proxy-secret@100.64.0.16:1080".into()),
                    proxy_network_scope: Some(
                        memeloop_token_center::network::OutboundScope::Private,
                    ),
                },
                status: ManagedOAuthImportStatus::Active,
                adapter: state
                    .providers
                    .managed_oauth_adapter_for_source("codex")
                    .unwrap(),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap()
        .account;
    let token = service_token(&state, &["providers:write"], None).await;
    let (status, plan_body) = call(
        &state,
        "POST",
        "/internal/v1/migrations/openai-codex/prepare",
        Some(&token),
        serde_json::to_vec(&json!({"account_ids": [account.id]})).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let plan: Value = serde_json::from_slice(&plan_body).unwrap();
    assert_eq!(plan["target_count"], 1);
    assert_eq!(plan["targets"][0]["account_id"], account.id.to_string());
    assert_eq!(plan["targets"][0]["has_proxy"], true);
    assert_eq!(plan["targets"][0]["proxy_network_scope"], "private");
    let plan_text = String::from_utf8(plan_body).unwrap();
    for secret in [
        "native-upgrade-access-secret",
        "native-upgrade-refresh-secret",
        "operator:proxy-secret",
        "100.64.0.16",
    ] {
        assert!(!plan_text.contains(secret));
    }
    let (status, result_body) = call(
        &state,
        "POST",
        "/internal/v1/migrations/openai-codex/apply",
        Some(&token),
        serde_json::to_vec(&json!({"targets": plan["targets"]})).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: Value = serde_json::from_slice(&result_body).unwrap();
    assert_eq!(result["upgraded_count"], 1);
    assert_eq!(
        result["upgraded_account_ids"][0],
        account.id.to_string()
    );
    assert_eq!(result["already_native_count"], 0);
}

#[tokio::test]
async fn exact_one_mib_document_and_tenant_provenance_are_replayable() {
    let (_directory, state, adapter) = test_state().await;
    Mock::given(method("POST"))
        .and(path("/normalize"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&adapter)
        .await;
    let token = service_token(&state, &["imports:cpa:write"], None).await;
    let path = "auth/boundary.json";
    // JSON string quotes account for the final two encoded bytes.
    let document = Value::String("x".repeat(MAX_DOCUMENT - 2));
    let account_a = seed_replay(
        &state,
        "managed-boundary-a",
        path,
        "codex-account",
        &document,
    )
    .await;
    let account_b = seed_replay(
        &state,
        "managed-boundary-b",
        path,
        "codex-account",
        &document,
    )
    .await;
    assert_ne!(account_a, account_b);

    for (tenant, expected) in [
        ("managed-boundary-a", account_a),
        ("managed-boundary-b", account_b),
    ] {
        let encoded = serde_json::to_vec(&import_request(tenant, path, document.clone())).unwrap();
        assert!(encoded.len() > MAX_DOCUMENT);
        assert!(encoded.len() <= MAX_REQUEST);
        let (status, response) = call(
            &state,
            "POST",
            "/internal/v1/imports/cpa/managed-oauth",
            Some(&token),
            encoded,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let response: Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(response["account"]["id"], expected.to_string());
    }
}

fn source_key(pepper: &[u8], tenant: &str, relative_path: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper).unwrap();
    mac.update(SOURCE_KEY_DOMAIN);
    mac.update(tenant.as_bytes());
    mac.update(b"\0auth_file\0");
    mac.update(relative_path.as_bytes());
    lower_hex(&mac.finalize().into_bytes())
}

fn payload_digest(pepper: &[u8], source_type: &str, document: &Value) -> String {
    let canonical = canonical_json(&json!({
        "contract_version": 1,
        "source_type": source_type,
        "document": canonical_json(document),
    }));
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper).unwrap();
    mac.update(PAYLOAD_DIGEST_DOMAIN);
    mac.update(&serde_json::to_vec(&canonical).unwrap());
    lower_hex(&mac.finalize().into_bytes())
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::Object(values) => {
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by_key(|(key, _)| *key);
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), canonical_json(value)))
                    .collect(),
            )
        }
        _ => value.clone(),
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
