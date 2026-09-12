use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::{AnyPool, PgPool, Row};
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
    let tenant_external_id = format!("session-candidate-pg-{unique}");
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant_external_id.clone(),
                principal_external_id: "postgres-scale-a".into(),
                alias: "PostgreSQL scale A".into(),
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
    let second_issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant_external_id.clone(),
                principal_external_id: "postgres-scale-b".into(),
                alias: "PostgreSQL scale B".into(),
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
        .expect("create second PostgreSQL key");
    let second_key = state
        .db
        .authenticate_key(&second_issued.key, PEPPER)
        .await
        .expect("authenticate second PostgreSQL key");
    sqlx::any::install_default_drivers();
    let pool = AnyPool::connect(&database_url)
        .await
        .expect("connect PostgreSQL pool");
    let plan_pool = PgPool::connect(&database_url)
        .await
        .expect("connect PostgreSQL plan pool");
    let historical_cluster_id = Uuid::now_v7();
    let base = unix_millis();
    let historical_base = base - 3 * 24 * 60 * 60 * 1_000;
    let seed = unique.to_string();

    sqlx::query(
        "INSERT INTO conversation_clusters (id, tenant_id, principal_id, explicit_session_id, created_at, updated_at) VALUES ($1, $2, $3, 'postgres-large-session', $4, $5)",
    )
    .bind(historical_cluster_id.to_string())
    .bind(key.tenant_id.to_string())
    .bind(key.principal_id.to_string())
    .bind(historical_base)
    .bind(historical_base + 10_001)
    .execute(&pool)
    .await
    .expect("PostgreSQL historical cluster");
    insert_history(
        &pool,
        &key,
        historical_cluster_id,
        historical_base,
        &seed,
        1,
        1_001,
    )
    .await;
    sqlx::query(
        "INSERT INTO conversation_key_clusters (key_id, cluster_id, explicit_session_id, updated_at, request_count, candidate_edge_count) VALUES ($1, $2, 'postgres-large-session', $3, 10001, 0)",
    )
    .bind(key.key_id.to_string())
    .bind(historical_cluster_id.to_string())
    .bind(historical_base + 10_001)
    .execute(&pool)
    .await
    .expect("PostgreSQL historical projection");

    let completed_session = Uuid::now_v7();
    let active_session = Uuid::now_v7();
    let archive_session = Uuid::now_v7();
    let shared_session = Uuid::now_v7();
    let unlinked_session = format!("unlinked:{}", second_key.key_id);
    let completed_request = Uuid::now_v7();
    let active_request = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO session_usage_totals (tenant_id, key_id, session_id, currency, last_activity_at, requests, errors, input_tokens, output_tokens, duration_count, duration_sum_ms, cost_micros) VALUES ($1,$2,$3,'USD',$4,2,1,20,10,2,40,100), ($1,$2,$3,'EUR',$4 - 1,1,0,3,4,1,5,200)",
    )
    .bind(key.tenant_id.to_string())
    .bind(key.key_id.to_string())
    .bind(completed_session.to_string())
    .bind(base + 300)
    .execute(&pool)
    .await
    .expect("completed multi-currency session totals");
    sqlx::query(
        "INSERT INTO session_usage_totals (tenant_id,key_id,session_id,currency,last_activity_at,requests,errors,input_tokens,output_tokens,duration_count,duration_sum_ms,cost_micros) VALUES ($1,$2,$3,'USD',$4,1,0,5,6,1,9,25)",
    )
    .bind(key.tenant_id.to_string())
    .bind(second_key.key_id.to_string())
    .bind(&unlinked_session)
    .bind(base + 250)
    .execute(&pool)
    .await
    .expect("completed unlinked session totals");
    sqlx::query(
        "INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, request_object, response_object, reservation_id, conversation_cluster_id) VALUES ($1,$2,$3,$4,'openai-responses','gpt-completed',500,12,20,10,100,'memory://request','memory://response',$5,$6), ($7,$2,$8,$9,'anthropic-messages','claude-active',NULL,NULL,0,0,0,'memory://request',NULL,$10,$11)",
    )
    .bind(completed_request.to_string())
    .bind(key.tenant_id.to_string())
    .bind(key.key_id.to_string())
    .bind(base + 300)
    .bind(Uuid::now_v7().to_string())
    .bind(completed_session.to_string())
    .bind(active_request.to_string())
    .bind(second_key.key_id.to_string())
    .bind(base + 200)
    .bind(Uuid::now_v7().to_string())
    .bind(active_session.to_string())
    .execute(&pool)
    .await
    .expect("live completed and active session metadata");
    sqlx::query(
        "INSERT INTO request_records (id,tenant_id,key_id,created_at,protocol,model,status_code,duration_ms,input_tokens,output_tokens,cost_micros,request_object,response_object,reservation_id) VALUES ($1,$2,$3,$4,'openai-responses','gpt-unlinked',200,9,5,6,25,'memory://request','memory://response',$5)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(key.tenant_id.to_string())
    .bind(second_key.key_id.to_string())
    .bind(base + 250)
    .bind(Uuid::now_v7().to_string())
    .execute(&pool)
    .await
    .expect("completed unlinked session metadata");
    sqlx::query(
        "INSERT INTO session_archive_totals (tenant_id,key_id,session_id,last_activity_at,requests,errors,input_tokens,output_tokens,duration_count,duration_sum_ms) VALUES ($1,$2,$3,$4,2,1,7,9,2,30)",
    )
    .bind(key.tenant_id.to_string())
    .bind(key.key_id.to_string())
    .bind(archive_session.to_string())
    .bind(base + 100)
    .execute(&pool)
    .await
    .expect("archive session totals");
    sqlx::query(
        "INSERT INTO session_archive_unlinked_requests (tenant_id,source,external_request_id,archive_request_id,key_id,principal_id,conversation_cluster_id,source_started_at,source_completed_at,protocol,model,status_code,duration_ms,input_tokens,output_tokens,error_code,imported_at) VALUES ($1,'fixture',$2,$3,$4,$5,$6,$7,$7,'openai-responses','gpt-archive',429,15,7,9,'rate_limit',$7)",
    )
    .bind(key.tenant_id.to_string())
    .bind(format!("archive-{unique}"))
    .bind(Uuid::now_v7().to_string())
    .bind(key.key_id.to_string())
    .bind(key.principal_id.to_string())
    .bind(archive_session.to_string())
    .bind(base + 100)
    .execute(&pool)
    .await
    .expect("archive session metadata");
    sqlx::query(
        "INSERT INTO conversation_key_clusters (key_id,cluster_id,updated_at,request_count,candidate_edge_count) VALUES ($1,$3,$4,1,0), ($2,$3,$4,1,0)",
    )
    .bind(key.key_id.to_string())
    .bind(second_key.key_id.to_string())
    .bind(shared_session.to_string())
    .bind(base)
    .execute(&pool)
    .await
    .expect("same-time same-session cross-key projections");
    analyze_session_sources(&pool).await;

    let before_plan = explain_candidate_first(&plan_pool, key.tenant_id, "", 6).await;
    let before_buffers = shared_buffers(&before_plan);

    insert_history(
        &pool,
        &key,
        historical_cluster_id,
        historical_base,
        &seed,
        1_002,
        10_001,
    )
    .await;
    analyze_session_sources(&pool).await;

    let history_partitions = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT tableoid::regclass::TEXT FROM request_records WHERE tenant_id = $1 AND key_id = $2 AND conversation_cluster_id = $3",
    )
    .bind(key.tenant_id.to_string())
    .bind(key.key_id.to_string())
    .bind(historical_cluster_id.to_string())
    .fetch_all(&pool)
    .await
    .expect("historical request partitions");
    assert_session_latest_indexes(&plan_pool, &history_partitions).await;
    let after_plan = explain_candidate_first(&plan_pool, key.tenant_id, "", 6).await;
    let after_buffers = shared_buffers(&after_plan);
    assert_relations_returned_no_rows(&after_plan, &history_partitions);
    assert!(
        after_buffers <= before_buffers + 128,
        "growing an excluded session from 1k to 10k requests must not grow first-page buffers proportionally (before={before_buffers}, after={after_buffers}): {after_plan}"
    );

    let filter = LogicalSessionListFilter {
        limit: 5,
        state: "all".into(),
        ..Default::default()
    };
    let candidate_first = state
        .db
        .operator_recent_sessions(&tenant_external_id, filter.clone())
        .await
        .expect("candidate-first PostgreSQL session page");
    let reference = state
        .db
        .recent_sessions_reference_for_test(&key.tenant_id.to_string(), filter)
        .await
        .expect("reference PostgreSQL session page");
    assert_eq!(candidate_first.len(), 6);
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

    let completed = candidate_first
        .iter()
        .find(|session| session.cluster_id == Some(completed_session))
        .expect("completed mixed-source summary");
    assert_eq!(completed.model, "gpt-completed");
    assert_eq!(completed.protocol, "openai-responses");
    assert_eq!(completed.last_status, "error");
    assert_eq!(completed.active_requests, 0);
    assert_eq!(completed.requests, 3);
    assert_eq!(completed.errors, 1);
    assert_eq!(completed.input_tokens, 23);
    assert_eq!(completed.output_tokens, 14);
    assert_eq!(completed.avg_duration_ms, Some(15.0));
    assert_eq!(
        serde_json::to_value(&completed.costs).expect("multi-currency costs"),
        serde_json::json!([
            {"currency": "EUR", "cost": "0.0002"},
            {"currency": "USD", "cost": "0.0001"}
        ])
    );

    let active = candidate_first
        .iter()
        .find(|session| session.cluster_id == Some(active_session))
        .expect("active mixed-source summary");
    assert_eq!(active.model, "claude-active");
    assert_eq!(active.protocol, "anthropic-messages");
    assert_eq!(active.last_status, "active");
    assert_eq!(active.active_requests, 1);
    assert_eq!(active.requests, 0);

    let unlinked = candidate_first
        .iter()
        .find(|session| session.session_id == unlinked_session)
        .expect("completed unlinked summary");
    assert!(unlinked.unlinked);
    assert_eq!(unlinked.cluster_id, None);
    assert_eq!(unlinked.model, "gpt-unlinked");
    assert_eq!(unlinked.last_status, "success");
    assert_eq!(unlinked.requests, 1);

    let archived = candidate_first
        .iter()
        .find(|session| session.cluster_id == Some(archive_session))
        .expect("archive mixed-source summary");
    assert_eq!(archived.model, "gpt-archive");
    assert_eq!(archived.last_status, "error");
    assert_eq!(archived.archived_only_requests, 2);
    assert_eq!(archived.archived_only_errors, 1);
    assert_eq!(archived.archived_only_input_tokens, 7);
    assert_eq!(archived.archived_only_output_tokens, 9);
    assert_eq!(archived.archived_only_avg_duration_ms, Some(15.0));

    let mut shared_keys = [key.key_id, second_key.key_id];
    shared_keys.sort_by_key(ToString::to_string);
    shared_keys.reverse();
    assert_eq!(candidate_first[4].cluster_id, Some(shared_session));
    assert_eq!(candidate_first[4].last_activity_at, base);
    assert_eq!(candidate_first[4].key_id, shared_keys[0]);
    assert_eq!(candidate_first[5].cluster_id, Some(shared_session));
    assert_eq!(candidate_first[5].last_activity_at, base);
    assert_eq!(candidate_first[5].key_id, shared_keys[1]);

    let page_one = &candidate_first[..5];
    let cursor = page_one.last().expect("visible first-page boundary");
    let page_two = state
        .db
        .operator_recent_sessions(
            &tenant_external_id,
            LogicalSessionListFilter {
                limit: 5,
                cursor: Some((
                    cursor.last_activity_at,
                    cursor.session_id.clone(),
                    cursor.key_id.to_string(),
                )),
                state: "all".into(),
                ..Default::default()
            },
        )
        .await
        .expect("full three-field cursor page");
    assert_eq!(page_two.len(), 2);
    assert_eq!(page_two[0].cluster_id, Some(shared_session));
    assert_eq!(page_two[0].key_id, shared_keys[1]);
    assert_eq!(page_two[1].cluster_id, Some(historical_cluster_id));
    let identities = page_one
        .iter()
        .chain(&page_two)
        .map(|session| (session.session_id.clone(), session.key_id))
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        identities.len(),
        7,
        "full cursor must neither lose nor repeat rows"
    );

    let legacy_error = state
        .db
        .operator_recent_sessions(
            &tenant_external_id,
            LogicalSessionListFilter {
                limit: 4,
                cursor: Some((base, shared_session.to_string(), String::new())),
                legacy_cursor: true,
                state: "all".into(),
                ..Default::default()
            },
        )
        .await
        .expect_err("unscoped legacy cursor must fail closed");
    assert!(matches!(
        legacy_error,
        AppError::BadRequest(message)
            if message == "legacy session cursor requires key_id; refresh and use the returned three-field cursor"
    ));
}

#[tokio::test]
async fn postgres_public_session_and_conversation_lists_cap_at_100() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!("MTC_TEST_POSTGRES_URL is unset; skipping PostgreSQL list cap contract");
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
                tenant_external_id: format!("session-cap-pg-{unique}"),
                principal_external_id: "postgres-cap".into(),
                alias: "PostgreSQL cap".into(),
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
        .expect("create PostgreSQL cap key");
    let key = state
        .db
        .authenticate_key(&issued.key, PEPPER)
        .await
        .expect("authenticate PostgreSQL cap key");
    sqlx::any::install_default_drivers();
    let pool = AnyPool::connect(&database_url)
        .await
        .expect("connect PostgreSQL cap pool");
    let base = unix_millis();
    let seed = unique.to_string();
    sqlx::query(
        "WITH source AS (SELECT value, md5($4 || value::TEXT) AS hash FROM generate_series(1, 101) value), rows AS (SELECT value, substring(hash FROM 1 FOR 8) || '-' || substring(hash FROM 9 FOR 4) || '-7' || substring(hash FROM 14 FOR 3) || '-8' || substring(hash FROM 18 FOR 3) || '-' || substring(hash FROM 21 FOR 12) AS id FROM source) INSERT INTO conversation_clusters (id, tenant_id, principal_id, created_at, updated_at) SELECT id, $1, $2, $3 + value, $3 + value FROM rows",
    )
    .bind(key.tenant_id.to_string())
    .bind(key.principal_id.to_string())
    .bind(base)
    .bind(&seed)
    .execute(&pool)
    .await
    .expect("PostgreSQL capped-list clusters");
    sqlx::query(
        "WITH source AS (SELECT value, md5($3 || value::TEXT) AS hash FROM generate_series(1, 101) value), rows AS (SELECT value, substring(hash FROM 1 FOR 8) || '-' || substring(hash FROM 9 FOR 4) || '-7' || substring(hash FROM 14 FOR 3) || '-8' || substring(hash FROM 18 FOR 3) || '-' || substring(hash FROM 21 FOR 12) AS id FROM source) INSERT INTO conversation_key_clusters (key_id, cluster_id, updated_at, request_count, candidate_edge_count) SELECT $1, id, $2 + value, 1, 0 FROM rows",
    )
    .bind(key.key_id.to_string())
    .bind(base)
    .bind(&seed)
    .execute(&pool)
    .await
    .expect("PostgreSQL capped-list projections");

    let conversations = api::router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/self/v1/conversations?limit=999")
                .header(header::AUTHORIZATION, format!("Bearer {}", issued.key))
                .body(Body::empty())
                .expect("PostgreSQL conversation list request"),
        )
        .await
        .expect("PostgreSQL conversation list response");
    assert_eq!(conversations.status(), StatusCode::OK);
    let conversations: Value = serde_json::from_slice(
        &to_bytes(conversations.into_body(), 1024 * 1024)
            .await
            .expect("bounded PostgreSQL conversation list body"),
    )
    .expect("PostgreSQL conversation list JSON");
    assert_eq!(conversations.as_array().map(Vec::len), Some(100));

    let sessions = api::router(state)
        .oneshot(
            Request::builder()
                .uri("/self/v1/sessions?limit=999")
                .header(header::AUTHORIZATION, format!("Bearer {}", issued.key))
                .body(Body::empty())
                .expect("PostgreSQL session list request"),
        )
        .await
        .expect("PostgreSQL session list response");
    assert_eq!(sessions.status(), StatusCode::OK);
    let sessions: Value = serde_json::from_slice(
        &to_bytes(sessions.into_body(), 1024 * 1024)
            .await
            .expect("bounded PostgreSQL session list body"),
    )
    .expect("PostgreSQL session list JSON");
    assert_eq!(sessions["sessions"].as_array().map(Vec::len), Some(100));
    assert!(sessions["next_cursor"]["before_key_id"].is_string());
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

async fn explain_candidate_first(
    pool: &PgPool,
    tenant_id: Uuid,
    key_id: &str,
    limit: i64,
) -> Value {
    let statement = format!(
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, TIMING OFF) {}",
        super::super::session_analytics::RECENT_SESSIONS_FIRST_PAGE_SQL
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(statement))
        .bind(tenant_id.to_string())
        .bind(key_id)
        .bind(limit)
        .fetch_one(pool)
        .await
        .expect("explain actual candidate-first session query");
    let raw = row
        .try_get_unchecked::<String, _>(0)
        .expect("PostgreSQL JSON plan text");
    serde_json::from_str(&raw).expect("structured PostgreSQL JSON plan")
}

fn plan_root(plan: &Value) -> &Value {
    plan.as_array()
        .and_then(|entries| entries.first())
        .and_then(|entry| entry.get("Plan"))
        .unwrap_or_else(|| panic!("PostgreSQL JSON plan root missing: {plan}"))
}

fn shared_buffers(plan: &Value) -> u64 {
    let root = plan_root(plan);
    let blocks = ["Shared Hit Blocks", "Shared Read Blocks"]
        .into_iter()
        .map(|field| root.get(field).and_then(Value::as_u64).unwrap_or(0))
        .sum();
    assert!(blocks > 0, "shared buffer count missing from plan: {plan}");
    blocks
}

fn assert_relations_returned_no_rows(plan: &Value, relation_names: &[String]) {
    fn visit(node: &Value, relation_names: &[String]) {
        if node
            .get("Relation Name")
            .and_then(Value::as_str)
            .is_some_and(|name| relation_names.iter().any(|candidate| candidate == name))
        {
            assert_eq!(
                node.get("Actual Rows").and_then(Value::as_u64),
                Some(0),
                "an excluded historical request partition returned rows: {node}"
            );
            assert_eq!(
                node.get("Rows Removed by Filter")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                0,
                "an excluded historical request partition scanned filtered rows: {node}"
            );
            assert_eq!(
                node.get("Rows Removed by Index Recheck")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                0,
                "an excluded historical request partition scanned index rows: {node}"
            );
        }
        if let Some(children) = node.get("Plans").and_then(Value::as_array) {
            for child in children {
                visit(child, relation_names);
            }
        }
    }

    assert!(!relation_names.is_empty(), "historical partition set");
    visit(plan_root(plan), relation_names);
}

async fn assert_session_latest_indexes(pool: &PgPool, relation_names: &[String]) {
    for relation_name in relation_names {
        let attached = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(DISTINCT parent_index.relname)
                   FROM pg_inherits attachment
                   JOIN pg_class parent_index
                     ON parent_index.oid = attachment.inhparent
                   JOIN pg_index child_index
                     ON child_index.indexrelid = attachment.inhrelid
                  WHERE child_index.indrelid = to_regclass($1)
                    AND child_index.indisvalid
                    AND child_index.indisready
                    AND parent_index.relname IN (
                        'request_records_session_latest_idx',
                        'request_records_unlinked_latest_idx'
                    )",
        )
        .bind(relation_name)
        .fetch_one(pool)
        .await
        .expect("inspect latest-session partition index");
        assert!(
            attached == 2,
            "historical partition lacks attached latest-session indexes: {relation_name}"
        );
    }
}
