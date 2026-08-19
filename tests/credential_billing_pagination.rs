use std::collections::HashSet;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{CreateServiceTokenInput, Database},
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const BOOTSTRAP_TOKEN: &str = "test-service-token";

async fn json_request(
    state: &AppState,
    method: &str,
    path: &str,
    bearer: &str,
    idempotency_key: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    if let Some(idempotency_key) = idempotency_key {
        builder = builder.header("idempotency-key", idempotency_key);
    }
    let body = if let Some(value) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(serde_json::to_vec(value).unwrap())
    } else {
        Body::empty()
    };
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

async fn create_and_drop_response(
    state: &AppState,
    idempotency_key: &str,
    body: &Value,
) -> StatusCode {
    let request = Request::post("/internal/v1/keys")
        .header(header::AUTHORIZATION, format!("Bearer {BOOTSTRAP_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("idempotency-key", idempotency_key)
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    drop(response);
    status
}

async fn create_credential(
    state: &AppState,
    tenant: &str,
    principal: &str,
    alias: &str,
    initial_balance: &str,
    idempotency_key: &str,
) -> Value {
    let body = json!({
        "tenant_external_id": tenant,
        "principal_external_id": principal,
        "alias": alias,
        "currency": "USD",
        "initial_balance": initial_balance,
        "policy": {
            "allowed_models": ["model-a"],
            "requests_per_minute": 60,
            "tokens_per_minute": 100000,
            "max_concurrency": 4,
            "daily_budget": "20",
            "weekly_budget": "80",
            "lifetime_budget": "200"
        }
    });
    let (status, response) = json_request(
        state,
        "POST",
        "/internal/v1/keys",
        BOOTSTRAP_TOKEN,
        Some(idempotency_key),
        Some(&body),
    )
    .await;
    // Never include the one-time secret-bearing response in failure output.
    assert_eq!(status, StatusCode::CREATED);
    response
}

async fn service_token(
    database: &Database,
    pepper: &[u8],
    name: String,
    tenant: Option<String>,
) -> String {
    database
        .create_service_token(
            CreateServiceTokenInput {
                name,
                scopes: vec![
                    "keys:read".into(),
                    "keys:write".into(),
                    "credits:read".into(),
                ],
                tenant_external_id: tenant,
            },
            pepper,
        )
        .await
        .unwrap()
        .token
}

fn ids(rows: &Value, field: &str) -> Vec<Uuid> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|row| Uuid::parse_str(row[field].as_str().unwrap()).unwrap())
        .collect()
}

async fn exercise_credential_and_ledger_acceptance(state: AppState, label: &str) {
    let unique = Uuid::now_v7();
    let tenant_a = format!("cred-page-{label}-a-{unique}");
    let tenant_b = format!("cred-page-{label}-b-{unique}");
    let principal = format!("principal-{unique}");
    let prior_idempotency = format!("cred-page:{unique}:prior");
    let target_idempotency = format!("cred-page:{unique}:lost-target");
    let other_idempotency = format!("cred-page:{unique}:other-tenant");

    let prior = create_credential(
        &state,
        &tenant_a,
        &principal,
        "prior ambiguous credential",
        "1",
        &prior_idempotency,
    )
    .await;
    let target_body = json!({
        "tenant_external_id": tenant_a,
        "principal_external_id": principal,
        "alias": "lost response target",
        "currency": "USD",
        "initial_balance": "7",
        "policy": {
            "allowed_models": ["model-a"],
            "requests_per_minute": 60,
            "tokens_per_minute": 100000,
            "max_concurrency": 4,
            "daily_budget": "20",
            "weekly_budget": "80",
            "lifetime_budget": "200"
        }
    });

    // Simulate the caller observing only a committed status before its response
    // body is lost. We intentionally do not retain or inspect the first secret.
    let lost_status = create_and_drop_response(&state, &target_idempotency, &target_body).await;
    assert_eq!(lost_status, StatusCode::CREATED);

    let other = create_credential(
        &state,
        &tenant_b,
        &principal,
        "other tenant credential",
        "2",
        &other_idempotency,
    )
    .await;

    let global_token = service_token(
        &state.db,
        state.config.key_pepper.as_bytes(),
        format!("global-reader-{unique}"),
        None,
    )
    .await;
    let tenant_a_token = service_token(
        &state.db,
        state.config.key_pepper.as_bytes(),
        format!("tenant-a-reader-{unique}"),
        Some(tenant_a.clone()),
    )
    .await;
    let tenant_b_token = service_token(
        &state.db,
        state.config.key_pepper.as_bytes(),
        format!("tenant-b-reader-{unique}"),
        Some(tenant_b.clone()),
    )
    .await;

    // Principal-only reconciliation is deliberately ambiguous: two records in
    // tenant A exist and neither list row exposes a secret.
    let (status, tenant_rows) = json_request(
        &state,
        "GET",
        &format!("/internal/v1/keys?principal_external_id={principal}&limit=10"),
        &tenant_a_token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tenant_rows.as_array().unwrap().len(), 2);
    assert!(tenant_rows.as_array().unwrap().iter().all(|row| {
        row["tenant_external_id"] == tenant_a
            && row.get("key").is_none()
            && row.get("secret").is_none()
            && row.get("token").is_none()
            && !row.to_string().contains("mtc_")
    }));

    // Replaying the exact request after the timeout resolves the ambiguity
    // without creating a second identity. A changed payload is rejected.
    let (replay_status, replay) = json_request(
        &state,
        "POST",
        "/internal/v1/keys",
        BOOTSTRAP_TOKEN,
        Some(&target_idempotency),
        Some(&target_body),
    )
    .await;
    assert_eq!(replay_status, StatusCode::CREATED);
    let target_key_id = Uuid::parse_str(replay["key_id"].as_str().unwrap()).unwrap();
    let target_account_id = Uuid::parse_str(replay["account_id"].as_str().unwrap()).unwrap();
    let target_fingerprint = replay["fingerprint"].as_str().unwrap();
    let target_row = tenant_rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["key_id"] == target_key_id.to_string())
        .expect("idempotent replay identifies exactly one ambiguous list row");
    assert_eq!(target_row["account_id"], target_account_id.to_string());
    assert_eq!(target_row["fingerprint"], target_fingerprint);
    assert_eq!(target_row["available_balance"], "7");
    assert_eq!(target_row["credential_generation"], 1);

    let mut changed_body = target_body.clone();
    changed_body["alias"] = Value::String("must not create another identity".into());
    let (status, _) = json_request(
        &state,
        "POST",
        "/internal/v1/keys",
        BOOTSTRAP_TOKEN,
        Some(&target_idempotency),
        Some(&changed_body),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Global credentials may page across tenants; scoped credentials remain
    // bound even when an explicit cross-tenant filter is supplied.
    let (status, first_page) = json_request(
        &state,
        "GET",
        &format!("/internal/v1/keys?principal_external_id={principal}&limit=2"),
        &global_token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first_page.as_array().unwrap().len(), 2);
    let cursor_row = first_page.as_array().unwrap().last().unwrap();
    let before_created_at = cursor_row["created_at"].as_i64().unwrap();
    let before_id = cursor_row["key_id"].as_str().unwrap();

    let inserted_after_cursor = create_credential(
        &state,
        &tenant_b,
        &principal,
        "inserted after first page",
        "3",
        &format!("cred-page:{unique}:after-cursor"),
    )
    .await;
    let inserted_after_cursor_id =
        Uuid::parse_str(inserted_after_cursor["key_id"].as_str().unwrap()).unwrap();
    let (status, second_page) = json_request(
        &state,
        "GET",
        &format!(
            "/internal/v1/keys?principal_external_id={principal}&limit=2&before_created_at={before_created_at}&before_id={before_id}"
        ),
        &global_token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let old_expected = HashSet::from([
        Uuid::parse_str(prior["key_id"].as_str().unwrap()).unwrap(),
        target_key_id,
        Uuid::parse_str(other["key_id"].as_str().unwrap()).unwrap(),
    ]);
    let all_old = ids(&first_page, "key_id")
        .into_iter()
        .chain(ids(&second_page, "key_id"))
        .collect::<HashSet<_>>();
    assert_eq!(all_old, old_expected);
    assert!(!all_old.contains(&inserted_after_cursor_id));

    let (status, _) = json_request(
        &state,
        "GET",
        &format!("/internal/v1/keys?tenant_external_id={tenant_b}"),
        &tenant_a_token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, tenant_b_rows) = json_request(
        &state,
        "GET",
        &format!(
            "/internal/v1/keys?tenant_external_id={tenant_b}&principal_external_id={principal}&limit=10"
        ),
        &global_token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tenant_b_rows.as_array().unwrap().len(), 2);

    for invalid in [
        "/internal/v1/keys?limit=0".to_owned(),
        "/internal/v1/keys?limit=501".to_owned(),
        "/internal/v1/keys?before_created_at=1".to_owned(),
        format!("/internal/v1/keys?before_created_at=-1&before_id={target_key_id}"),
    ] {
        let (status, _) = json_request(&state, "GET", &invalid, &global_token, None, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}");
    }

    // Rotation changes only the credential generation and fingerprint. Stable
    // identity, account, policy, quota, and initial ledger remain attached.
    let (status, rotated) = json_request(
        &state,
        "POST",
        &format!("/internal/v1/keys/{target_key_id}/rotate"),
        &tenant_a_token,
        Some(&format!("cred-page:{unique}:rotate")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rotated["key_id"], target_key_id.to_string());
    assert_eq!(rotated["account_id"], target_account_id.to_string());
    assert_eq!(rotated["credential_generation"], 2);
    let (status, after_rotation) = json_request(
        &state,
        "GET",
        &format!("/internal/v1/keys?principal_external_id={principal}&limit=10"),
        &tenant_a_token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rotated_row = after_rotation
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["key_id"] == target_key_id.to_string())
        .unwrap();
    assert_eq!(rotated_row["account_id"], target_account_id.to_string());
    assert_eq!(rotated_row["credential_generation"], 2);
    assert_eq!(rotated_row["available_balance"], "7");
    assert_eq!(rotated_row["policy"], target_row["policy"]);
    assert_ne!(rotated_row["fingerprint"], target_fingerprint);
    assert!(rotated_row.get("key").is_none());

    state
        .db
        .grant(
            target_account_id,
            Decimal::ONE,
            "pagination grant one",
            &format!("cred-page:{unique}:grant-1"),
        )
        .await
        .unwrap();
    state
        .db
        .grant(
            target_account_id,
            Decimal::from(2),
            "pagination grant two",
            &format!("cred-page:{unique}:grant-2"),
        )
        .await
        .unwrap();

    let ledger_path = format!("/internal/v1/accounts/{target_account_id}/ledger?limit=1");
    let (status, first_ledger) =
        json_request(&state, "GET", &ledger_path, &tenant_a_token, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first_ledger.as_array().unwrap().len(), 1);
    let ledger_cursor = &first_ledger[0];

    // A new head entry after page one must not appear behind the old cursor.
    state
        .db
        .grant(
            target_account_id,
            Decimal::from(4),
            "pagination grant after cursor",
            &format!("cred-page:{unique}:grant-after-cursor"),
        )
        .await
        .unwrap();
    let mut ledger_rows = first_ledger.as_array().unwrap().clone();
    let mut cursor_created_at = ledger_cursor["created_at"].as_i64().unwrap();
    let mut cursor_id = ledger_cursor["entry_id"].as_str().unwrap().to_owned();
    loop {
        let (status, page) = json_request(
            &state,
            "GET",
            &format!(
                "/internal/v1/accounts/{target_account_id}/ledger?limit=1&before_created_at={cursor_created_at}&before_id={cursor_id}"
            ),
            &tenant_a_token,
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let Some(row) = page.as_array().unwrap().first() else {
            break;
        };
        ledger_rows.push(row.clone());
        cursor_created_at = row["created_at"].as_i64().unwrap();
        cursor_id = row["entry_id"].as_str().unwrap().to_owned();
    }
    assert_eq!(ledger_rows.len(), 3);
    assert_eq!(
        ledger_rows
            .iter()
            .map(|row| row["entry_id"].as_str().unwrap())
            .collect::<HashSet<_>>()
            .len(),
        3
    );
    assert!(ledger_rows.iter().all(|row| row.get("key").is_none()));

    let other_account_id = Uuid::parse_str(other["account_id"].as_str().unwrap()).unwrap();
    let (status, _) = json_request(
        &state,
        "GET",
        &format!("/internal/v1/accounts/{other_account_id}/ledger"),
        &tenant_a_token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, other_ledger) = json_request(
        &state,
        "GET",
        &format!("/internal/v1/accounts/{other_account_id}/ledger"),
        &tenant_b_token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(other_ledger.as_array().unwrap().len(), 1);
    let missing_account = Uuid::now_v7();
    let (status, _) = json_request(
        &state,
        "GET",
        &format!("/internal/v1/accounts/{missing_account}/ledger"),
        &global_token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    for invalid in [
        format!("/internal/v1/accounts/{target_account_id}/ledger?limit=501"),
        format!("/internal/v1/accounts/{target_account_id}/ledger?before_id={target_key_id}"),
        format!(
            "/internal/v1/accounts/{target_account_id}/ledger?before_created_at=-1&before_id={target_key_id}"
        ),
    ] {
        let (status, _) = json_request(&state, "GET", &invalid, &tenant_a_token, None, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}");
    }
}

#[tokio::test]
async fn sqlite_create_reconciliation_and_paginated_tenant_billing_are_stable() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("credential-pagination.db").display()
    );
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap();
    exercise_credential_and_ledger_acceptance(state, "sqlite").await;
}

#[tokio::test]
async fn postgres_create_reconciliation_and_paginated_tenant_billing_are_stable() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!("MTC_TEST_POSTGRES_URL is unset; skipping PostgreSQL credential/billing E2E");
        return;
    };
    let state = AppState::initialize(Config::for_test(database_url.clone()))
        .await
        .unwrap();
    let pool = sqlx::PgPool::connect(&database_url).await.unwrap();
    let cursor_indexes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_indexes WHERE schemaname = current_schema() AND indexname IN ('key_records_created_cursor_idx', 'key_records_tenant_created_cursor_idx', 'key_records_principal_created_cursor_idx', 'ledger_entries_account_time_idx')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cursor_indexes, 4);
    exercise_credential_and_ledger_acceptance(state, "postgres").await;
}
