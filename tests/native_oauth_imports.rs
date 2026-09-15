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
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use uuid::Uuid;

const CAPABILITIES: &str = "/internal/v1/native-oauth-imports/capabilities";
const COHORT: &str = "/internal/v1/native-oauth-imports/kimi-cohort";

#[tokio::test]
async fn native_cursor_source_http_contract_preserves_identity_and_hides_tokens() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::initialize(Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("cursor-source.db").display()
    )))
    .await
    .unwrap();
    state.db.create_tenant("cursor-source", None).await.unwrap();
    let mut tokens = Vec::new();
    for (name, tenant, scopes) in [
        ("global", None, vec!["upstreams:import:write".into()]),
        (
            "tenant",
            Some("cursor-source".into()),
            vec!["upstreams:import:write".into()],
        ),
        ("wrong", None, vec!["providers:write".into()]),
    ] {
        tokens.push(
            state
                .db
                .create_service_token(
                    CreateServiceTokenInput {
                        name: name.into(),
                        tenant_external_id: tenant,
                        scopes,
                    },
                    state.config.key_pepper.as_bytes(),
                )
                .await
                .unwrap(),
        );
    }
    let access = format!(
        "{}.{}.synthetic-signature",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256"}"#),
        URL_SAFE_NO_PAD.encode(r#"{"sub":"cursor-native-http-fixture","exp":1000}"#)
    );
    let document = json!({"accessToken":access,"refreshToken":"cursor-source-synthetic-refresh", "unrelatedTheme":"light"});
    let mut body = json!({
        "contract":"source-bound-native-cursor-v1", "tenant_external_id":"cursor-source",
        "account_name":"Cursor Imported", "source_identity_hash":"a".repeat(64),
        "source_document_sha256":digest(&document), "source_layout":"legacy-auth-v1",
        "source_relative_path":"account-home/legacy-auth.json", "document":document,
        "proxy_url":"socks5h://192.168.1.20:1080",
    });
    let path = "/internal/v1/native-oauth-imports/cursor";
    for token in &tokens[1..] {
        assert_eq!(
            call(&state, "POST", path, &token.token, Some(&body))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    body["source_document_sha256"] = json!("b".repeat(64));
    assert_eq!(
        call(&state, "POST", path, &tokens[0].token, Some(&body))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    body["source_document_sha256"] = json!(digest(&body["document"]));
    let (status, created, bytes) = call(&state, "POST", path, &tokens[0].token, Some(&body)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["disposition"], "created");
    assert_eq!(created["account"]["driver"], "cursor");
    assert_eq!(created["account"]["credential_expires_at"], 1_000_000);
    assert_eq!(
        created["account"]["import_source_document_sha256"],
        body["source_document_sha256"]
    );
    let response = String::from_utf8(bytes).unwrap();
    assert!(!response.contains(&access));
    assert!(!response.contains("cursor-source-synthetic-refresh"));
    let (status, replay, _) = call(&state, "POST", path, &tokens[0].token, Some(&body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["disposition"], "replayed");
    assert_eq!(replay["account"]["id"], created["account"]["id"]);
    assert_eq!(replay["account"]["credential_generation"], 1);
    body["document"]["accessToken"] = json!(format!(
        "{}.{}.synthetic-signature",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256"}"#),
        URL_SAFE_NO_PAD.encode(r#"{"sub":"another-subject","exp":1000}"#)
    ));
    body["source_document_sha256"] = json!(digest(&body["document"]));
    body["expected_current_account_id"] = created["account"]["id"].clone();
    body["expected_current_document_sha256"] =
        created["account"]["import_source_document_sha256"].clone();
    body["expected_current_credential_generation"] = json!(1);
    assert_eq!(
        call(&state, "POST", path, &tokens[0].token, Some(&body))
            .await
            .0,
        StatusCode::CONFLICT
    );
}

async fn call(
    state: &AppState,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&Value>,
) -> (StatusCode, Value, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    let body = match body {
        Some(value) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(value).unwrap())
        }
        None => Body::empty(),
    };
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 3 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value, bytes)
}

fn digest(value: &Value) -> String {
    format!("{:x}", Sha256::digest(serde_json::to_vec(value).unwrap()))
}

fn account(identity: char, path: &str, secret: &str, expiry: &str) -> Value {
    let document = json!({
        "access_token": format!("{secret}-access"),
        "device_id": format!("device-{identity}"),
        "disabled": false,
        "expired": expiry,
        "last_refresh": "2026-09-01T00:00:00Z",
        "refresh_token": format!("{secret}-refresh"),
        "scope": "coding",
        "token_type": "Bearer",
        "type": "kimi"
    });
    json!({
        "source": {"kind": "auth_file", "relative_path": path},
        "source_type": "kimi",
        "source_identity_hash": identity.to_string().repeat(64),
        "source_document_sha256": digest(&document),
        "expected_current_account_id": null,
        "expected_current_document_sha256": null,
        "expected_current_credential_generation": null,
        "document": document
    })
}

fn cohort_request(tenant: &str, accounts: Vec<Value>) -> Value {
    let current = Value::Array(
        accounts
            .iter()
            .map(|account| {
                json!({
                    "account_id": account["expected_current_account_id"],
                    "credential_generation": account["expected_current_credential_generation"],
                    "source_document_sha256": account["expected_current_document_sha256"],
                    "source_identity_hash": account["source_identity_hash"]
                })
            })
            .collect(),
    );
    let next = Value::Array(
        accounts
            .iter()
            .map(|account| {
                json!({
                    "source_document_sha256": account["source_document_sha256"],
                    "source_identity_hash": account["source_identity_hash"]
                })
            })
            .collect(),
    );
    json!({
        "contract_version": 2,
        "tenant_external_id": tenant,
        "cohort_contract": "atomic_kimi_cohort_v2",
        "approval": {
            "contract": "kimi-cohort-digest-pair-v1",
            "expected_current_cohort_sha256": digest(&current),
            "new_cohort_sha256": digest(&next)
        },
        "accounts": accounts
    })
}

fn bind_current(accounts: &mut [Value], response: &Value) {
    for (requested, current) in accounts
        .iter_mut()
        .zip(response["accounts"].as_array().unwrap())
    {
        requested["expected_current_account_id"] = current["id"].clone();
        requested["expected_current_document_sha256"] =
            current["import_source_document_sha256"].clone();
        requested["expected_current_credential_generation"] =
            current["credential_generation"].clone();
    }
}

#[tokio::test]
async fn native_kimi_cohort_contract_is_atomic_rotatable_and_secret_free() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("native-kimi.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap();
    state
        .db
        .create_tenant("native-kimi-cohort", None)
        .await
        .unwrap();
    let issued = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "native OAuth import fixture".into(),
                scopes: vec!["upstreams:import:write".into(), "providers:read".into()],
                tenant_external_id: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let tenant_scoped = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "tenant-scoped native import fixture".into(),
                scopes: vec!["upstreams:import:write".into()],
                tenant_external_id: Some("native-kimi-cohort".into()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let wrong_scope = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "wrong-scope native import fixture".into(),
                scopes: vec!["providers:write".into()],
                tenant_external_id: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();

    assert_eq!(
        call(&state, "GET", CAPABILITIES, &tenant_scoped.token, None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&state, "GET", CAPABILITIES, &wrong_scope.token, None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );

    let (status, capabilities, _) = call(&state, "GET", CAPABILITIES, &issued.token, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(capabilities["contract_version"], 2);
    assert_eq!(
        capabilities["atomic_cohort_contracts"],
        json!(["atomic_kimi_cohort_v2"])
    );
    assert_eq!(
        capabilities["credential_lifecycle_policies"]["kimi"]["expired_access_token"],
        "managed_refresh_required"
    );

    let tenant = "native-kimi-cohort";
    let mut accounts = vec![
        account(
            'e',
            "auth/second.json",
            "fixture-second",
            "2099-01-01T00:00:00Z",
        ),
        account(
            'd',
            "auth/first.json",
            "fixture-first",
            "2099-01-01T00:00:00Z",
        ),
    ];
    accounts[0]["document"]["proxy_url"] = json!("socks5h://10.0.0.1:1080");
    accounts[0]["source_document_sha256"] = json!(digest(&accounts[0]["document"]));
    let create = cohort_request(tenant, accounts.clone());
    let mut unknown = create.clone();
    unknown["unexpected"] = json!(true);
    assert_eq!(
        call(&state, "POST", COHORT, &issued.token, Some(&unknown))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (status, created, bytes) = call(&state, "POST", COHORT, &issued.token, Some(&create)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["disposition"], "created");
    assert_eq!(created["accounts"][0]["name"], "Kimi OAuth 2");
    assert_eq!(created["accounts"][1]["name"], "Kimi OAuth 1");
    // Batch import responses use the same public account boundary as list and
    // individual mutations: only the fixed non-secret config is exposed.
    for account in created["accounts"].as_array().unwrap() {
        let keys = account["config"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec!["base_url", "network_scope", "reservation_token_bounds"]
        );
    }
    assert!(
        created["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|account| {
                account["driver"] == "kimi-oauth"
                    && account["auth_kind"] == "oauth"
                    && account["status"] == "active"
                    && account["credential_generation"] == 1
                    && account["route_count"] == 0
            })
    );
    assert_eq!(
        created["accounts"][0]["import_source_identity_hash"],
        "e".repeat(64)
    );
    assert_eq!(created["accounts"][0]["has_proxy"], true);
    assert_eq!(created["accounts"][0]["proxy_scheme"], "socks5h");
    assert_eq!(created["accounts"][0]["proxy_remote_dns"], true);
    assert_eq!(created["accounts"][1]["has_proxy"], false);
    let response = String::from_utf8(bytes).unwrap();
    for forbidden in [
        "auth/second.json",
        "auth/first.json",
        "fixture-second",
        "fixture-first",
        "10.0.0.1",
    ] {
        assert!(!response.contains(forbidden));
    }
    let (status, create_retry, _) =
        call(&state, "POST", COHORT, &issued.token, Some(&create)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(create_retry["disposition"], "replayed");
    assert_eq!(create_retry["accounts"][0]["credential_generation"], 1);
    assert_eq!(create_retry["accounts"][1]["credential_generation"], 1);
    let (status, inventory, _) = call(
        &state,
        "GET",
        "/internal/v1/upstreams?tenant_external_id=native-kimi-cohort",
        &issued.token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inventory.as_array().unwrap().len(), 2);
    assert!(inventory.as_array().unwrap().iter().all(|account| {
        account["import_source_identity_hash"].is_string()
            && account["import_source_document_sha256"].is_string()
    }));

    bind_current(&mut accounts, &created);
    let replay = cohort_request(tenant, accounts.clone());
    let (status, replayed, _) = call(&state, "POST", COHORT, &issued.token, Some(&replay)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replayed["disposition"], "replayed");

    let stale = accounts.clone();
    for (index, account) in accounts.iter_mut().enumerate() {
        account["document"]["access_token"] = json!(format!("rotated-{index}-access"));
        account["document"]["refresh_token"] = json!(format!("rotated-{index}-refresh"));
        account["source_document_sha256"] = json!(digest(&account["document"]));
    }
    let rotation = cohort_request(tenant, accounts.clone());
    let (status, rotated, _) = call(&state, "POST", COHORT, &issued.token, Some(&rotation)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rotated["disposition"], "rotated");
    assert_eq!(rotated["accounts"][0]["credential_generation"], 2);
    assert_eq!(rotated["accounts"][1]["credential_generation"], 2);
    let (status, rotation_retry, _) =
        call(&state, "POST", COHORT, &issued.token, Some(&rotation)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rotation_retry["disposition"], "replayed");
    assert_eq!(rotation_retry["accounts"][0]["credential_generation"], 2);
    assert_eq!(rotation_retry["accounts"][1]["credential_generation"], 2);
    let stale = cohort_request(tenant, stale);
    assert_eq!(
        call(&state, "POST", COHORT, &issued.token, Some(&stale))
            .await
            .0,
        StatusCode::CONFLICT
    );

    let first_id = Uuid::parse_str(rotated["accounts"][0]["id"].as_str().unwrap()).unwrap();
    state
        .db
        .set_upstream_account_status(
            first_id,
            tenant,
            "disabled",
            rotated["accounts"][0]["updated_at"].as_i64().unwrap(),
        )
        .await
        .unwrap();
    bind_current(&mut accounts, &rotated);
    let disabled = cohort_request(tenant, accounts);
    assert_eq!(
        call(&state, "POST", COHORT, &issued.token, Some(&disabled))
            .await
            .0,
        StatusCode::CONFLICT
    );

    let expired_tenant = "native-kimi-expired";
    let expired = cohort_request(
        expired_tenant,
        vec![
            account('a', "auth/a.json", "current", "2099-01-01T00:00:00Z"),
            account('b', "auth/b.json", "expired", "2000-01-01T00:00:00Z"),
        ],
    );
    let (status, imported_expired, _) =
        call(&state, "POST", COHORT, &issued.token, Some(&expired)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(imported_expired["disposition"], "created");
    assert!(imported_expired["accounts"].as_array().unwrap().iter().all(
        |account| account["status"] == "active"
            && account["credential_generation"] == 1
            && account["route_count"] == 0
    ));
    let expired_id =
        Uuid::parse_str(imported_expired["accounts"][1]["id"].as_str().unwrap()).unwrap();
    assert!(
        state
            .db
            .list_managed_oauth_refresh_candidates(memeloop_token_center::db::unix_millis(), 100)
            .await
            .unwrap()
            .contains(&(expired_id, 1))
    );

    let missing_expiry_tenant = "native-kimi-missing-expiry";
    let missing_expiry = cohort_request(
        missing_expiry_tenant,
        vec![
            account('6', "auth/6.json", "missing-a", ""),
            account('7', "auth/7.json", "missing-b", ""),
        ],
    );
    assert_eq!(
        call(&state, "POST", COHORT, &issued.token, Some(&missing_expiry),)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );

    let foreign_tenant = "native-kimi-foreign";
    state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: foreign_tenant.into(),
                name: "foreign Kimi".into(),
                driver: "kimi-oauth".into(),
                config: json!({
                    "base_url": "https://api.kimi.com/coding",
                    "network_scope": "public",
                    "reservation_token_bounds": {}
                }),
                credential: UpstreamCredential::OAuth {
                    access_token: "foreign-fixture-access".into(),
                    refresh_token: Some("foreign-fixture-refresh".into()),
                    expires_at: Some(4_070_908_800_000),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: Some(json!({
                        "schema": "kimi-oauth-v1",
                        "device_id": "foreign-device",
                        "scope": "coding",
                        "token_type": "Bearer"
                    })),
                    proxy_url: None,
                    proxy_network_scope: None::<OutboundScope>,
                },
                oauth_session_id: Some(Uuid::now_v7()),
                oauth_driver: Some("kimi-oauth".into()),
                oauth_refresh_url: Some("https://auth.kimi.com/api/oauth/token".into()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let foreign = cohort_request(
        foreign_tenant,
        vec![
            account('c', "auth/c.json", "cohort-c", "2099-01-01T00:00:00Z"),
            account('d', "auth/d.json", "cohort-d", "2099-01-01T00:00:00Z"),
        ],
    );
    assert_eq!(
        call(&state, "POST", COHORT, &issued.token, Some(&foreign))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let foreign_inventory = state
        .db
        .list_upstream_accounts(foreign_tenant)
        .await
        .unwrap();
    assert_eq!(foreign_inventory.len(), 1);
    assert_eq!(foreign_inventory[0].import_source_identity_hash, None);
    assert_eq!(foreign_inventory[0].import_source_document_sha256, None);
}
