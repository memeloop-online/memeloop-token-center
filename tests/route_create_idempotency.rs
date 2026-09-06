use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
};
use futures_util::future::join_all;
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{
        CreateGroupInput, CreateServiceTokenInput, CreateUpstreamAccountInput, GroupKind,
        UpdateGroupInput, UpdateRoutedModelRouteInput,
    },
    error::AppError,
    provider::UpstreamCredential,
};
use serde_json::{Value, json};
use sqlx::any::AnyPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

struct RouteCreateFixture {
    state: AppState,
    tenant: String,
    upstream_id: Uuid,
    secondary_upstream_id: Uuid,
    write_token: String,
    read_token: String,
}

async fn route_create_fixture(database_url: String, tenant: String) -> RouteCreateFixture {
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .expect("route create idempotency state");
    let pepper = state.config.key_pepper.as_bytes();
    let upstream = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "route create idempotency upstream".to_owned(),
                driver: "http-json".to_owned(),
                config: json!({
                    "base_url": "http://127.0.0.1:18081",
                    "network_scope": "private"
                }),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            pepper,
        )
        .await
        .expect("route create idempotency upstream");
    let secondary_upstream = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "route create idempotency secondary upstream".to_owned(),
                driver: "http-json".to_owned(),
                config: json!({
                    "base_url": "http://127.0.0.1:18082",
                    "network_scope": "private"
                }),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            pepper,
        )
        .await
        .expect("route create idempotency secondary upstream");
    let write = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: format!("route create writer {tenant}"),
                scopes: vec!["routes:write".to_owned(), "routes:read".to_owned()],
                tenant_external_id: Some(tenant.clone()),
            },
            pepper,
        )
        .await
        .expect("route create writer token");
    let read = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: format!("route create reader {tenant}"),
                scopes: vec!["routes:read".to_owned()],
                tenant_external_id: Some(tenant.clone()),
            },
            pepper,
        )
        .await
        .expect("route create reader token");
    RouteCreateFixture {
        state,
        tenant,
        upstream_id: upstream.id,
        secondary_upstream_id: secondary_upstream.id,
        write_token: write.token,
        read_token: read.token,
    }
}

async fn request_json(
    state: AppState,
    token: &str,
    body: Value,
    idempotency_key: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    let idempotency_keys = idempotency_key.into_iter().collect::<Vec<_>>();
    request_json_with_idempotency_keys(state, token, body, &idempotency_keys).await
}

async fn request_json_with_idempotency_keys(
    state: AppState,
    token: &str,
    body: Value,
    idempotency_keys: &[&str],
) -> (StatusCode, HeaderMap, Value) {
    let idempotency_values = idempotency_keys
        .iter()
        .map(|idempotency_key| {
            HeaderValue::from_str(idempotency_key).expect("valid test idempotency key")
        })
        .collect();
    request_json_with_idempotency_values(state, token, body, idempotency_values).await
}

async fn request_json_with_idempotency_values(
    state: AppState,
    token: &str,
    body: Value,
    idempotency_values: Vec<HeaderValue>,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::post("/internal/v1/model-routes")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&body).expect("route create JSON"),
        ))
        .expect("route create request");
    for idempotency_value in idempotency_values {
        request
            .headers_mut()
            .append("idempotency-key", idempotency_value);
    }
    let response = api::router_for_role(state, RuntimeRole::Control)
        .oneshot(request)
        .await
        .expect("route create response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("bounded route create response");
    let body = serde_json::from_slice(&bytes).expect("route create JSON response");
    (status, headers, body)
}

async fn patch_route_enabled(
    state: AppState,
    token: &str,
    route_id: Uuid,
    tenant_external_id: &str,
    enabled: bool,
    expected_updated_at: i64,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("PATCH")
        .uri(format!("/internal/v1/model-routes/{route_id}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "tenant_external_id": tenant_external_id,
                "enabled": enabled,
                "expected_updated_at": expected_updated_at,
            }))
            .expect("route status JSON"),
        ))
        .expect("route status request");
    let response = api::router_for_role(state, RuntimeRole::Control)
        .oneshot(request)
        .await
        .expect("route status response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("bounded route status response");
    let body = serde_json::from_slice(&bytes).expect("route status JSON response");
    (status, body)
}

fn create_body(fixture: &RouteCreateFixture, public_model: &str) -> Value {
    json!({
        "tenant_external_id": fixture.tenant,
        "public_model": public_model,
        "upstream_account_ids": [fixture.upstream_id],
        "upstream_model": "unlisted-route-create-idempotency-model",
        "protocol": "openai",
        "priority": 0,
        "custom_model_confirmed": true
    })
}

fn disposition(headers: &HeaderMap) -> &str {
    headers
        .get("x-mtc-route-create-disposition")
        .and_then(|value| value.to_str().ok())
        .expect("route create disposition header")
}

async fn exercise_route_create_idempotency(database_url: String, tenant: String) {
    let fixture = route_create_fixture(database_url.clone(), tenant).await;
    let headerless = create_body(&fixture, "route-create-headerless");

    let (status, headers, legacy_route) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        headerless.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(disposition(&headers), "created");
    assert_eq!(legacy_route["enabled"].as_bool(), Some(true));
    let legacy_id = legacy_route["id"].clone();

    // An explicit key may never silently adopt a semantically equal route
    // created before that key claim existed. Canonicalization must make
    // harmless whitespace, list order, and list duplicates stable too.
    let owned_body = json!({
        "tenant_external_id": fixture.tenant,
        "public_model": " route-create-owned ",
        "upstream_account_ids": [fixture.secondary_upstream_id, fixture.upstream_id, fixture.upstream_id],
        "upstream_model": " unlisted-route-create-idempotency-model ",
        "protocol": "openai",
        "priority": 0,
        "route_group_names": ["Route Create Owned Group", " route create owned group "],
        "custom_model_confirmed": true
    });
    let operation_key = "route-create-idempotency:owned";
    let (status, headers, owned_route) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        owned_body.clone(),
        Some(operation_key),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(disposition(&headers), "created");
    assert_ne!(owned_route["id"], legacy_id);

    let mut normalized_replay = owned_body.clone();
    normalized_replay["public_model"] = json!("route-create-owned");
    normalized_replay["upstream_account_ids"] =
        json!([fixture.upstream_id, fixture.secondary_upstream_id]);
    normalized_replay["upstream_model"] = json!("unlisted-route-create-idempotency-model");
    normalized_replay["route_group_names"] = json!(["ROUTE CREATE OWNED GROUP"]);
    let (status, headers, replay) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        normalized_replay.clone(),
        Some(operation_key),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(disposition(&headers), "reused");
    assert_eq!(replay["id"], owned_route["id"]);

    let mut conflicting = normalized_replay;
    conflicting["priority"] = json!(1);
    let (status, _, _) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        conflicting,
        Some(operation_key),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let mut disabled_body = create_body(&fixture, "route-create-disabled");
    disabled_body["enabled"] = json!(false);
    let disabled_key = "route-create-idempotency:disabled";
    let (status, headers, disabled_route) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        disabled_body.clone(),
        Some(disabled_key),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(disposition(&headers), "created");
    assert_eq!(disabled_route["enabled"].as_bool(), Some(false));
    let (status, headers, disabled_replay) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        disabled_body.clone(),
        Some(disabled_key),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(disposition(&headers), "reused");
    assert_eq!(disabled_replay["id"], disabled_route["id"]);
    assert_eq!(disabled_replay["enabled"].as_bool(), Some(false));
    disabled_body["enabled"] = json!(true);
    let (status, _, _) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        disabled_body,
        Some(disabled_key),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let empty_provider_group = fixture
        .state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: fixture.tenant.clone(),
                name: "route-create-empty-provider-group".to_owned(),
            },
        )
        .await
        .expect("empty provider group");
    let no_candidate_key = "route-create-idempotency:no-candidate-disabled";
    let mut no_candidate_body = json!({
        "tenant_external_id": fixture.tenant,
        "public_model": "route-create-no-candidate",
        "included_provider_group_ids": [empty_provider_group.id],
        "upstream_model": "route-create-no-candidate-upstream",
        "protocol": "openai",
        "priority": 0,
        "enabled": false,
        "custom_model_confirmed": true
    });
    let (status, headers, no_candidate_disabled) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        no_candidate_body.clone(),
        Some(no_candidate_key),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(disposition(&headers), "created");
    assert_eq!(no_candidate_disabled["enabled"].as_bool(), Some(false));
    let no_candidate_route_id = Uuid::parse_str(
        no_candidate_disabled["id"]
            .as_str()
            .expect("no-candidate route ID"),
    )
    .expect("valid no-candidate route UUID");
    let no_candidate_updated_at = no_candidate_disabled["updated_at"]
        .as_i64()
        .expect("no-candidate route version");

    // A distinct operation with the otherwise identical enabled route still
    // rejects before traffic could select a route without a viable upstream.
    no_candidate_body["enabled"] = json!(true);
    let (status, _, _) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        no_candidate_body,
        Some("route-create-idempotency:no-candidate-enabled"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = patch_route_enabled(
        fixture.state.clone(),
        &fixture.write_token,
        no_candidate_route_id,
        &fixture.tenant,
        true,
        no_candidate_updated_at,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let persisted_no_candidate = fixture
        .state
        .db
        .list_model_routes(Some(&fixture.tenant))
        .await
        .expect("list staged routes")
        .into_iter()
        .find(|route| route.id == no_candidate_route_id)
        .expect("staged no-candidate route remains");
    assert!(!persisted_no_candidate.enabled);
    assert_eq!(persisted_no_candidate.updated_at, no_candidate_updated_at);

    let disabled_route_id = Uuid::parse_str(
        disabled_route["id"]
            .as_str()
            .expect("eligible disabled route ID"),
    )
    .expect("valid eligible disabled route UUID");
    let (status, enabled_route) = patch_route_enabled(
        fixture.state.clone(),
        &fixture.write_token,
        disabled_route_id,
        &fixture.tenant,
        true,
        disabled_route["updated_at"]
            .as_i64()
            .expect("eligible disabled route version"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(enabled_route["id"], disabled_route["id"]);
    assert_eq!(enabled_route["enabled"].as_bool(), Some(true));

    // Expiry is a hard boundary even if a bounded global cleanup has a large
    // backlog: the exact key is removed before lookup/claim.
    let raw_pool = AnyPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("read route create claim storage");
    let expiry_key = "route-create-idempotency:expiry";
    let expiry_body = create_body(&fixture, "route-create-expiry");
    let (status, headers, first_expiry_route) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        expiry_body.clone(),
        Some(expiry_key),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(disposition(&headers), "created");
    let (expiry_key_hash, _) = memeloop_token_center::crypto::hash_credential(
        expiry_key,
        fixture.state.config.key_pepper.as_bytes(),
    );
    sqlx::query(
        "UPDATE model_route_create_operations \
         SET created_at = 0, expires_at = 1 \
         WHERE tenant_id = (SELECT id FROM tenants WHERE external_id = $1) \
           AND idempotency_key_hash = $2",
    )
    .bind(&fixture.tenant)
    .bind(expiry_key_hash)
    .execute(&raw_pool)
    .await
    .expect("expire route create claim");
    let (status, headers, expired_key_create) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        expiry_body,
        Some(expiry_key),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(disposition(&headers), "created");
    assert_ne!(expired_key_create["id"], first_expiry_route["id"]);

    let (status, headers, unowned) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        headerless,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(disposition(&headers), "equivalent_unowned");
    assert_eq!(unowned["id"], legacy_id);

    let owned_route_id = Uuid::parse_str(owned_route["id"].as_str().expect("owned route ID"))
        .expect("valid owned route UUID");
    let owned_route_updated_at = owned_route["updated_at"]
        .as_i64()
        .expect("owned route update version");
    let owned_group_id = Uuid::parse_str(
        owned_route["route_group_ids"][0]
            .as_str()
            .expect("owned route group ID"),
    )
    .expect("valid owned route group UUID");
    let owned_group_id_text = owned_group_id.to_string();
    let owned_group = fixture
        .state
        .db
        .list_groups(GroupKind::Route, &fixture.tenant)
        .await
        .expect("list route groups")
        .into_iter()
        .find(|group| group.id == owned_group_id)
        .expect("owned route group");
    fixture
        .state
        .db
        .update_group(
            GroupKind::Route,
            owned_group_id,
            UpdateGroupInput {
                tenant_external_id: fixture.tenant.clone(),
                name: "route-create-owned-group-renamed".to_owned(),
                expected_updated_at: owned_group.updated_at,
            },
        )
        .await
        .expect("rename owned route group");
    let (status, headers, renamed_group_replay) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        owned_body.clone(),
        Some(operation_key),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(disposition(&headers), "reused");
    assert_eq!(renamed_group_replay["id"], owned_route["id"]);
    assert_eq!(
        renamed_group_replay["route_group_ids"][0].as_str(),
        Some(owned_group_id_text.as_str())
    );

    let updated_route = fixture
        .state
        .db
        .update_routed_model_route(
            owned_route_id,
            UpdateRoutedModelRouteInput {
                tenant_external_id: fixture.tenant.clone(),
                public_model: "route-create-owned-after-update".to_owned(),
                upstream_model: "unlisted-route-create-idempotency-model".to_owned(),
                protocol: "openai".to_owned(),
                priority: 1,
                upstream_account_ids: vec![fixture.upstream_id, fixture.secondary_upstream_id],
                included_provider_group_ids: Vec::new(),
                excluded_provider_group_ids: Vec::new(),
                route_group_ids: vec![owned_group_id],
                route_group_names: Vec::new(),
                granted_credential_ids: Vec::new(),
                expected_updated_at: owned_route_updated_at,
                expected_grant_revision: owned_route["grant_revision"]
                    .as_i64()
                    .expect("owned route grant revision"),
                custom_model_confirmed: true,
            },
        )
        .await
        .expect("update owned route");
    let (status, headers, updated_route_replay) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        owned_body.clone(),
        Some(operation_key),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(disposition(&headers), "reused");
    assert_eq!(updated_route_replay["id"], owned_route["id"]);
    assert_eq!(
        updated_route_replay["public_model"].as_str(),
        Some("route-create-owned-after-update")
    );

    let legacy_upstream_id = updated_route.0.upstream_account_id;
    let removable_upstream = fixture
        .state
        .db
        .list_upstream_accounts(&fixture.tenant)
        .await
        .expect("list owned route upstreams")
        .into_iter()
        .find(|upstream| {
            upstream.id != legacy_upstream_id
                && (upstream.id == fixture.upstream_id
                    || upstream.id == fixture.secondary_upstream_id)
        })
        .expect("non-compatibility owned route upstream");
    let disabled_upstream = fixture
        .state
        .db
        .set_upstream_account_status(
            removable_upstream.id,
            &fixture.tenant,
            "disabled",
            removable_upstream.updated_at,
        )
        .await
        .expect("disable removable owned route upstream");
    fixture
        .state
        .db
        .delete_upstream_account(
            removable_upstream.id,
            &fixture.tenant,
            disabled_upstream.updated_at,
        )
        .await
        .expect("delete removable owned route upstream");
    let (status, headers, deleted_upstream_replay) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        owned_body.clone(),
        Some(operation_key),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(disposition(&headers), "reused");
    assert_eq!(deleted_upstream_replay["id"], owned_route["id"]);

    let disabled_owned_route = fixture
        .state
        .db
        .set_model_route_enabled(
            owned_route_id,
            &fixture.tenant,
            false,
            updated_route.0.updated_at,
        )
        .await
        .expect("disable owned route");
    assert!(matches!(
        fixture
            .state
            .db
            .delete_model_route(
                owned_route_id,
                &fixture.tenant,
                disabled_owned_route.updated_at
            )
            .await,
        Err(AppError::Conflict(message)) if message.contains("active create idempotency claim")
    ));

    let forbidden_body = create_body(&fixture, "route-create-permission-fence");
    let (status, _, _) = request_json(
        fixture.state.clone(),
        &fixture.read_token,
        forbidden_body.clone(),
        Some("route-create-idempotency:permission-fence"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, headers, _) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        forbidden_body,
        Some("route-create-idempotency:permission-fence"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(disposition(&headers), "created");

    let key_syntax_body = create_body(&fixture, "route-create-key-syntax");
    let (status, _, _) = request_json_with_idempotency_keys(
        fixture.state.clone(),
        &fixture.write_token,
        key_syntax_body.clone(),
        &[
            "route-create-idempotency:duplicate-a",
            "route-create-idempotency:duplicate-b",
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    for invalid_value in [
        HeaderValue::from_static(""),
        HeaderValue::from_bytes(b" ").expect("whitespace test header"),
        HeaderValue::from_bytes(&[0xFF]).expect("non-ASCII test header"),
    ] {
        let (status, _, _) = request_json_with_idempotency_values(
            fixture.state.clone(),
            &fixture.write_token,
            key_syntax_body.clone(),
            vec![invalid_value],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let maximum_length_key = "x".repeat(200);
    let (status, headers, _) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        key_syntax_body.clone(),
        Some(&maximum_length_key),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(disposition(&headers), "created");
    let oversized_key = "x".repeat(201);
    let (status, _, _) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        key_syntax_body,
        Some(&oversized_key),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let concurrent_body = create_body(&fixture, "route-create-concurrent");
    let concurrent_key = "route-create-idempotency:concurrent";
    let responses = join_all((0..16).map(|_| {
        request_json(
            fixture.state.clone(),
            &fixture.write_token,
            concurrent_body.clone(),
            Some(concurrent_key),
        )
    }))
    .await;
    let created = responses
        .iter()
        .filter(|(status, headers, _)| {
            *status == StatusCode::CREATED && disposition(headers) == "created"
        })
        .count();
    let reused = responses
        .iter()
        .filter(|(status, headers, _)| {
            *status == StatusCode::OK && disposition(headers) == "reused"
        })
        .count();
    assert_eq!(created, 1, "exactly one concurrent request owns creation");
    assert_eq!(reused, 15, "all remaining concurrent requests must replay");
    let route_ids = responses
        .iter()
        .map(|(_, _, body)| body["id"].as_str().expect("route ID"))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        route_ids.len(),
        1,
        "all replays must return the same route ID"
    );

    let (operation_key_hash, _) = memeloop_token_center::crypto::hash_credential(
        operation_key,
        fixture.state.config.key_pepper.as_bytes(),
    );
    let stored_key_hash: Vec<u8> = sqlx::query_scalar(
        "SELECT idempotency_key_hash FROM model_route_create_operations \
         WHERE tenant_id = (SELECT id FROM tenants WHERE external_id = $1) \
           AND idempotency_key_hash = $2",
    )
    .bind(&fixture.tenant)
    .bind(operation_key_hash.clone())
    .fetch_one(&raw_pool)
    .await
    .expect("stored route create claim");
    assert_eq!(
        stored_key_hash.len(),
        32,
        "claim stores a fixed HMAC digest"
    );
    assert_eq!(stored_key_hash, operation_key_hash);

    let other_fixture =
        route_create_fixture(database_url, format!("{}-other", fixture.tenant)).await;
    let other_state = other_fixture.state.clone();
    let other_body = create_body(&other_fixture, "route-create-cross-tenant");
    let (status, headers, _) = request_json(
        other_state,
        &other_fixture.write_token,
        other_body,
        Some(operation_key),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(disposition(&headers), "created");

    // An operator can also remove a provider implementation while preserving
    // account rows. This directly simulates that stale driver reference: a
    // normal first create would fail provider validation, while this exact
    // in-window retry must resolve its owned route before that mutable check.
    sqlx::query("UPDATE upstream_accounts SET driver = $1 WHERE id = $2")
        .bind("route-create-idempotency-provider-removed")
        .bind(legacy_upstream_id.to_string())
        .execute(&raw_pool)
        .await
        .expect("simulate removed provider driver");
    let (status, headers, removed_driver_replay) = request_json(
        fixture.state.clone(),
        &fixture.write_token,
        owned_body,
        Some(operation_key),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(disposition(&headers), "reused");
    assert_eq!(removed_driver_replay["id"], owned_route["id"]);
}

#[tokio::test]
async fn sqlite_route_create_idempotency_is_owned_and_concurrent_safe() {
    let directory = tempfile::tempdir().expect("route create idempotency directory");
    exercise_route_create_idempotency(
        format!(
            "sqlite://{}?mode=rwc",
            directory
                .path()
                .join("route-create-idempotency.db")
                .display()
        ),
        "route-create-idempotency-sqlite".to_owned(),
    )
    .await;
}

#[tokio::test]
async fn postgres_route_create_idempotency_is_owned_and_concurrent_safe_when_configured() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    exercise_route_create_idempotency(
        database_url,
        format!("route-create-idempotency-postgres-{}", Uuid::now_v7()),
    )
    .await;
}

#[test]
fn openapi_documents_owned_route_create_replay_contract() {
    let contract = include_str!("../openapi/openapi.yaml");
    let route_create = contract
        .split_once("  /internal/v1/model-routes:\n")
        .and_then(|(_, section)| section.split_once("\n  /internal/v1/model-routes/{route_id}:"))
        .map(|(section, _)| section)
        .expect("model-route create OpenAPI operation");
    assert!(route_create.contains("$ref: '#/components/parameters/RouteCreateIdempotencyKey'"));
    let created_response = route_create
        .split_once("        '201':\n")
        .and_then(|(_, section)| section.split_once("        '200':\n"))
        .map(|(section, _)| section)
        .expect("201 route-create response");
    assert!(
        created_response.contains("$ref: '#/components/headers/RouteCreateCreatedDisposition'")
    );
    let reused_response = route_create
        .split_once("        '200':\n")
        .and_then(|(_, section)| section.split_once("        '400':"))
        .map(|(section, _)| section)
        .expect("200 route-create response");
    assert!(reused_response.contains("Exact replay of this caller's unexpired Idempotency-Key"));
    assert!(reused_response.contains("$ref: '#/components/headers/RouteCreateReusedDisposition'"));
    assert!(route_create.contains("        '409': { $ref: '#/components/responses/Conflict' }"));

    let headers = contract
        .split_once("  headers:\n")
        .and_then(|(_, section)| section.split_once("\n  parameters:\n"))
        .map(|(section, _)| section)
        .expect("OpenAPI headers component");
    assert!(headers.contains("RouteCreateCreatedDisposition:"));
    assert!(headers.contains("enum: [created, equivalent_unowned]"));
    assert!(headers.contains("RouteCreateReusedDisposition:"));
    assert!(headers.contains("enum: [reused]"));

    let parameter = contract
        .split_once("    RouteCreateIdempotencyKey:\n")
        .and_then(|(_, section)| section.split_once("    RequiredIdempotencyKey:\n"))
        .map(|(section, _)| section)
        .expect("route-create idempotency parameter");
    assert!(parameter.contains("name: Idempotency-Key"));
    assert!(parameter.contains("required: false"));
    assert!(parameter.contains("minLength: 1, maxLength: 200"));

    let route_request = contract
        .split_once("    CreateModelRouteRequest:\n")
        .and_then(|(_, section)| section.split_once("    ReplaceModelRouteRequest:\n"))
        .map(|(section, _)| section)
        .expect("model-route create request schema");
    assert!(
        route_request
            .contains("        enabled:\n          type: boolean\n          default: true")
    );
}
