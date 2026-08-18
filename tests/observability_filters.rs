use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::Config,
    db::{CreateKeyInput, CreateServiceTokenInput, FinishRequest, NewRequest},
    model::KeyPolicy,
};
use rust_decimal::Decimal;
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

const PEPPER: &[u8] = b"observability filters test pepper is long enough";

async fn get_json(state: &AppState, path: &str, credential: &str) -> (StatusCode, Value) {
    let response = api::router(state.clone())
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {credential}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

#[tokio::test]
async fn postgres_observability_queries_use_the_same_bound_contract() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let mut config = Config::for_test(database_url);
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).unwrap();
    let state = AppState::initialize(config).await.unwrap();
    let unique = Uuid::now_v7();
    let tenant = format!("observability-postgres-{unique}");
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.clone(),
                principal_external_id: "Postgres-Principal".into(),
                alias: "Postgres-Alias".into(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    allowed_models: vec!["*".into()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    let key = state
        .db
        .authenticate_key(&issued.key, PEPPER)
        .await
        .unwrap();
    let route_id = Uuid::now_v7();
    let upstream_id = Uuid::now_v7();
    record(
        &state,
        &key,
        RecordedRequest {
            model: "postgres-diagnostic-model",
            route_id,
            upstream_id,
            duration_ms: 321,
            cost_micros: 750_000,
            status_code: 503,
            error_code: Some("postgres_upstream_error"),
        },
    )
    .await;
    let filter = memeloop_token_center::db::RequestListFilter {
        limit: 10,
        route_id: Some(route_id),
        upstream_account_id: Some(upstream_id),
        min_duration_ms: Some(300),
        max_duration_ms: Some(400),
        min_cost_micros: Some(700_000),
        max_cost_micros: Some(800_000),
        key_alias: Some("postgres-a".into()),
        principal: Some("postgres-p".into()),
        status: Some("error".into()),
        error_code: Some("postgres_upstream_error".into()),
        ..Default::default()
    };
    let requests = state
        .db
        .list_all_requests_filtered(&tenant, filter)
        .await
        .unwrap();
    assert_eq!(requests.len(), 1);
    let now = memeloop_token_center::db::unix_millis();
    let stats = state
        .db
        .operator_stats_filtered(
            &tenant,
            memeloop_token_center::db::StatsFilter {
                from_created_at: Some(now.saturating_sub(86_400_000)),
                to_created_at: Some(now),
                route_id: Some(route_id),
                upstream_account_id: Some(upstream_id),
                key_alias: Some("postgres-a".into()),
                principal: Some("postgres-p".into()),
                status: Some("error".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(stats.summary.total_requests, 1);
    assert_eq!(stats.errors[0].name, "postgres_upstream_error");
}

struct RecordedRequest<'a> {
    model: &'a str,
    route_id: Uuid,
    upstream_id: Uuid,
    duration_ms: i64,
    cost_micros: i64,
    status_code: i64,
    error_code: Option<&'a str>,
}

async fn record(
    state: &AppState,
    key: &memeloop_token_center::model::AuthenticatedKey,
    request: RecordedRequest<'_>,
) -> Uuid {
    let request_id = Uuid::now_v7();
    state
        .db
        .record_request_started(NewRequest {
            request_id,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: "openai-chat".into(),
            model: request.model.into(),
            request_object: format!("memory://request/{request_id}"),
            reservation_id: Uuid::now_v7(),
            upstream_account_id: Some(request.upstream_id),
            model_route_id: Some(request.route_id),
        })
        .await
        .unwrap();
    state
        .db
        .record_request_finished(FinishRequest {
            request_id,
            status_code: request.status_code,
            duration_ms: request.duration_ms,
            input_tokens: 11,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 7,
            service_tier: None,
            cost_micros: request.cost_micros,
            error_code: request.error_code.map(str::to_owned),
            response_object: format!("memory://response/{request_id}"),
        })
        .await
        .unwrap();
    request_id
}

#[tokio::test]
async fn operator_and_self_observability_filters_are_bounded_scoped_and_keyset_paginated() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("observability.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).unwrap();
    let state = AppState::initialize(config).await.unwrap();

    let issue = |tenant: &str, principal: &str, alias: &str| CreateKeyInput {
        tenant_external_id: tenant.into(),
        principal_external_id: principal.into(),
        alias: alias.into(),
        currency: "USD".into(),
        policy: KeyPolicy {
            allowed_models: vec!["*".into()],
            ..KeyPolicy::default()
        },
        initial_balance: Decimal::TEN,
        idempotency_key: None,
    };
    let issued_alpha = state
        .db
        .create_key(
            issue("observe-a", "Alice-Member", "Alpha-Credential"),
            PEPPER,
        )
        .await
        .unwrap();
    let issued_beta = state
        .db
        .create_key(issue("observe-a", "Bob-Member", "Beta-Credential"), PEPPER)
        .await
        .unwrap();
    let issued_other_tenant = state
        .db
        .create_key(
            issue("observe-b", "Alice-Member", "Alpha-Credential"),
            PEPPER,
        )
        .await
        .unwrap();
    let alpha = state
        .db
        .authenticate_key(&issued_alpha.key, PEPPER)
        .await
        .unwrap();
    let beta = state
        .db
        .authenticate_key(&issued_beta.key, PEPPER)
        .await
        .unwrap();
    let other = state
        .db
        .authenticate_key(&issued_other_tenant.key, PEPPER)
        .await
        .unwrap();
    let alpha_route = Uuid::now_v7();
    let beta_route = Uuid::now_v7();
    let upstream = Uuid::now_v7();
    let alpha_request = record(
        &state,
        &alpha,
        RecordedRequest {
            model: "diagnostic-model",
            route_id: alpha_route,
            upstream_id: upstream,
            duration_ms: 150,
            cost_micros: 1_250_000,
            status_code: 502,
            error_code: Some("upstream_boom"),
        },
    )
    .await;
    let _alpha_older = record(
        &state,
        &alpha,
        RecordedRequest {
            model: "diagnostic-model",
            route_id: alpha_route,
            upstream_id: upstream,
            duration_ms: 180,
            cost_micros: 1_500_000,
            status_code: 502,
            error_code: Some("upstream_boom"),
        },
    )
    .await;
    record(
        &state,
        &beta,
        RecordedRequest {
            model: "diagnostic-model",
            route_id: beta_route,
            upstream_id: upstream,
            duration_ms: 20,
            cost_micros: 100_000,
            status_code: 200,
            error_code: None,
        },
    )
    .await;
    record(
        &state,
        &other,
        RecordedRequest {
            model: "diagnostic-model",
            route_id: alpha_route,
            upstream_id: upstream,
            duration_ms: 150,
            cost_micros: 1_250_000,
            status_code: 502,
            error_code: Some("upstream_boom"),
        },
    )
    .await;

    let service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "observe-a-reader".into(),
                scopes: vec!["requests:read".into()],
                tenant_external_id: Some("observe-a".into()),
            },
            PEPPER,
        )
        .await
        .unwrap();
    let now = memeloop_token_center::db::unix_millis();
    let query = format!(
        "from_created_at=0&to_created_at={now}&model=diagnostic-model&protocol=openai-chat&status=error&error_code=upstream_boom&upstream_account_id={upstream}&route_id={alpha_route}&min_duration_ms=100&max_duration_ms=200&min_cost=1&max_cost=2&key_alias=alpha&principal=alice"
    );
    let bounded_query = query.replace(
        "from_created_at=0",
        &format!("from_created_at={}", now.saturating_sub(86_400_000)),
    );
    let (status, requests) = get_json(
        &state,
        &format!("/internal/v1/requests?limit=1&{query}"),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(requests.as_array().unwrap().len(), 1);
    let first = &requests[0];
    let before_created_at = first["created_at"].as_i64().unwrap();
    let before_id = first["request_id"].as_str().unwrap();
    let (status, older) = get_json(
        &state,
        &format!(
            "/internal/v1/requests?limit=1&{query}&before_created_at={before_created_at}&before_id={before_id}"
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(older.as_array().unwrap().len(), 1);
    assert_ne!(older[0]["request_id"], first["request_id"]);

    let (status, stats) = get_json(
        &state,
        &format!("/internal/v1/stats?{bounded_query}"),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stats["summary"]["total_requests"], 2);
    assert_eq!(stats["summary"]["failed_requests"], 2);
    assert_eq!(stats["errors"][0]["name"], "upstream_boom");
    assert_eq!(stats["errors"][0]["requests"], 2);

    let (status, escaped_search) = get_json(
        &state,
        &format!(
            "/internal/v1/requests?from_created_at=0&to_created_at={now}&key_alias=%25&principal=_"
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(escaped_search.as_array().unwrap().is_empty());

    let (status, _) = get_json(
        &state,
        "/internal/v1/stats?from_created_at=0&to_created_at=9000000000000",
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, self_requests) = get_json(
        &state,
        &format!(
            "/self/v1/requests?key_id={}&key_alias=beta&principal=bob&limit=10",
            issued_beta.key_id
        ),
        &issued_alpha.key,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(self_requests.as_array().unwrap().len(), 2);
    assert!(
        self_requests
            .as_array()
            .unwrap()
            .iter()
            .any(|request| request["request_id"] == alpha_request.to_string())
    );

    let (status, _) = get_json(
        &state,
        "/internal/v1/stats?tenant_external_id=observe-b",
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
