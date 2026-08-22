use std::{collections::BTreeSet, time::Duration};

use memeloop_token_center::{
    conversation::ConversationHints,
    db::{CreateKeyInput, CreateServiceTokenInput, FinishRequest, NewRequest, unix_millis},
    model::{AuthenticatedKey, IssuedKey, KeyPolicy},
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::AnyPool;

use super::*;

async fn start_postgres_service(world: &mut TokenCenterWorld) {
    let database_url = std::env::var("MTC_TEST_POSTGRES_URL")
        .expect("the @postgres runner requires MTC_TEST_POSTGRES_URL");
    let parsed = url::Url::parse(&database_url).expect("MTC_TEST_POSTGRES_URL is a URL");
    assert!(
        matches!(parsed.scheme(), "postgres" | "postgresql"),
        "MTC_TEST_POSTGRES_URL must use postgres:// or postgresql://"
    );
    let mock = MockServer::start().await;
    let mut config = Config::for_test(database_url);
    config.upstream_openai_url = Some(mock.uri());
    config.upstream_anthropic_url = Some(mock.uri());
    let state = AppState::initialize(config)
        .await
        .expect("initialize PostgreSQL logical-session service");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind PostgreSQL logical-session service");
    let address = listener.local_addr().expect("logical-session address");
    let server_state = state.clone();
    let server_task = tokio::spawn(async move {
        axum::serve(listener, api::router(server_state))
            .await
            .expect("serve PostgreSQL logical-session application");
    });
    world.service_url = format!("http://{address}");
    world.state = Some(state);
    world.mock = Some(mock);
    world.server_task = Some(server_task);
}

#[given("a logical session token center backed by SQLite")]
async fn logical_sessions_on_sqlite(world: &mut TokenCenterWorld) {
    start_test_service(world).await;
    // Logical-session contracts write deterministic request and projection fixtures
    // directly through the database. They do not exercise generation processing, so
    // let the worker finish cancellation before any fixture write can contend with it.
    let worker = world
        .worker_task
        .take()
        .expect("SQLite logical-session fixture starts a generation worker");
    worker.abort();
    assert!(worker.await.is_err(), "generation worker must be cancelled");
}

#[given("a logical session token center backed by PostgreSQL")]
async fn logical_sessions_on_postgres(world: &mut TokenCenterWorld) {
    start_postgres_service(world).await;
}

fn unique(label: &str) -> String {
    format!("{label}-{}", Uuid::now_v7())
}

async fn issue_key(
    world: &TokenCenterWorld,
    tenant: &str,
    principal: &str,
    alias: &str,
    currency: &str,
) -> (IssuedKey, AuthenticatedKey) {
    let state = world.state.as_ref().expect("logical-session state");
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.to_owned(),
                principal_external_id: principal.to_owned(),
                alias: alias.to_owned(),
                currency: currency.to_owned(),
                policy: KeyPolicy {
                    allowed_models: vec!["*".to_owned()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::new(100, 0),
                idempotency_key: Some(unique("logical-session-key")),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .expect("issue logical-session credential");
    let authenticated = state
        .db
        .authenticate_key(&issued.key, state.config.key_pepper.as_bytes())
        .await
        .expect("authenticate logical-session credential");
    (issued, authenticated)
}

async fn issue_reader(world: &TokenCenterWorld, tenant: &str) -> String {
    let state = world.state.as_ref().expect("logical-session state");
    state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: unique("logical-session-reader"),
                scopes: vec!["requests:read".to_owned()],
                tenant_external_id: Some(tenant.to_owned()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .expect("issue logical-session reader")
        .token
}

async fn get_json(world: &TokenCenterWorld, path: &str, bearer: &str) -> (StatusCode, Value) {
    let response = world
        .client
        .get(format!("{}{path}", world.service_url))
        .bearer_auth(bearer)
        .send()
        .await
        .expect("logical-session HTTP response");
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

async fn completed_session(
    world: &TokenCenterWorld,
    key: &AuthenticatedKey,
    explicit_session: &str,
    model: &str,
    status_code: i64,
    cost_micros: i64,
) -> String {
    let state = world.state.as_ref().expect("logical-session state");
    let request_id = Uuid::now_v7();
    state
        .db
        .record_request_started(NewRequest {
            request_id,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: "openai-responses".to_owned(),
            model: model.to_owned(),
            request_object: format!("memory://logical-session/{request_id}/request"),
            reservation_id: Uuid::now_v7(),
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .expect("start logical-session request");
    state
        .db
        .record_request_finished(FinishRequest {
            request_id,
            status_code,
            duration_ms: 40,
            input_tokens: 11,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 7,
            service_tier: None,
            cost_micros,
            error_code: (status_code >= 400).then(|| "logical_session_failure".to_owned()),
            response_object: format!("memory://logical-session/{request_id}/response"),
        })
        .await
        .expect("finish logical-session request");
    state
        .db
        .record_conversation_observation(
            key,
            request_id,
            &json!({"input": [{"role": "user", "content": explicit_session}]}),
            &ConversationHints {
                session_id: Some(explicit_session.to_owned()),
                ..ConversationHints::default()
            },
            Some("Cucumber"),
        )
        .await
        .expect("attach logical-session request")
        .to_string()
}

async fn active_request_in_session(
    world: &TokenCenterWorld,
    key: &AuthenticatedKey,
    explicit_session: &str,
    model: &str,
) -> String {
    let state = world.state.as_ref().expect("logical-session state");
    let request_id = Uuid::now_v7();
    state
        .db
        .record_request_started(NewRequest {
            request_id,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: "openai-responses".to_owned(),
            model: model.to_owned(),
            request_object: format!("memory://logical-session/{request_id}/active"),
            reservation_id: Uuid::now_v7(),
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .expect("start active logical-session request");
    state
        .db
        .record_conversation_observation(
            key,
            request_id,
            &json!({"input": [{"role": "user", "content": "active filter target"}]}),
            &ConversationHints {
                session_id: Some(explicit_session.to_owned()),
                ..ConversationHints::default()
            },
            Some("Cucumber"),
        )
        .await
        .expect("attach active logical-session request")
        .to_string()
}

async fn rotation_contract(world: &TokenCenterWorld) {
    let tenant = unique("logical-session-rotation");
    let (issued, key) = issue_key(world, &tenant, "rotation-owner", "Rotation owner", "USD").await;
    let (other_issued, _) = issue_key(world, &tenant, "other-owner", "Other owner", "USD").await;
    let session_id =
        completed_session(world, &key, "rotation-session", "rotation-model", 200, 125).await;
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/keys/{}/rotate",
            world.service_url, key.key_id
        ))
        .bearer_auth("test-service-token")
        .header("idempotency-key", unique("logical-session-rotation"))
        .send()
        .await
        .expect("rotate logical-session credential");
    assert_eq!(response.status(), StatusCode::OK);
    let rotated = response
        .json::<Value>()
        .await
        .expect("rotated credential JSON");
    assert_eq!(rotated["key_id"], key.key_id.to_string());
    let rotated_key = rotated["key"].as_str().expect("rotated credential");

    let (status, sessions) = get_json(world, "/self/v1/sessions", rotated_key).await;
    assert_eq!(status, StatusCode::OK, "{sessions}");
    let history = sessions["sessions"]
        .as_array()
        .expect("rotated session history");
    assert!(history.iter().any(|session| {
        session["session_id"] == session_id && session["key_id"] == key.key_id.to_string()
    }));
    let (status, _) = get_json(world, "/self/v1/sessions", &issued.key).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = get_json(
        world,
        &format!("/self/v1/sessions/{session_id}"),
        &other_issued.key,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn operator_filter_contract(world: &TokenCenterWorld) {
    let tenant = unique("logical-session-filter");
    let (_, target_key) =
        issue_key(world, &tenant, "filter-target", "Needle credential", "USD").await;
    let (_, noise_key) = issue_key(world, &tenant, "filter-noise", "Noise credential", "USD").await;
    let reader = issue_reader(world, &tenant).await;
    let explicit = unique("filter-target-session");
    let target_session = completed_session(
        world,
        &target_key,
        &explicit,
        "filter-target-model",
        503,
        99,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(2)).await;
    assert_eq!(
        active_request_in_session(world, &target_key, &explicit, "filter-target-model").await,
        target_session
    );
    tokio::time::sleep(Duration::from_millis(10)).await;
    for index in 0..55 {
        completed_session(
            world,
            &noise_key,
            &format!("newer-noise-{index}"),
            "noise-model",
            200,
            1,
        )
        .await;
    }

    let (status, baseline) =
        get_json(world, "/internal/v1/sessions?limit=50&state=all", &reader).await;
    assert_eq!(status, StatusCode::OK, "{baseline}");
    let baseline = baseline["sessions"].as_array().expect("operator sessions");
    assert_eq!(baseline.len(), 50);
    assert!(
        baseline
            .iter()
            .all(|session| session["session_id"] != target_session)
    );

    let filters = [
        "state=active".to_owned(),
        "state=has_errors".to_owned(),
        format!("q={target_session}"),
        format!("key_id={}", target_key.key_id),
        "model=filter-target-model".to_owned(),
    ];
    for filter in filters {
        let (status, body) = get_json(
            world,
            &format!("/internal/v1/sessions?limit=10&{filter}"),
            &reader,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "filter={filter}: {body}");
        let match_row = body["sessions"]
            .as_array()
            .expect("filtered sessions")
            .iter()
            .find(|session| session["session_id"] == target_session)
            .unwrap_or_else(|| panic!("filter={filter} did not find target: {body}"));
        assert_eq!(match_row["key_id"], target_key.key_id.to_string());
        if filter == "state=has_errors" {
            assert_eq!(match_row["errors"], 1, "{body}");
            assert_eq!(match_row["last_status"], "active", "{body}");
        }
    }
}

async fn force_same_activity(
    world: &TokenCenterWorld,
    key: &AuthenticatedKey,
    session_ids: &[String],
    timestamp: i64,
) {
    let database_url = &world
        .state
        .as_ref()
        .expect("logical-session state")
        .config
        .database_url;
    let pool = AnyPool::connect(database_url)
        .await
        .expect("connect cursor fixture database");
    for session_id in session_ids {
        sqlx::query(
            "UPDATE session_usage_totals SET last_activity_at = $1 WHERE key_id = $2 AND session_id = $3",
        )
        .bind(timestamp)
        .bind(key.key_id.to_string())
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("move cursor usage total");
        sqlx::query(
            "UPDATE conversation_key_clusters SET updated_at = $1 WHERE key_id = $2 AND cluster_id = $3",
        )
        .bind(timestamp)
        .bind(key.key_id.to_string())
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("move cursor conversation projection");
    }
    pool.close().await;
}

async fn cursor_contract(world: &TokenCenterWorld) {
    let tenant = unique("logical-session-cursor");
    let (issued, key) = issue_key(world, &tenant, "cursor-owner", "Cursor owner", "USD").await;
    let mut expected = Vec::new();
    for index in 0..7 {
        expected.push(
            completed_session(
                world,
                &key,
                &format!("equal-cursor-{index}"),
                "cursor-model",
                200,
                10,
            )
            .await,
        );
    }
    force_same_activity(world, &key, &expected, 1_790_000_000_000).await;

    let mut path = "/self/v1/sessions?limit=2".to_owned();
    let mut seen = Vec::new();
    loop {
        let (status, body) = get_json(world, &path, &issued.key).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        seen.extend(
            body["sessions"]
                .as_array()
                .expect("cursor page sessions")
                .iter()
                .map(|session| {
                    assert_eq!(session["last_activity_at"], 1_790_000_000_000_i64);
                    session["session_id"]
                        .as_str()
                        .expect("cursor session id")
                        .to_owned()
                }),
        );
        let Some(cursor) = body["next_cursor"].as_object() else {
            break;
        };
        path = format!(
            "/self/v1/sessions?limit=2&before_last_activity_at={}&before_session_id={}",
            cursor["before_last_activity_at"]
                .as_i64()
                .expect("cursor activity"),
            cursor["before_session_id"]
                .as_str()
                .expect("cursor session")
        );
        assert!(seen.len() <= expected.len(), "cursor loop: {body}");
    }
    assert_eq!(seen.len(), expected.len());
    assert_eq!(
        seen.iter().collect::<BTreeSet<_>>().len(),
        seen.len(),
        "cursor returned a duplicate"
    );
    assert_eq!(
        seen.into_iter().collect::<BTreeSet<_>>(),
        expected.into_iter().collect::<BTreeSet<_>>()
    );
}

async fn archive_only_contract(world: &TokenCenterWorld) {
    let tenant = unique("logical-session-archive");
    let (issued, key) = issue_key(world, &tenant, "archive-owner", "Archive owner", "USD").await;
    let reader = issue_reader(world, &tenant).await;
    let session_id = format!("unlinked:{}", key.key_id);
    let archive_request_id = Uuid::now_v7();
    let now = unix_millis();
    let pool = AnyPool::connect(
        &world
            .state
            .as_ref()
            .expect("logical-session state")
            .config
            .database_url,
    )
    .await
    .expect("connect archive-only fixture database");
    sqlx::query(
        r#"INSERT INTO session_archive_unlinked_requests (
               tenant_id, source, external_request_id, archive_request_id, key_id,
               principal_id, conversation_cluster_id, source_started_at,
               source_completed_at, protocol, model, status_code, duration_ms,
               input_tokens, output_tokens, error_code, request_object, imported_at)
           VALUES ($1, $2, $3, $4, $5, $6, NULL, $7, $8, 'openai-responses',
                   'archive-only-model', 429, 800, 20, 10, 'rate_limited', $9, $7)"#,
    )
    .bind(key.tenant_id.to_string())
    .bind(unique("archive-source"))
    .bind(unique("archive-external"))
    .bind(archive_request_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.principal_id.to_string())
    .bind(now)
    .bind(now + 800)
    .bind(format!(
        "memory://logical-session/archive/{archive_request_id}"
    ))
    .execute(&pool)
    .await
    .expect("insert archive-only request");
    sqlx::query(
        r#"INSERT INTO session_archive_totals (
               tenant_id, key_id, session_id, last_activity_at, requests, errors,
               input_tokens, output_tokens, duration_count, duration_sum_ms)
           VALUES ($1, $2, $3, $4, 1, 1, 20, 10, 1, 800)"#,
    )
    .bind(key.tenant_id.to_string())
    .bind(key.key_id.to_string())
    .bind(&session_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert archive-only projection");
    pool.close().await;

    let (status, sessions) = get_json(world, "/self/v1/sessions", &issued.key).await;
    assert_eq!(status, StatusCode::OK, "{sessions}");
    let archive = sessions["sessions"]
        .as_array()
        .expect("archive-only sessions")
        .iter()
        .find(|session| session["session_id"] == session_id)
        .expect("archive-only logical session");
    assert_eq!(archive["requests"], 0);
    assert_eq!(archive["errors"], 0);
    assert_eq!(archive["costs"], json!([]));
    assert_eq!(archive["archived_only_requests"], 1);
    assert_eq!(archive["archived_only_errors"], 1);
    assert_eq!(archive["archived_only_input_tokens"], 20);
    assert_eq!(archive["archived_only_output_tokens"], 10);

    let (status, stats) = get_json(world, "/self/v1/stats", &issued.key).await;
    assert_eq!(status, StatusCode::OK, "{stats}");
    assert_eq!(stats["summary"]["total_requests"], 0);
    assert_eq!(stats["summary"]["costs"], json!([]));
    let (status, analysis) = get_json(
        world,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={}&to_created_at={}&granularity=hour",
            now - 1_000,
            now + 1_000
        ),
        &reader,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{analysis}");
    assert_eq!(analysis["summary"]["requests"], 0);
    assert_eq!(analysis["summary"]["costs"], json!([]));
    assert_eq!(analysis["by_session"], json!([]));
}

async fn finish_unlinked(
    world: &TokenCenterWorld,
    key: &AuthenticatedKey,
    model: &str,
    cost_micros: i64,
) -> Uuid {
    let state = world.state.as_ref().expect("logical-session state");
    let request_id = Uuid::now_v7();
    state
        .db
        .record_request_started(NewRequest {
            request_id,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: "openai-responses".to_owned(),
            model: model.to_owned(),
            request_object: format!("memory://logical-session/{request_id}/unlinked"),
            reservation_id: Uuid::now_v7(),
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .expect("start unlinked usage request");
    state
        .db
        .record_request_finished(FinishRequest {
            request_id,
            status_code: 200,
            duration_ms: 20,
            input_tokens: 4,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 2,
            service_tier: None,
            cost_micros,
            error_code: None,
            response_object: format!("memory://logical-session/{request_id}/response"),
        })
        .await
        .expect("finish unlinked usage request");
    request_id
}

async fn change_unlinked_currency(
    world: &TokenCenterWorld,
    key: &AuthenticatedKey,
    request_id: Uuid,
    model: &str,
    currency: &str,
) {
    let database_url = &world
        .state
        .as_ref()
        .expect("logical-session state")
        .config
        .database_url;
    let pool = AnyPool::connect(database_url)
        .await
        .expect("connect currency fixture database");
    sqlx::query("UPDATE request_records SET currency = $1 WHERE id = $2")
        .bind(currency)
        .bind(request_id.to_string())
        .execute(&pool)
        .await
        .expect("update historical request currency");
    sqlx::query("UPDATE request_stats_facts SET currency = $1 WHERE request_id = $2")
        .bind(currency)
        .bind(request_id.to_string())
        .execute(&pool)
        .await
        .expect("update historical fact currency");
    for table in [
        "request_daily_aggregates",
        "usage_analysis_hourly",
        "usage_analysis_daily",
        "session_usage_hourly",
        "session_usage_daily",
    ] {
        let statement =
            format!("UPDATE {table} SET currency = $1 WHERE key_id = $2 AND model = $3");
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(currency)
            .bind(key.key_id.to_string())
            .bind(model)
            .execute(&pool)
            .await
            .expect("update historical rollup currency");
    }
    let session_id = format!("unlinked:{}", key.key_id);
    sqlx::query("DELETE FROM session_usage_totals WHERE key_id = $1 AND session_id = $2")
        .bind(key.key_id.to_string())
        .bind(&session_id)
        .execute(&pool)
        .await
        .expect("replace mixed-currency session total");
    sqlx::query(
        r#"INSERT INTO session_usage_totals (
               tenant_id, key_id, session_id, currency, last_activity_at,
               requests, errors, input_tokens, output_tokens, duration_count,
               duration_sum_ms, cost_micros)
           SELECT tenant_id, key_id, session_id, currency, MAX(created_at),
                  COUNT(*), SUM(CASE WHEN status_class = 'failure' THEN 1 ELSE 0 END),
                  SUM(input_tokens), SUM(output_tokens), COUNT(*), SUM(duration_ms),
                  SUM(cost_micros)
             FROM request_stats_facts
            WHERE key_id = $1 AND session_id = $2
            GROUP BY tenant_id, key_id, session_id, currency"#,
    )
    .bind(key.key_id.to_string())
    .bind(&session_id)
    .execute(&pool)
    .await
    .expect("rebuild mixed-currency session total");
    pool.close().await;
}

async fn usage_by_session_contract(world: &TokenCenterWorld) {
    let tenant = unique("logical-session-usage");
    let (_, key) = issue_key(
        world,
        &tenant,
        "usage-mixed",
        "Mixed currency credential",
        "USD",
    )
    .await;
    let reader = issue_reader(world, &tenant).await;
    finish_unlinked(world, &key, "currency-usd-model", 1_000_000).await;
    let cny_request = finish_unlinked(world, &key, "currency-cny-model", 2_000_000).await;
    change_unlinked_currency(world, &key, cny_request, "currency-cny-model", "CNY").await;
    let linked_session = completed_session(
        world,
        &key,
        "readable-usage-session",
        "readable-session-model",
        200,
        3_000_000,
    )
    .await;
    let now = unix_millis();
    let (status, body) = get_json(
        world,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={}&to_created_at={}&granularity=hour",
            now - 60_000,
            now + 60_000
        ),
        &reader,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sessions = body["by_session"].as_array().expect("usage by_session");
    assert_eq!(sessions.len(), 2, "{body}");
    let session = sessions
        .iter()
        .find(|session| session["unlinked"] == true)
        .expect("unlinked usage bucket");
    assert_eq!(session["id"], format!("unlinked:{}", key.key_id));
    assert_eq!(session["label"], "Mixed currency credential");
    assert_eq!(session["key_id"], key.key_id.to_string());
    assert_eq!(session["key_alias"], "Mixed currency credential");
    assert_eq!(session["unlinked"], true);
    assert_eq!(session["requests"], 2);
    assert_eq!(
        session["costs"],
        json!([
            {"currency": "CNY", "cost": "2"},
            {"currency": "USD", "cost": "1"}
        ])
    );
    let linked = sessions
        .iter()
        .find(|session| session["id"] == linked_session)
        .expect("linked usage bucket");
    assert_eq!(
        linked["label"],
        "Mixed currency credential · readable-session-model"
    );
    assert_eq!(linked["key_id"], key.key_id.to_string());
    assert_eq!(linked["unlinked"], false);
    assert_eq!(
        body["summary"]["costs"],
        json!([
            {"currency": "CNY", "cost": "2"},
            {"currency": "USD", "cost": "4"}
        ])
    );
}

#[then(
    "rotated credentials retain logical-session history without granting another credential access"
)]
async fn rotated_credentials_keep_sessions(world: &mut TokenCenterWorld) {
    rotation_contract(world).await;
}

#[then("every operator logical-session filter finds its match beyond the first fifty sessions")]
async fn operator_filters_precede_limit(world: &mut TokenCenterWorld) {
    operator_filter_contract(world).await;
}

#[then("equal logical-session activity timestamps paginate without duplicates or omissions")]
async fn equal_cursor_is_total_order(world: &mut TokenCenterWorld) {
    cursor_contract(world).await;
}

#[then("archive-only logical-session metrics do not change authoritative usage or cost")]
async fn archive_only_is_not_billed(world: &mut TokenCenterWorld) {
    archive_only_contract(world).await;
}

#[then("usage analysis sessions expose key identity and never combine USD with CNY")]
async fn session_usage_is_currency_safe(world: &mut TokenCenterWorld) {
    usage_by_session_contract(world).await;
}

#[then("every logical-session acceptance contract holds")]
async fn all_session_contracts(world: &mut TokenCenterWorld) {
    rotation_contract(world).await;
    operator_filter_contract(world).await;
    cursor_contract(world).await;
    archive_only_contract(world).await;
    usage_by_session_contract(world).await;
}
