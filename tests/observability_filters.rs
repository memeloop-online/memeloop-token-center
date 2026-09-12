use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::Config,
    conversation::ConversationHints,
    db::{CreateKeyInput, CreateServiceTokenInput, FinishRequest, NewRequest},
    filter_ast::{
        TypedFilterAst, TypedFilterCondition, TypedFilterField, TypedFilterLogicalOperator,
        TypedFilterOperator, TypedFilterValue,
    },
    model::KeyPolicy,
};
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::AnyPool;
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
async fn request_projection_preserves_source_truth_scope_and_cross_source_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("request-projection.db").display()
    );
    let mut config = Config::for_test(database_url.clone());
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).unwrap();
    let state = AppState::initialize(config).await.unwrap();
    let inspection = AnyPool::connect(&database_url).await.unwrap();

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
    let issued = state
        .db
        .create_key(
            issue("projection-a", "Projection Member", "Projection Key"),
            PEPPER,
        )
        .await
        .unwrap();
    let foreign_issued = state
        .db
        .create_key(
            issue("projection-b", "Foreign Member", "Foreign Key"),
            PEPPER,
        )
        .await
        .unwrap();
    let key = state
        .db
        .authenticate_key(&issued.key, PEPPER)
        .await
        .unwrap();
    let foreign = state
        .db
        .authenticate_key(&foreign_issued.key, PEPPER)
        .await
        .unwrap();
    let now = memeloop_token_center::db::unix_millis();

    let text_id = Uuid::now_v7();
    let text_reservation_id = Uuid::now_v7();
    state
        .db
        .record_request_started(NewRequest {
            request_id: text_id,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: "openai".into(),
            model: "projection-text".into(),
            request_object: "inline-json:{\"prompt\":\"bounded\"}".into(),
            reservation_id: text_reservation_id,
            upstream_account_id: Some(Uuid::now_v7()),
            model_route_id: Some(Uuid::now_v7()),
        })
        .await
        .unwrap();
    let capturing = state.db.list_requests(key.key_id, 10).await.unwrap();
    let capturing = capturing
        .iter()
        .find(|request| request.request_id == text_id)
        .unwrap();
    assert_eq!(
        capturing.archive_state,
        memeloop_token_center::model::RequestArchiveState::Capturing
    );
    assert!(capturing.input_tokens.is_none());
    assert!(capturing.cost.is_none());

    state
        .db
        .record_request_finished(FinishRequest {
            request_id: text_id,
            status_code: 200,
            duration_ms: 12,
            input_tokens: 4,
            cached_input_tokens: 1,
            cache_write_tokens: 0,
            output_tokens: 2,
            service_tier: None,
            cost_micros: 125_000,
            error_code: None,
            response_object: format!("gap://{text_id}/response"),
        })
        .await
        .unwrap();
    sqlx::query("INSERT INTO response_archive_spools (request_id, tenant_id, reservation_id, state, next_attempt_at, created_at, updated_at, expires_at) VALUES ($1, $2, $3, 'pending', $4, $4, $4, $5)")
        .bind(text_id.to_string())
        .bind(key.tenant_id.to_string())
        .bind(text_reservation_id.to_string())
        .bind(now)
        .bind(now + 60_000)
        .execute(&inspection)
        .await
        .unwrap();
    let pending = state.db.list_requests(key.key_id, 10).await.unwrap();
    assert_eq!(
        pending
            .iter()
            .find(|request| request.request_id == text_id)
            .unwrap()
            .archive_state,
        memeloop_token_center::model::RequestArchiveState::Pending
    );
    sqlx::query("UPDATE response_archive_spools SET state = 'uploading', lease_owner = 'fixture', lease_token = 'fixture', lease_expires_at = $1 WHERE request_id = $2")
        .bind(now + 30_000)
        .bind(text_id.to_string())
        .execute(&inspection)
        .await
        .unwrap();
    let uploading = state.db.list_requests(key.key_id, 10).await.unwrap();
    assert_eq!(
        uploading
            .iter()
            .find(|request| request.request_id == text_id)
            .unwrap()
            .archive_state,
        memeloop_token_center::model::RequestArchiveState::Uploading
    );

    let generation_id = Uuid::now_v7();
    let generation_route_id = Uuid::now_v7();
    let generation_upstream_id = Uuid::now_v7();
    sqlx::query("INSERT INTO generation_jobs (id, tenant_id, key_id, upstream_account_id, reservation_id, public_model, upstream_model, driver, status, request_object, error_code, estimated_units, billed_units, cost_micros, next_attempt_at, created_at, updated_at, completed_at, billing_unit_snapshot, micros_per_unit_snapshot, model_route_id) VALUES ($1, $2, $3, $4, $5, 'projection-image', 'projection-image-upstream', 'comfyui', 'cancelled', 'inline-json:{}', 'cancelled_by_user', 1, 0, 0, $6, $6, $6, $7, 'job', 1000000, $8)")
        .bind(generation_id.to_string())
        .bind(key.tenant_id.to_string())
        .bind(key.key_id.to_string())
        .bind(generation_upstream_id.to_string())
        .bind(Uuid::now_v7().to_string())
        .bind(now - 10)
        .bind(now - 5)
        .bind(generation_route_id.to_string())
        .execute(&inspection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO generation_stats_facts (job_id, tenant_id, key_id, created_at, model, status_class, error_code, upstream_account_id, duration_ms, cost_micros, billed_units, modality, billing_unit, model_route_id, currency) VALUES ($1, $2, $3, $4, 'projection-image', 'failure', 'cancelled_by_user', $5, 5, 0, 0, 'image', 'job', $6, 'USD')")
        .bind(generation_id.to_string())
        .bind(key.tenant_id.to_string())
        .bind(key.key_id.to_string())
        .bind(now - 10)
        .bind(generation_upstream_id.to_string())
        .bind(generation_route_id.to_string())
        .execute(&inspection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) VALUES ($1, $2, $3, $4, $5, 'finished', 'generation', 'projection-image', 499, 5, 0, 0, 0, 'cancelled_by_user')")
        .bind(Uuid::now_v7().to_string())
        .bind(key.tenant_id.to_string())
        .bind(key.key_id.to_string())
        .bind(generation_id.to_string())
        .bind(now)
        .execute(&inspection)
        .await
        .unwrap();

    let archive_id = Uuid::now_v7();
    let archive_completed_at = now - 15;
    sqlx::query("INSERT INTO session_archive_correlations (tenant_id, source, external_request_id, disposition, key_id, principal_id, record_digest, proof_digest, identity_proof_kind, identity_proof_digest, source_model, source_started_at, correlated_at) VALUES ($1, 'fixture', $2, 'unlinked', $3, $4, $7, $8, 'credential', $9, 'projection-archive', $5, $6)")
        .bind(key.tenant_id.to_string())
        .bind(archive_id.to_string())
        .bind(key.key_id.to_string())
        .bind(key.principal_id.to_string())
        .bind(now - 20)
        .bind(now)
        .bind("a".repeat(64))
        .bind("b".repeat(64))
        .bind("c".repeat(64))
        .execute(&inspection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO session_archive_unlinked_requests (tenant_id, source, external_request_id, archive_request_id, key_id, principal_id, source_started_at, source_completed_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, request_object, response_object, imported_at, source_session_id) VALUES ($1, 'fixture', $2, $2, $3, $4, $5, $6, 'openai-responses', 'projection-archive', 200, 5, 0, 0, 'inline-json:{}', 'inline-json:{}', $7, 'fixture-session')")
        .bind(key.tenant_id.to_string())
        .bind(archive_id.to_string())
        .bind(key.key_id.to_string())
        .bind(key.principal_id.to_string())
        .bind(now - 20)
        .bind(archive_completed_at)
        .bind(now)
        .execute(&inspection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO session_archive_unlinked_requests (tenant_id, source, external_request_id, archive_request_id, key_id, principal_id, source_started_at, protocol, model, input_tokens, output_tokens, imported_at, source_session_id) VALUES ($1, 'fixture', $2, $2, $3, $4, $5, 'openai-responses', 'foreign-archive', 0, 0, $5, 'foreign-session')")
        .bind(foreign.tenant_id.to_string())
        .bind(Uuid::now_v7().to_string())
        .bind(foreign.key_id.to_string())
        .bind(foreign.principal_id.to_string())
        .bind(now)
        .execute(&inspection)
        .await
        .unwrap();

    let self_rows = state.db.list_requests(key.key_id, 10).await.unwrap();
    assert_eq!(self_rows.len(), 3);
    assert!(
        self_rows
            .iter()
            .all(|row| row.credential_identity.is_none())
    );
    let generation = self_rows
        .iter()
        .find(|row| row.request_id == generation_id)
        .unwrap();
    assert_eq!(generation.status_code, Some(499));
    assert_eq!(
        generation.lifecycle_state,
        memeloop_token_center::model::RequestLifecycleState::Cancelled
    );
    assert_eq!(generation.error_code.as_deref(), Some("cancelled_by_user"));
    assert!(generation.usage.tokens.is_none());
    assert_eq!(
        generation
            .usage
            .generation
            .as_ref()
            .and_then(|usage| usage.billing_unit.as_deref()),
        Some("job")
    );
    let archived = self_rows
        .iter()
        .find(|row| row.request_id == archive_id)
        .unwrap();
    assert!(archived.completed_at.is_none());
    assert_eq!(archived.source_completed_at, Some(archive_completed_at));
    assert!(archived.input_tokens.is_none());
    assert!(archived.output_tokens.is_none());
    assert!(archived.cost.is_none());
    assert!(!archived.billing.billable);
    assert_eq!(
        archived.archive_state,
        memeloop_token_center::model::RequestArchiveState::Bound
    );

    let operator_rows = state
        .db
        .list_all_requests("projection-a", 10)
        .await
        .unwrap();
    assert_eq!(operator_rows.len(), 3);
    assert!(operator_rows.iter().all(|row| {
        row.credential_identity
            .as_ref()
            .is_some_and(|identity| identity.tenant_external_id == "projection-a")
    }));
    let ordinary_route = state
        .db
        .list_all_requests_filtered(
            "projection-a",
            memeloop_token_center::db::RequestListFilter {
                limit: 10,
                route_id: Some(generation_route_id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(ordinary_route.len(), 1);
    assert_eq!(ordinary_route[0].request_id, generation_id);
    let typed_route = state
        .db
        .list_all_requests_filtered(
            "projection-a",
            memeloop_token_center::db::RequestListFilter {
                limit: 10,
                typed_ast: Some(TypedFilterAst {
                    logical_operator: TypedFilterLogicalOperator::And,
                    conditions: vec![TypedFilterCondition {
                        field: TypedFilterField::RouteId,
                        operator: TypedFilterOperator::Equals,
                        value: TypedFilterValue::Uuid(generation_route_id),
                        upper: None,
                    }],
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(typed_route.len(), 1);
    assert_eq!(typed_route[0].request_id, generation_id);
    let events = state
        .db
        .request_events_after("projection-a", 0, None, 100)
        .await
        .unwrap();
    let cancelled_event = events
        .iter()
        .find(|event| event.request_id == generation_id)
        .unwrap();
    assert_eq!(cancelled_event.status_code, Some(499));
    assert_eq!(
        cancelled_event.lifecycle_state,
        memeloop_token_center::model::RequestLifecycleState::Cancelled
    );
    assert_eq!(cancelled_event.billing.cost.as_deref(), Some("0"));

    let service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "projection-a-reader".into(),
                scopes: vec!["requests:read".into()],
                tenant_external_id: Some("projection-a".into()),
            },
            PEPPER,
        )
        .await
        .unwrap();
    let (status, historical) = get_json(
        &state,
        "/internal/v1/requests?protocol=openai-responses&limit=10",
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(historical.as_array().unwrap().len(), 1);
    assert_eq!(historical[0]["request_id"], archive_id.to_string());
    let (status, archive_detail) = get_json(
        &state,
        &format!("/internal/v1/requests/{archive_id}"),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(archive_detail["completed_at"].is_null());
    assert_eq!(archive_detail["source_completed_at"], archive_completed_at);
    assert_eq!(archive_detail["archive"]["request"]["complete"], true);
    assert_eq!(archive_detail["archive"]["response"]["complete"], true);
    assert_eq!(archive_detail["billing"]["billable"], false);

    let (status, first_page) = get_json(
        &state,
        "/internal/v1/requests?limit=2&paged=true",
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first_page["requests"].as_array().unwrap().len(), 2);
    let cursor = &first_page["next_cursor"];
    let (status, second_page) = get_json(
        &state,
        &format!(
            "/internal/v1/requests?limit=2&paged=true&before_created_at={}&before_id={}",
            cursor["before_created_at"].as_i64().unwrap(),
            cursor["before_id"].as_str().unwrap()
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second_page["requests"].as_array().unwrap().len(), 1);
    let mut paged_ids = first_page["requests"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second_page["requests"].as_array().unwrap())
        .map(|row| row["request_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    paged_ids.sort();
    paged_ids.dedup();
    assert_eq!(paged_ids.len(), 3);
    assert!(paged_ids.contains(&text_id.to_string()));
    assert!(paged_ids.contains(&generation_id.to_string()));
    assert!(paged_ids.contains(&archive_id.to_string()));

    sqlx::query("UPDATE response_archive_spools SET state = 'gap', last_error_code = 'upload_failed', lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL WHERE request_id = $1")
        .bind(text_id.to_string())
        .execute(&inspection)
        .await
        .unwrap();
    let (status, text_detail) = get_json(
        &state,
        &format!("/internal/v1/requests/{text_id}"),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text_detail["archive_state"], "gap");
    assert_eq!(text_detail["archive"]["request"]["state"], "bound");
    assert_eq!(text_detail["archive"]["request"]["complete"], true);
    assert_eq!(text_detail["archive"]["response"]["state"], "gap");
    assert_eq!(
        text_detail["archive"]["response"]["reason"],
        "upload_failed"
    );
    assert_eq!(text_detail["usage"]["tokens"]["input_tokens"], 4);
    assert_eq!(text_detail["billing"]["cost"], "0.125");
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
    let confirmed_request = record(
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
    let confirmed_cluster = state
        .db
        .record_conversation_observation(
            &key,
            confirmed_request,
            &serde_json::json!({"input": "persisted PostgreSQL session"}),
            &ConversationHints {
                session_name: Some("PostgreSQL session".into()),
                agent_id: Some("postgres-contract-agent".into()),
                task_kind: Some("contract-test".into()),
                ..Default::default()
            },
            Some("contract-test"),
        )
        .await
        .unwrap();
    let unlinked_request = record(
        &state,
        &key,
        RecordedRequest {
            model: "postgres-unlinked-model",
            route_id: Uuid::now_v7(),
            upstream_id: Uuid::now_v7(),
            duration_ms: 10,
            cost_micros: 1,
            status_code: 200,
            error_code: None,
        },
    )
    .await;
    let projected = state.db.list_requests(key.key_id, 10).await.unwrap();
    let confirmed = projected
        .iter()
        .find(|request| request.request_id == confirmed_request)
        .unwrap()
        .session_context
        .as_ref()
        .unwrap();
    assert_eq!(
        confirmed.session_id.as_deref(),
        Some(confirmed_cluster.to_string().as_str())
    );
    assert_eq!(
        confirmed.association,
        memeloop_token_center::model::RequestSessionAssociation::Confirmed
    );
    assert_eq!(
        confirmed.session_name.as_deref(),
        Some("PostgreSQL session")
    );
    assert_eq!(confirmed.semantics_source.as_deref(), Some("declared"));
    let unlinked = projected
        .iter()
        .find(|request| request.request_id == unlinked_request)
        .unwrap()
        .session_context
        .as_ref()
        .unwrap();
    assert_eq!(
        unlinked.association,
        memeloop_token_center::model::RequestSessionAssociation::Unlinked
    );
    assert!(unlinked.session_id.is_none());
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
    let request = &requests[0];
    assert_eq!(request.request_id, confirmed_request);
    assert_eq!(request.upstream_account_id, Some(upstream_id));
    assert_eq!(request.route_id, Some(route_id));
    assert!(request.completed_at.is_some());
    assert_eq!(request.currency.as_deref(), Some("USD"));
    let detail = state
        .db
        .request_archive_refs_for_tenant(&tenant, confirmed_request)
        .await
        .unwrap();
    assert_eq!(detail.view.upstream_account_id, Some(upstream_id));
    assert_eq!(detail.view.route_id, Some(route_id));
    assert!(detail.view.completed_at.is_some());
    assert_eq!(detail.view.currency.as_deref(), Some("USD"));
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
    let alpha_cluster = state
        .db
        .record_conversation_observation(
            &alpha,
            alpha_request,
            &serde_json::json!({"input": "operator-visible confirmed session"}),
            &ConversationHints {
                session_name: Some("Operator-visible session".into()),
                ..Default::default()
            },
            Some("contract-test"),
        )
        .await
        .unwrap();
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
    let other_tenant_request = record(
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
    let projected_request = [first, &older[0]]
        .into_iter()
        .find(|request| request["request_id"] == alpha_request.to_string())
        .expect("operator-scoped confirmed request");
    assert_eq!(
        projected_request["session_context"]["association"],
        "confirmed"
    );
    assert_eq!(
        projected_request["session_context"]["session_id"],
        alpha_cluster.to_string()
    );
    assert_eq!(
        projected_request["session_context"]["session_name"],
        "Operator-visible session"
    );
    assert_eq!(
        projected_request["upstream_account_id"],
        upstream.to_string()
    );
    assert_eq!(projected_request["route_id"], alpha_route.to_string());
    assert!(projected_request["completed_at"].is_i64());
    assert_eq!(projected_request["currency"], "USD");

    let (status, detail) = get_json(
        &state,
        &format!("/internal/v1/requests/{alpha_request}"),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["upstream_account_id"], upstream.to_string());
    assert_eq!(detail["route_id"], alpha_route.to_string());
    assert!(detail["completed_at"].is_i64());
    assert_eq!(detail["currency"], "USD");

    let (status, _) = get_json(
        &state,
        &format!("/internal/v1/requests/{other_tenant_request}"),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

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
