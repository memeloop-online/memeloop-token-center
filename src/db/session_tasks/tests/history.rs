use super::*;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;

async fn credential(
    state: &crate::AppState,
    tenant: &str,
) -> (crate::model::AuthenticatedKey, String) {
    let issued = state
        .db
        .create_key(
            crate::db::CreateKeyInput {
                tenant_external_id: tenant.into(),
                principal_external_id: Uuid::now_v7().to_string(),
                alias: "session history acceptance".into(),
                currency: "USD".into(),
                policy: crate::model::KeyPolicy::default(),
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let authenticated = state
        .db
        .authenticate_key(&issued.key, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    (authenticated, issued.key)
}

async fn native(
    db: &Database,
    owner: &crate::model::AuthenticatedKey,
    identity: Option<&SessionIdentity>,
    timestamp: i64,
    currency: &str,
) -> Uuid {
    let request_id = Uuid::now_v7();
    let session_id = identity.map(|identity| identity.session_id.to_string());
    sqlx::query("INSERT INTO request_records (id,tenant_id,key_id,created_at,protocol,model,currency,request_object,reservation_id,conversation_cluster_id) VALUES ($1,$2,$3,$4,'openai','session-history',$5,'gap://fixture/request',$6,$7)")
        .bind(request_id.to_string()).bind(owner.tenant_id.to_string()).bind(owner.key_id.to_string())
        .bind(timestamp).bind(currency).bind(Uuid::now_v7().to_string()).bind(&session_id)
        .execute(&db.pool).await.unwrap();
    sqlx::query(
        "INSERT INTO request_record_locators (id,created_at,tenant_id,key_id) VALUES ($1,$2,$3,$4)",
    )
    .bind(request_id.to_string())
    .bind(timestamp)
    .bind(owner.tenant_id.to_string())
    .bind(owner.key_id.to_string())
    .execute(&db.pool)
    .await
    .unwrap();
    db.record_request_finished(crate::db::FinishRequest {
        request_id,
        first_output_ms: None,
        generation_duration_ms: None,
        status_code: 200,
        duration_ms: 17,
        input_tokens: 10,
        cached_input_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 20,
        service_tier: None,
        cost_micros: 7,
        error_code: None,
        response_object: "gap://fixture/response".into(),
    })
    .await
    .unwrap();
    request_id
}

async fn post(app: &axum::Router, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

#[tokio::test]
#[ignore = "existing CI supplies real PostgreSQL and ClickHouse services"]
async fn session_tasks_history_real_services_acceptance() {
    let postgres = std::env::var("MTC_TEST_POSTGRES_URL").unwrap();
    let endpoint = std::env::var("MTC_TEST_CLICKHOUSE_URL").unwrap();
    let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
    let schema = format!("session_history_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&postgres).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let state = crate::AppState::initialize(crate::config::Config::for_test(isolated.to_string()))
        .await
        .unwrap();
    let db = &state.db;
    let tenant = format!("session-history-{}", Uuid::now_v7());
    let (owner, token) = credential(&state, &tenant).await;
    let (sibling_owner, _) = credential(&state, &tenant).await;
    let (foreign_owner, _) = credential(&state, &format!("{tenant}-foreign")).await;
    let thread = Uuid::now_v7();
    let child_thread = Uuid::now_v7();
    let root = session(db, &owner, thread).await;
    let child = session(db, &sibling_owner, child_thread).await;
    let unrelated = session(db, &owner, Uuid::now_v7()).await;
    let foreign = session(db, &foreign_owner, Uuid::now_v7()).await;
    db.import_session_parents(
        owner.tenant_id,
        &[ThreadParent {
            child_thread_id: child_thread,
            parent_thread_id: thread,
            evidence: "thread_spawn_edges".into(),
        }],
    )
    .await
    .unwrap();
    let revision = db
        .session_title(owner.tenant_id, owner.key_id, root.session_id)
        .await
        .unwrap()
        .revision;
    db.update_session_name(
        owner.tenant_id,
        &root,
        SessionNameUpdate {
            title: "真实 Codex 任务".into(),
            source: "codex".into(),
            explicit_session_id: Some(thread.to_string()),
            expected_revision: revision,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    db.history_start().await.unwrap();
    let mut root_requests = Vec::new();
    for timestamp in [1_772_944_200_000, 1_772_947_800_000, 1_773_030_600_000] {
        root_requests.push(native(db, &owner, Some(&root), timestamp, "USD").await);
    }
    native(db, &sibling_owner, Some(&child), 1_773_027_000_000, "EUR").await;
    native(db, &owner, Some(&unrelated), 1_773_027_000_000, "USD").await;
    native(db, &owner, None, 1_773_027_000_000, "USD").await;
    native(db, &foreign_owner, Some(&foreign), 1_773_027_000_000, "USD").await;
    let archive_id = Uuid::now_v7();
    sqlx::query("INSERT INTO session_archive_correlations (tenant_id,source,external_request_id,disposition,key_id,principal_id,record_digest,proof_digest,identity_proof_kind,identity_proof_digest,source_model,source_started_at,correlated_at) VALUES ($1,'session-history-fixture',$2,'unlinked',$3,$4,$5,$6,'credential',$7,'session-history',$8,1)")
        .bind(owner.tenant_id.to_string()).bind(archive_id.to_string())
        .bind(sibling_owner.key_id.to_string()).bind(sibling_owner.principal_id.to_string())
        .bind("a".repeat(64)).bind("b".repeat(64)).bind("c".repeat(64))
        .bind(1_773_030_600_000_i64).execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO session_archive_unlinked_requests (tenant_id,source,external_request_id,archive_request_id,key_id,principal_id,conversation_cluster_id,source_started_at,protocol,model,input_tokens,output_tokens,imported_at) VALUES ($1,'session-history-fixture',$2,$2,$3,$4,$5,$6,'openai','session-history',3,4,1)")
        .bind(owner.tenant_id.to_string()).bind(archive_id.to_string()).bind(sibling_owner.key_id.to_string())
        .bind(sibling_owner.principal_id.to_string()).bind(child.session_id.to_string())
        .bind(1_773_030_600_000_i64).execute(&db.pool).await.unwrap();
    let app = crate::api::router(state.clone());
    let mut cases = Vec::new();
    for (scope, identities, include_subtrees) in [
        ("session", vec![root.clone()], false),
        ("subtree", vec![root.clone()], false),
        (
            "related",
            vec![root.clone(), child.clone(), root.clone()],
            true,
        ),
    ] {
        for (zone, offset) in [("America/New_York", None), ("UTC", Some(-300))] {
            let request = json!({
                "tenant_external_id": tenant, "identities": identities, "scope": scope,
                "include_subtrees": include_subtrees, "timezone": zone,
                "timezone_offset_minutes": offset
            });
            let (status, expected) = post(
                &app,
                "/internal/v1/sessions/analyze",
                &state.config.service_token,
                request.clone(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{expected}");
            assert_eq!(
                expected["totals"]["requests"],
                if scope == "session" { 3 } else { 5 }
            );
            if scope != "session" {
                assert_eq!(expected["totals"]["costs"]["USD"], 21);
                assert_eq!(expected["totals"]["costs"]["EUR"], 7);
                assert_eq!(expected["totals"]["unknown_cost_requests"], 1);
            }
            if zone == "America/New_York" {
                assert_eq!(expected["active_days"], 3);
                assert_eq!(
                    expected["daily"][2]["day_start_at"].as_i64().unwrap()
                        - expected["daily"][1]["day_start_at"].as_i64().unwrap(),
                    23 * 3_600_000
                );
            }
            cases.push((request, expected));
        }
    }
    let client = crate::request_history::ClickHouse::new(
        db.http_clients
            .handle(crate::http_settings::HttpPurpose::General),
        &endpoint,
        std::env::var("MTC_REQUEST_HISTORY_DATABASE").unwrap(),
        "default".into(),
        String::new(),
    )
    .unwrap();
    client.initialize().await.unwrap();
    while !db.history_backfill_done().await.unwrap() {
        db.history_backfill_page(2).await.unwrap();
        crate::request_history::migration::drain(db, &client, 2)
            .await
            .unwrap();
    }
    crate::request_history::migration::verify(db, &client)
        .await
        .unwrap();
    db.history_switch(true).await.unwrap();
    for (request, expected) in &cases {
        let (status, actual) = post(
            &app,
            "/internal/v1/sessions/analyze",
            &state.config.service_token,
            request.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{actual}");
        assert_eq!(&actual, expected, "{request}");
    }
    let (status, resolved) = post(
        &app,
        "/internal/v1/sessions/resolve",
        &state.config.service_token,
        json!({"tenant_external_id":tenant,"explicit_session_ids":[thread]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resolved}");
    assert_eq!(
        resolved["sessions"][0]["session_id"],
        json!(root.session_id)
    );
    assert_ne!(root.session_id, thread);
    let self_request = json!({"identities":[root],"scope":"subtree"});
    let (status, own) = post(&app, "/self/v1/sessions/analyze", &token, self_request).await;
    assert_eq!(status, StatusCode::OK, "{own}");
    assert_eq!(own["totals"]["requests"], 3);
    assert_eq!(own["identities"].as_array().unwrap().len(), 1);
    for identity in [&child, &foreign] {
        let (status, _) = post(
            &app,
            "/self/v1/sessions/analyze",
            &token,
            json!({"identities":[identity],"scope":"session"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let (status, _) = post(
        &app,
        "/internal/v1/sessions/analyze",
        &state.config.service_token,
        json!({"tenant_external_id":tenant,"identities":[foreign],"scope":"session"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = post(
        &app,
        "/internal/v1/sessions/analyze",
        &state.config.service_token,
        json!({"tenant_external_id":tenant,"identities":[{"key_id":owner.key_id,"session_id":format!("unlinked:{}",owner.key_id)}],"scope":"session"}),
    )
    .await;
    assert!(status.is_client_error());
    sqlx::query("UPDATE request_stats_facts SET input_tokens=9999 WHERE request_id=$1")
        .bind(root_requests[0].to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    let (status, frozen) = post(
        &app,
        "/internal/v1/sessions/analyze",
        &state.config.service_token,
        cases[0].0.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{frozen}");
    assert_eq!(
        frozen, cases[0].1,
        "selected history reader must not fall back to CNPG facts"
    );
    crate::request_history::migration::drain(db, &client, 2)
        .await
        .unwrap();
    let (status, corrected) = post(
        &app,
        "/internal/v1/sessions/analyze",
        &state.config.service_token,
        cases[0].0.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{corrected}");
    assert_eq!(corrected["totals"]["input_tokens"], 10019);
    db.close().await;
    drop(app);
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}
