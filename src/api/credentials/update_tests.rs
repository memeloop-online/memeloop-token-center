use super::super::*;
use crate::db::CreateServiceTokenInput;
use tower::ServiceExt;

async fn test_state() -> (AppState, tempfile::TempDir) {
    let (state, directory) = crate::api::tests::test_state().await;
    match state.db.create_tenant("default", None).await {
        Ok(_) | Err(AppError::Conflict(_)) => {}
        Err(error) => panic!("cannot prepare default tenant: {error}"),
    }
    (state, directory)
}

async fn request(
    state: &AppState,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn issue(
    state: &AppState,
    name: &str,
    scopes: &[&str],
    tenant: Option<&str>,
) -> crate::model::IssuedServiceToken {
    state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: name.to_owned(),
                scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
                tenant_external_id: tenant.map(str::to_owned),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn update_preserves_secret_and_generation_and_applies_exact_scopes_immediately() {
    let (state, _directory) = test_state().await;
    let issued = issue(&state, "before", &["keys:read"], None).await;
    let path = format!("/internal/v1/service-tokens/{}", issued.service_id);
    let (status, updated) = request(
        &state,
        "PATCH",
        &path,
        &state.config.service_token,
        Some(json!({"name": "  after  ", "scopes": ["requests:read"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["name"], "after");
    assert_eq!(updated["scopes"], json!(["requests:read"]));
    assert_eq!(
        updated["credential_generation"],
        issued.credential_generation
    );
    assert_eq!(updated["fingerprint"], issued.fingerprint);
    assert!(updated.get("token").is_none());
    assert!(updated.get("key").is_none());
    let copied = state
        .db
        .copy_service_token(issued.service_id, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert_eq!(copied.token, issued.token);
    let authenticated = state
        .db
        .authenticate_service_token(&issued.token, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert!(authenticated.allows("requests:read"));
    assert!(!authenticated.allows("keys:read"));
    assert!(!authenticated.allows("service_tokens:write"));
    assert_eq!(
        request(&state, "GET", "/internal/v1/keys", &issued.token, None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn service_update_requires_global_writer_even_when_target_is_self() {
    let (state, _directory) = test_state().await;
    let target = issue(&state, "target", &["requests:read"], None).await;
    let path = format!("/internal/v1/service-tokens/{}", target.service_id);
    for (index, (scopes, tenant)) in [
        (vec!["service_tokens:read"], None),
        (vec!["requests:read"], None),
        (vec!["service_tokens:write"], Some("default")),
    ]
    .into_iter()
    .enumerate()
    {
        let caller = issue(&state, &format!("caller-{index}"), &scopes, tenant).await;
        assert_eq!(
            request(
                &state,
                "PATCH",
                &path,
                &caller.token,
                Some(json!({"name":"denied"}))
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let own_path = format!("/internal/v1/service-tokens/{}", caller.service_id);
        assert_eq!(
            request(
                &state,
                "PATCH",
                &own_path,
                &caller.token,
                Some(json!({"tenant_external_id": null, "scopes":["service_tokens:write"]}))
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let writer = issue(&state, "writer", &["service_tokens:write"], None).await;
    assert_eq!(
        request(
            &state,
            "PATCH",
            &path,
            &writer.token,
            Some(json!({"name":"authorized"}))
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &state,
            "PATCH",
            &path,
            "invalid-token",
            Some(json!({"name":"denied"}))
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn invalid_partial_service_updates_leave_all_fields_unchanged() {
    let (state, _directory) = test_state().await;
    let issued = issue(&state, "original", &["requests:read"], None).await;
    let path = format!("/internal/v1/service-tokens/{}", issued.service_id);
    for body in [
        json!({}),
        json!({"name":"changed", "scopes": []}),
        json!({"name":"changed", "scopes": ["*"]}),
        json!({"name":"changed", "scopes": ["analytics:read"]}),
        json!({"name":"changed", "scopes": ["requests:read", "requests:read"]}),
        json!({"name":" "}),
        json!({"name":"changed", "tenant_external_id":" "}),
        json!({"name":null}),
        json!({"scopes":null}),
        json!({"name":"changed", "alias":"unknown"}),
    ] {
        assert_eq!(
            request(
                &state,
                "PATCH",
                &path,
                &state.config.service_token,
                Some(body)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let values = state.db.list_service_tokens().await.unwrap();
    let stored = values
        .iter()
        .find(|value| value.service_id == issued.service_id)
        .unwrap();
    assert_eq!(stored.name, "original");
    assert_eq!(stored.scopes, vec!["requests:read"]);
    assert_eq!(stored.tenant_external_id, None);
}

#[tokio::test]
async fn missing_tenant_preserves_scope_and_explicit_null_selects_global_scope() {
    let (state, _directory) = test_state().await;
    let issued = issue(&state, "scoped", &["requests:read"], Some("default")).await;
    let path = format!("/internal/v1/service-tokens/{}", issued.service_id);
    for (body, expected_tenant) in [
        (json!({"name":"still scoped"}), json!("default")),
        (json!({"tenant_external_id":null}), Value::Null),
        (json!({"tenant_external_id":" default "}), json!("default")),
    ] {
        let (status, updated) = request(
            &state,
            "PATCH",
            &path,
            &state.config.service_token,
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(updated["tenant_external_id"], expected_tenant);
        assert_eq!(updated["scopes"], json!(["requests:read"]));
    }
}

#[tokio::test]
async fn updates_do_not_resume_suspended_or_revoked_service_credentials() {
    let (state, _directory) = test_state().await;
    let issued = issue(&state, "suspended", &["requests:read"], None).await;
    state
        .db
        .set_service_token_status(issued.service_id, "suspended")
        .await
        .unwrap();
    let path = format!("/internal/v1/service-tokens/{}", issued.service_id);
    let (status, updated) = request(
        &state,
        "PATCH",
        &path,
        &state.config.service_token,
        Some(json!({"name":"edited while suspended", "scopes":["metrics:read"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["status"], "suspended");
    assert_eq!(updated["credential_copy_available"], false);
    assert!(
        state
            .db
            .authenticate_service_token(&issued.token, state.config.key_pepper.as_bytes())
            .await
            .is_err()
    );
    state
        .db
        .set_service_token_status(issued.service_id, "active")
        .await
        .unwrap();
    let authenticated = state
        .db
        .authenticate_service_token(&issued.token, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert!(authenticated.allows("metrics:read"));
    assert!(!authenticated.allows("requests:read"));
    state
        .db
        .set_service_token_status(issued.service_id, "revoked")
        .await
        .unwrap();
    assert_eq!(
        request(
            &state,
            "PATCH",
            &path,
            &state.config.service_token,
            Some(json!({"name":"cannot restore"}))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert!(
        state
            .db
            .copy_service_token(issued.service_id, state.config.key_pepper.as_bytes())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn requests_read_alone_allows_analysis_and_search_but_not_other_tenants_or_catalogs() {
    let (state, _directory) = test_state().await;
    let issued = issue(&state, "analysis only", &["requests:read"], Some("default")).await;
    for path in [
        "/internal/v1/usage-analysis",
        "/internal/v1/usage-analysis/trends",
        "/internal/v1/requests?limit=1",
        "/internal/v1/sessions?limit=1",
    ] {
        assert_eq!(
            request(&state, "GET", path, &issued.token, None).await.0,
            StatusCode::OK,
            "{path}"
        );
    }
    for path in [
        "/internal/v1/usage-analysis?tenant_external_id=other",
        "/internal/v1/usage-analysis/trends?tenant_external_id=other",
        "/internal/v1/requests?tenant_external_id=other&limit=1",
        "/internal/v1/keys",
        "/internal/v1/model-picker-options?tenant_external_id=default&selection_kind=route",
    ] {
        assert_eq!(
            request(&state, "GET", path, &issued.token, None).await.0,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    let metrics = issue(&state, "metrics only", &["metrics:read"], Some("default")).await;
    assert_eq!(
        request(
            &state,
            "GET",
            "/internal/v1/usage-analysis",
            &metrics.token,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn rotated_token_uses_updated_grants_and_old_receipts_do_not_restore_authority() {
    let (state, _directory) = test_state().await;
    let issued = issue(&state, "rotate", &["keys:read"], None).await;
    let rotated = state
        .db
        .rotate_service_token(
            issued.service_id,
            "before-update",
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let path = format!("/internal/v1/service-tokens/{}", issued.service_id);
    let (status, updated) = request(
        &state,
        "PATCH",
        &path,
        &state.config.service_token,
        Some(json!({"scopes":["requests:read"], "tenant_external_id":"default"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["credential_generation"], 2);
    let replay = state
        .db
        .rotate_service_token(
            issued.service_id,
            "before-update",
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(replay.token, rotated.token);
    let authenticated = state
        .db
        .authenticate_service_token(&replay.token, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert!(authenticated.allows("requests:read"));
    assert!(!authenticated.allows("keys:read"));
    assert_eq!(authenticated.tenant_external_id.as_deref(), Some("default"));
    assert!(
        state
            .db
            .authenticate_service_token(&issued.token, state.config.key_pepper.as_bytes())
            .await
            .is_err()
    );
    let next = state
        .db
        .rotate_service_token(
            issued.service_id,
            "after-update",
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(next.scopes, vec!["requests:read"]);
    assert_eq!(next.tenant_external_id.as_deref(), Some("default"));
}

#[tokio::test]
async fn self_analysis_accepts_only_client_identity_and_rejects_management_selectors() {
    let (state, _directory) = test_state().await;
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "default".into(),
                principal_external_id: "self-only".into(),
                alias: "self-only".into(),
                currency: "USD".into(),
                policy: crate::model::KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let service = issue(
        &state,
        "management reader",
        &["requests:read"],
        Some("default"),
    )
    .await;
    for (path, token, expected) in [
        (
            "/self/v1/usage-analysis",
            issued.key.as_str(),
            StatusCode::OK,
        ),
        (
            "/self/v1/usage-analysis?tenant_external_id=other",
            issued.key.as_str(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/self/v1/usage-analysis?key_id=00000000-0000-0000-0000-000000000001",
            issued.key.as_str(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/self/v1/usage-analysis",
            service.token.as_str(),
            StatusCode::UNAUTHORIZED,
        ),
    ] {
        let response = router_for_role(state.clone(), RuntimeRole::Gateway)
            .oneshot(
                Request::get(path)
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path}");
    }
    assert_eq!(
        request(
            &state,
            "GET",
            "/internal/v1/usage-analysis",
            &issued.key,
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let path = format!("/internal/v1/service-tokens/{}", service.service_id);
    let response = router_for_role(state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::patch(path)
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", state.config.service_token),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"gateway cannot manage"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn postgres_waiting_name_update_preserves_current_scope_and_tenant() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let state = AppState::initialize(crate::config::Config::for_test(database_url.clone()))
        .await
        .unwrap();
    let issued = issue(&state, "before lock", &["keys:read"], None).await;
    let pool = sqlx::PgPool::connect(&database_url).await.unwrap();
    let mut transaction = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM service_principals WHERE id = $1 FOR UPDATE")
        .bind(issued.service_id.to_string())
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
    let database = state.db.clone();
    let service_id = issued.service_id;
    let waiting = tokio::spawn(async move {
        database
            .update_service_token(service_id, Some("after lock"), None, None)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE 'SELECT name, status, credential_generation, created_at FROM service_principals WHERE id = $1 FOR UPDATE%')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if blocked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("name update must wait for the locked principal");
    sqlx::query("UPDATE service_credentials SET scopes_json = '[\"requests:read\"]', tenant_external_id = 'default' WHERE service_principal_id = $1 AND generation = 1")
        .bind(service_id.to_string())
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query("UPDATE service_principals SET name = 'changed while locked' WHERE id = $1")
        .bind(service_id.to_string())
        .execute(&mut *transaction)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    let updated = waiting.await.unwrap().unwrap();
    assert_eq!(updated.name, "after lock");
    assert_eq!(updated.scopes, vec!["requests:read"]);
    assert_eq!(updated.tenant_external_id.as_deref(), Some("default"));
    let stored = state.db.list_service_tokens().await.unwrap();
    let stored = stored
        .iter()
        .find(|value| value.service_id == service_id)
        .unwrap();
    assert_eq!(stored.scopes, updated.scopes);
    assert_eq!(stored.tenant_external_id, updated.tenant_external_id);
    pool.close().await;
}
