//! Real guest, real control routes and worker, durable DB, loopback supplier only.
use super::*;
use crate::{
    config::{Config, RuntimeRole},
    db::{CreateServiceTokenInput, CreateUpstreamAccountInput},
    provider::{UpstreamAccountView, UpstreamCredential},
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU16, Ordering},
};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const PLUGIN: &str = "mtc-codex-expiring-reset";

fn credits(now: i64, ahead: i64, count: i64) -> Value {
    let timestamp = |time| {
        chrono::DateTime::from_timestamp_millis(time)
            .unwrap()
            .to_rfc3339()
    };
    let mut entries = vec![
        json!({"id":"target-credit","reset_type":"codex_rate_limits","status":"available","granted_at":timestamp(now-3_600_000),"expires_at":timestamp(now+ahead)}),
    ];
    if count > 1 {
        entries.push(json!({"id":"later-credit","reset_type":"codex_rate_limits","status":"available","granted_at":timestamp(now-3_600_000),"expires_at":timestamp(now+3_600_000)}));
    }
    json!({"available_count":count,"credits":entries})
}

async fn account(state: &AppState, supplier: &MockServer, tenant: &str) -> UpstreamAccountView {
    state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.into(),
                name: format!("fixture-{}", Uuid::now_v7()),
                driver: "openai-codex".into(),
                config: json!({"base_url":"https://chatgpt.com","reset_test_supplier_url":format!("{}/",supplier.uri())}),
                credential: UpstreamCredential::OAuth {
                    access_token: "synthetic-only".into(),
                    refresh_token: None,
                    expires_at: None,
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    proxy_url: None,
                    proxy_network_scope: None,
                    adapter_state: Some(
                        json!({"schema":"openai-codex-oauth-v1","account_id":"synthetic-account"}),
                    ),
                },
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap()
}

async fn request(
    state: &AppState,
    account: Uuid,
    tenant: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let path = format!(
        "/internal/v1/plugins/{PLUGIN}/background-actions/{account}?tenant_external_id={tenant}"
    );
    let builder = if body.is_some() {
        Request::put(path)
    } else {
        Request::get(path)
    };
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            builder
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    body.map(|body| body.to_string()).unwrap_or_default(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let value = serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
        .unwrap();
    (status, value)
}

async fn due(pool: &sqlx::AnyPool) {
    sqlx::query("UPDATE plugin_background_action_grants SET next_attempt_at=0,lease_until=0,lease_owner=NULL").execute(pool).await.unwrap();
}

async fn post_count(supplier: &MockServer) -> usize {
    supplier
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method == "POST")
        .count()
}

async fn contract(database_url: String, directory: &tempfile::TempDir) {
    let source = PathBuf::from(
        std::env::var("MTC_CODEX_EXPIRING_RESET_PACKAGE").expect("real CI guest package required"),
    );
    let root = directory.path().join("plugins");
    let package = root.join(PLUGIN);
    std::fs::create_dir_all(package.join("ui")).unwrap();
    for name in ["plugin.json", "plugin.wasm", "ui/codex-expiring-reset.mjs"] {
        std::fs::copy(source.join(name), package.join(name)).unwrap();
    }
    let mut config = Config::for_test(database_url.clone());
    config.plugin_dir = Some(root.to_str().unwrap().into());
    let state = AppState::initialize(config.clone()).await.unwrap();
    let pool = sqlx::any::AnyPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    let supplier = MockServer::start().await;
    let now = crate::db::unix_millis();
    let inventory = Arc::new(Mutex::new(credits(now, 1_200_000, 1)));
    let status = Arc::new(AtomicU16::new(503));
    let response_inventory = inventory.clone();
    Mock::given(method("GET"))
        .and(path("/credits"))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(response_inventory.lock().unwrap().clone())
        })
        .mount(&supplier)
        .await;
    let response_status = status.clone();
    Mock::given(method("POST"))
        .and(path("/consume"))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(response_status.load(Ordering::SeqCst))
        })
        .mount(&supplier)
        .await;
    let tenant = format!("real-background-{}", Uuid::now_v7());
    let account = account(&state, &supplier, &tenant).await;
    let issued = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: tenant.clone(),
                tenant_external_id: Some(tenant.clone()),
                scopes: vec![
                    "plugins:read".into(),
                    "plugins:write".into(),
                    "providers:read".into(),
                    "providers:write".into(),
                ],
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let readonly = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: format!("{tenant}-readonly"),
                tenant_external_id: Some(tenant.clone()),
                scopes: vec!["plugins:read".into(), "providers:read".into()],
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let (_, initial) = request(&state, account.id, &tenant, &readonly.token, None).await;
    assert_eq!(initial["can_write"], false);
    assert!(initial["grant"].is_null());
    assert_eq!(
        request(
            &state,
            account.id,
            &tenant,
            &readonly.token,
            Some(json!({"enabled":true,"expected_revision":0}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&state, account.id, "wrong-tenant", &issued.token, None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (_stop, shutdown) = watch::channel(false);
    let blocking = BlockingTasks::new();
    run_plugin_background_actions(&state, &shutdown, &blocking).await;
    assert_eq!(
        post_count(&supplier).await,
        0,
        "installation alone is not authority"
    );
    let (code, enabled) = request(
        &state,
        account.id,
        &tenant,
        &issued.token,
        Some(json!({"enabled":true,"expected_revision":0})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(enabled["grant"]["revision"], 1);
    assert_eq!(
        request(
            &state,
            account.id,
            &tenant,
            &issued.token,
            Some(json!({"enabled":true,"expected_revision":0}))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    run_plugin_background_actions(&state, &shutdown, &blocking).await;
    assert_eq!(
        post_count(&supplier).await,
        0,
        "no reset before guest ten-minute window"
    );
    let grant = state
        .db
        .background_action_grant(PLUGIN, &tenant, &account.id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(grant.last_result.as_deref(), Some("no_action"));
    *inventory.lock().unwrap() = credits(now, 500_000, 1);
    due(&pool).await;
    tokio::join!(
        run_plugin_background_actions(&state, &shutdown, &blocking),
        run_plugin_background_actions(&state, &shutdown, &blocking)
    );
    assert_eq!(
        post_count(&supplier).await,
        1,
        "competing Pods dispatch once"
    );
    let (_, unknown) = request(&state, account.id, &tenant, &issued.token, None).await;
    assert_eq!(unknown["operation"]["state"], "unknown");
    assert_eq!(unknown["grant"]["last_result"], "unknown");
    let unknown_id = unknown["operation"]["id"].clone();
    for (revision, enabled) in [(1, false), (2, true)] {
        assert_eq!(
            request(
                &state,
                account.id,
                &tenant,
                &issued.token,
                Some(json!({"enabled":enabled,"expected_revision":revision}))
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    // Fresh runtime / new Pod sees the same durable unknown business identity.
    let restarted = AppState::initialize(config.clone()).await.unwrap();
    due(&pool).await;
    run_plugin_background_actions(&restarted, &shutdown, &blocking).await;
    assert_eq!(
        post_count(&supplier).await,
        1,
        "unknown never re-POSTs after restart/regrant"
    );
    let (_, replayed) = request(&restarted, account.id, &tenant, &issued.token, None).await;
    assert_eq!(replayed["operation"]["id"], unknown_id);
    // A different still-valid credit cannot bypass the unresolved account lock.
    inventory.lock().unwrap()["credits"][0]["id"] = json!("replacement-credit");
    due(&pool).await;
    run_plugin_background_actions(&restarted, &shutdown, &blocking).await;
    assert_eq!(post_count(&supplier).await, 1);

    status.store(200, Ordering::SeqCst);
    *inventory.lock().unwrap() = credits(now, 500_000, 1);
    let accepted_account = self::account(&restarted, &supplier, &tenant).await;
    assert_eq!(
        request(
            &restarted,
            accepted_account.id,
            &tenant,
            &issued.token,
            Some(json!({"enabled":true,"expected_revision":0}))
        )
        .await
        .0,
        StatusCode::OK
    );
    run_plugin_background_actions(&restarted, &shutdown, &blocking).await;
    assert_eq!(post_count(&supplier).await, 2);
    let (_, accepted) = request(
        &restarted,
        accepted_account.id,
        &tenant,
        &issued.token,
        None,
    )
    .await;
    assert_eq!(accepted["operation"]["state"], "accepted");
    due(&pool).await;
    run_plugin_background_actions(&restarted, &shutdown, &blocking).await;
    assert_eq!(
        post_count(&supplier).await,
        2,
        "accepted replay does not consume again"
    );

    // Recover both expired and still-prepared inventory changes through the
    // real planner -> preparation -> transport -> claim -> supplier path.
    for expired in [true, false] {
        *inventory.lock().unwrap() = credits(now, 500_000, 1);
        let recovering = self::account(&restarted, &supplier, &tenant).await;
        let (_, credential) = restarted
            .db
            .upstream_account_with_credential(recovering.id, restarted.config.key_pepper.as_bytes())
            .await
            .unwrap();
        let observed = crate::upstream_quota::reset::background_credit_inventory(
            &restarted,
            &recovering,
            &credential,
            &tenant,
        )
        .await
        .unwrap();
        let key = format!(
            "background-reset-credit-v1:{}:{}",
            recovering.id, observed.credits[0].fingerprint
        );
        let prepared = crate::upstream_quota::reset::prepare(
            &restarted,
            &recovering,
            &credential,
            &tenant,
            &format!("plugin:{PLUGIN}"),
            &key,
        )
        .await
        .unwrap();
        // Access durable operation by existing tenant-safe read (token stays opaque).
        let old = restarted
            .db
            .current_quota_reset_operation_for_tenant(&tenant, &recovering.id.to_string())
            .await
            .unwrap()
            .unwrap();
        drop(prepared);
        if expired {
            sqlx::query("UPDATE upstream_quota_reset_operations SET expires_at=$1 WHERE id=$2")
                .bind(now - 120_001)
                .bind(&old.id)
                .execute(&pool)
                .await
                .unwrap();
        }
        *inventory.lock().unwrap() = credits(now, 500_000, 2);
        assert_eq!(
            request(
                &restarted,
                recovering.id,
                &tenant,
                &issued.token,
                Some(json!({"enabled":true,"expected_revision":0}))
            )
            .await
            .0,
            StatusCode::OK
        );
        due(&pool).await;
        run_plugin_background_actions(&restarted, &shutdown, &blocking).await;
        let (_, recovered) = request(&restarted, recovering.id, &tenant, &issued.token, None).await;
        assert_eq!(recovered["operation"]["id"], old.id);
        assert_eq!(recovered["operation"]["state"], "accepted");
        assert_eq!(recovered["operation"]["available_credits"], 2);
    }
    assert_eq!(
        post_count(&supplier).await,
        4,
        "each authorized business operation has one POST"
    );
    for request in supplier
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method == "POST")
    {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body.as_object().unwrap().len(), 1);
        assert!(body["redeem_request_id"].is_string());
        assert_eq!(
            request.headers.get("chatgpt-account-id").unwrap(),
            "synthetic-account"
        );
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires MTC_CODEX_EXPIRING_RESET_PACKAGE built in CI"]
async fn sqlite_codex_background_action_real_component_worker_contract() {
    let directory = tempfile::tempdir().unwrap();
    contract(
        format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("real-background.db").display()
        ),
        &directory,
    )
    .await;
}

#[tokio::test]
#[ignore = "requires MTC_CODEX_EXPIRING_RESET_PACKAGE and PostgreSQL in CI"]
async fn postgres_codex_background_action_real_component_worker_contract() {
    let url = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL fixture required");
    let fixture = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    let directory = tempfile::tempdir().unwrap();
    contract(fixture.url().into(), &directory).await;
    fixture.cleanup().await;
}
