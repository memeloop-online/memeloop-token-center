use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::{AnyPool, Row};
use tower::ServiceExt;
use uuid::Uuid;

use super::super::*;
use crate::{AppState, api, config::Config};

const PEPPER: &[u8] = b"session candidate-first test pepper is long enough";

#[test]
fn candidate_first_dispatch_is_only_the_unfiltered_first_page() {
    use super::super::session_analytics::should_use_candidate_first_page;

    assert!(should_use_candidate_first_page(true, false, "all", "", ""));
    assert!(!should_use_candidate_first_page(true, true, "all", "", ""));
    assert!(!should_use_candidate_first_page(
        true, false, "active", "", ""
    ));
    assert!(!should_use_candidate_first_page(
        true,
        false,
        "has_errors",
        "",
        ""
    ));
    assert!(!should_use_candidate_first_page(
        true,
        false,
        "all",
        "gpt-5.6-sol",
        ""
    ));
    assert!(!should_use_candidate_first_page(
        true, false, "all", "", "session%"
    ));
    assert!(!should_use_candidate_first_page(
        false, false, "all", "", ""
    ));
}

#[tokio::test]
async fn postgres_candidate_first_sessions_match_reference_and_ignore_old_history_growth() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!("MTC_TEST_POSTGRES_URL is unset; skipping PostgreSQL session plan contract");
        return;
    };
    let mut config = Config::for_test(database_url.clone());
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).expect("UTF-8 pepper");
    let state = AppState::initialize(config)
        .await
        .expect("initialize PostgreSQL state");
    let unique = Uuid::now_v7();
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("session-candidate-pg-{unique}"),
                principal_external_id: "postgres-scale".into(),
                alias: "PostgreSQL scale".into(),
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
        .expect("create PostgreSQL key");
    let key = state
        .db
        .authenticate_key(&issued.key, PEPPER)
        .await
        .expect("authenticate PostgreSQL key");
    sqlx::any::install_default_drivers();
    let pool = AnyPool::connect(&database_url)
        .await
        .expect("connect PostgreSQL pool");
    let historical_cluster_id = Uuid::now_v7();
    let base = unix_millis();
    let seed = unique.to_string();

    sqlx::query(
        "INSERT INTO conversation_clusters (id, tenant_id, principal_id, explicit_session_id, created_at, updated_at) VALUES ($1, $2, $3, 'postgres-large-session', $4, $5)",
    )
    .bind(historical_cluster_id.to_string())
    .bind(key.tenant_id.to_string())
    .bind(key.principal_id.to_string())
    .bind(base)
    .bind(base + 110_001)
    .execute(&pool)
    .await
    .expect("PostgreSQL historical cluster");
    insert_history(&pool, &key, historical_cluster_id, base, &seed, 1, 10_001).await;
    sqlx::query(
        "INSERT INTO conversation_key_clusters (key_id, cluster_id, explicit_session_id, updated_at, request_count, candidate_edge_count) VALUES ($1, $2, 'postgres-large-session', $3, 110001, 0)",
    )
    .bind(key.key_id.to_string())
    .bind(historical_cluster_id.to_string())
    .bind(base + 110_001)
    .execute(&pool)
    .await
    .expect("PostgreSQL historical projection");

    // Fifty-one newer identities fill the entire internal page. The large
    // completed conversation must therefore never enter candidate aggregation.
    sqlx::query(
        "WITH source AS (SELECT value, md5('recent-session-' || $4 || value::TEXT) AS hash FROM generate_series(1, 51) value), rows AS (SELECT value, substring(hash FROM 1 FOR 8) || '-' || substring(hash FROM 9 FOR 4) || '-7' || substring(hash FROM 14 FOR 3) || '-8' || substring(hash FROM 18 FOR 3) || '-' || substring(hash FROM 21 FOR 12) AS id FROM source) INSERT INTO conversation_clusters (id, tenant_id, principal_id, created_at, updated_at) SELECT id, $1, $2, $3 + value, $3 + value FROM rows",
    )
    .bind(key.tenant_id.to_string())
    .bind(key.principal_id.to_string())
    .bind(base + 200_000)
    .bind(&seed)
    .execute(&pool)
    .await
    .expect("PostgreSQL recent clusters");
    sqlx::query(
        "WITH source AS (SELECT value, md5('recent-session-' || $3 || value::TEXT) AS hash FROM generate_series(1, 51) value), rows AS (SELECT value, substring(hash FROM 1 FOR 8) || '-' || substring(hash FROM 9 FOR 4) || '-7' || substring(hash FROM 14 FOR 3) || '-8' || substring(hash FROM 18 FOR 3) || '-' || substring(hash FROM 21 FOR 12) AS id FROM source) INSERT INTO conversation_key_clusters (key_id, cluster_id, updated_at, request_count, candidate_edge_count) SELECT $1, id, $2 + value, 1, 0 FROM rows",
    )
    .bind(key.key_id.to_string())
    .bind(base + 200_000)
    .bind(&seed)
    .execute(&pool)
    .await
    .expect("PostgreSQL recent projections");
    analyze_session_sources(&pool).await;

    let before_plan = explain_candidate_first(&pool, key.tenant_id, key.key_id).await;
    let before_buffers = shared_buffers(&before_plan);

    insert_history(
        &pool,
        &key,
        historical_cluster_id,
        base,
        &seed,
        10_002,
        110_001,
    )
    .await;
    analyze_session_sources(&pool).await;

    let after_plan = explain_candidate_first(&pool, key.tenant_id, key.key_id).await;
    let after_buffers = shared_buffers(&after_plan);
    assert!(
        !after_plan.contains("Seq Scan on request_records"),
        "completed history must not be scanned by the first-page session query: {after_plan}"
    );
    assert!(
        after_buffers <= before_buffers + 128,
        "growing an excluded session from 10k to 110k requests must not grow first-page buffers proportionally (before={before_buffers}, after={after_buffers}): {after_plan}"
    );

    let filter = LogicalSessionListFilter {
        limit: 50,
        state: "all".into(),
        ..Default::default()
    };
    let candidate_first = state
        .db
        .self_recent_sessions(key.tenant_id, filter.clone())
        .await
        .expect("candidate-first PostgreSQL session page");
    let reference = state
        .db
        .recent_sessions_reference_for_test(&key.tenant_id.to_string(), filter)
        .await
        .expect("reference PostgreSQL session page");
    assert_eq!(candidate_first.len(), 51);
    assert_eq!(
        serde_json::to_value(&candidate_first).expect("candidate-first JSON"),
        serde_json::to_value(&reference).expect("reference JSON"),
        "the optimized first page must preserve every returned summary field"
    );
    assert!(
        candidate_first
            .iter()
            .all(|session| session.cluster_id != Some(historical_cluster_id)),
        "the older large-history session must fall outside the first candidate page"
    );

    let list_plan = explain_bound_query(
        &pool,
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT TEXT, TIMING OFF) SELECT cluster_id, updated_at, request_count FROM conversation_key_clusters WHERE key_id = $1 ORDER BY updated_at DESC, cluster_id DESC LIMIT 100",
        &[key.key_id.to_string()],
    )
    .await;
    assert!(list_plan.contains("Index"), "{list_plan}");
    assert!(
        !list_plan.contains("Seq Scan on conversation_key_clusters"),
        "{list_plan}"
    );
    let detail_plan = explain_bound_query(
        &pool,
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT TEXT, TIMING OFF) SELECT id, created_at FROM request_records WHERE key_id = $1 AND conversation_cluster_id = $2 ORDER BY created_at DESC, id DESC LIMIT 201",
        &[
            key.key_id.to_string(),
            historical_cluster_id.to_string(),
        ],
    )
    .await;
    assert!(detail_plan.contains("Index"), "{detail_plan}");
    assert!(
        !detail_plan.contains("Seq Scan on request_records"),
        "{detail_plan}"
    );

    // Preserve the existing public conversation list/detail scale contract in
    // this single large fixture instead of maintaining a second 110k test.
    let list_response = api::router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/self/v1/conversations?limit=999")
                .header(header::AUTHORIZATION, format!("Bearer {}", issued.key))
                .body(Body::empty())
                .expect("PostgreSQL list request"),
        )
        .await
        .expect("PostgreSQL list response");
    assert_eq!(list_response.status(), StatusCode::OK);
    let list_body: Value = serde_json::from_slice(
        &to_bytes(list_response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("bounded PostgreSQL list response"),
    )
    .expect("PostgreSQL list JSON");
    assert_eq!(list_body.as_array().map(Vec::len), Some(52));

    let detail_response = api::router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/self/v1/conversations/{historical_cluster_id}?limit=999"
                ))
                .header(header::AUTHORIZATION, format!("Bearer {}", issued.key))
                .body(Body::empty())
                .expect("PostgreSQL detail request"),
        )
        .await
        .expect("PostgreSQL detail response");
    assert_eq!(detail_response.status(), StatusCode::OK);
    let detail_body: Value = serde_json::from_slice(
        &to_bytes(detail_response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("bounded PostgreSQL detail response"),
    )
    .expect("PostgreSQL detail JSON");
    assert_eq!(detail_body["cluster"]["request_count"], 110_001);
    assert_eq!(detail_body["requests"].as_array().map(Vec::len), Some(200));
    assert_eq!(detail_body["has_more"], true);
}

async fn insert_history(
    pool: &AnyPool,
    key: &AuthenticatedKey,
    cluster_id: Uuid,
    base: i64,
    seed: &str,
    first: i64,
    last: i64,
) {
    sqlx::query(
        "WITH source AS (SELECT value, md5($7 || value::TEXT) AS hash FROM generate_series($5, $6) value), rows AS (SELECT value, substring(hash FROM 1 FOR 8) || '-' || substring(hash FROM 9 FOR 4) || '-7' || substring(hash FROM 14 FOR 3) || '-8' || substring(hash FROM 18 FOR 3) || '-' || substring(hash FROM 21 FOR 12) AS id FROM source) INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, request_object, response_object, reservation_id, conversation_cluster_id) SELECT id, $1, $2, $3 + value, 'openai-responses', 'gpt-scale', 200, 1, 1, 1, 1, 'memory://request', 'memory://response', id, $4 FROM rows",
    )
    .bind(key.tenant_id.to_string())
    .bind(key.key_id.to_string())
    .bind(base)
    .bind(cluster_id.to_string())
    .bind(first)
    .bind(last)
    .bind(seed)
    .execute(pool)
    .await
    .expect("PostgreSQL request history range");
    sqlx::query(
        "WITH source AS (SELECT value, md5($6 || value::TEXT) AS request_hash, md5('observation-' || $6 || value::TEXT) AS observation_hash FROM generate_series($4, $5) value), rows AS (SELECT value, substring(request_hash FROM 1 FOR 8) || '-' || substring(request_hash FROM 9 FOR 4) || '-7' || substring(request_hash FROM 14 FOR 3) || '-8' || substring(request_hash FROM 18 FOR 3) || '-' || substring(request_hash FROM 21 FOR 12) AS request_id, substring(observation_hash FROM 1 FOR 8) || '-' || substring(observation_hash FROM 9 FOR 4) || '-7' || substring(observation_hash FROM 14 FOR 3) || '-8' || substring(observation_hash FROM 18 FOR 3) || '-' || substring(observation_hash FROM 21 FOR 12) AS observation_id FROM source) INSERT INTO conversation_observations (id, cluster_id, request_id, key_id, atom_hashes_json, client_name, created_at, inference_version, compaction) SELECT observation_id, $1, request_id, $2, '[]', 'Codex', $3 + value, 2, 0 FROM rows",
    )
    .bind(cluster_id.to_string())
    .bind(key.key_id.to_string())
    .bind(base)
    .bind(first)
    .bind(last)
    .bind(seed)
    .execute(pool)
    .await
    .expect("PostgreSQL observation history range");
}

async fn analyze_session_sources(pool: &AnyPool) {
    for table in [
        "conversation_key_clusters",
        "request_records",
        "session_usage_totals",
        "session_archive_totals",
    ] {
        let statement = format!("ANALYZE {table}");
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(pool)
            .await
            .expect("analyze PostgreSQL session source");
    }
}

async fn explain_candidate_first(pool: &AnyPool, tenant_id: Uuid, key_id: Uuid) -> String {
    let statement = format!(
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT TEXT, TIMING OFF) {}",
        super::super::session_analytics::RECENT_SESSIONS_FIRST_PAGE_SQL
    );
    sqlx::query(sqlx::AssertSqlSafe(statement))
        .bind(tenant_id.to_string())
        .bind(key_id.to_string())
        .bind(51_i64)
        .fetch_all(pool)
        .await
        .expect("explain actual candidate-first session query")
        .into_iter()
        .map(|row| row.get::<String, _>(0))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn explain_bound_query(pool: &AnyPool, statement: &'static str, binds: &[String]) -> String {
    let mut query = sqlx::query(statement);
    for value in binds {
        query = query.bind(value);
    }
    query
        .fetch_all(pool)
        .await
        .expect("explain bounded PostgreSQL query")
        .into_iter()
        .map(|row| row.get::<String, _>(0))
        .collect::<Vec<_>>()
        .join("\n")
}

fn shared_buffers(plan: &str) -> u64 {
    let root_buffers = plan
        .lines()
        .find(|line| line.trim_start().starts_with("Buffers: shared"))
        .unwrap_or_else(|| panic!("root shared buffers missing from plan: {plan}"));
    let blocks = root_buffers
        .split_whitespace()
        .filter_map(|field| {
            field
                .strip_prefix("hit=")
                .or_else(|| field.strip_prefix("read="))
                .and_then(|value| value.parse::<u64>().ok())
        })
        .sum();
    assert!(blocks > 0, "shared buffer count missing from plan: {plan}");
    blocks
}
