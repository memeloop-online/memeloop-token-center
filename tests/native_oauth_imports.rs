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
    let response = String::from_utf8(bytes).unwrap();
    for forbidden in [
        "auth/second.json",
        "auth/first.json",
        "fixture-second",
        "fixture-first",
    ] {
        assert!(!response.contains(forbidden));
    }
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
    assert_eq!(
        call(&state, "POST", COHORT, &issued.token, Some(&expired))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert!(
        state
            .db
            .list_upstream_accounts(expired_tenant)
            .await
            .unwrap()
            .is_empty()
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
