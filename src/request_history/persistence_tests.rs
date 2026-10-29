use super::*;
use crate::request_history::{ClickHouse, HistoryQuery};

async fn seed(database: &Database) -> (AuthenticatedKey, Uuid) {
    let pepper = b"request history synthetic fixture credential pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "history-tenant".into(),
                principal_external_id: "history-member".into(),
                alias: "history-key".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    database
        .record_request_started(NewRequest {
            request_id,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: "openai".into(),
            model: "history-model".into(),
            request_object: "gap://fixture/request".into(),
            reservation_id: Uuid::now_v7(),
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    (key, request_id)
}

async fn finish(database: &Database, request_id: Uuid) {
    database
        .record_request_finished(FinishRequest {
            request_id,
            first_output_ms: None,
            generation_duration_ms: None,
            status_code: 200,
            duration_ms: 20,
            input_tokens: 10,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 5,
            service_tier: None,
            cost_micros: 17,
            error_code: None,
            response_object: "gap://fixture/response".into(),
        })
        .await
        .unwrap();
}

async fn capture_replay(database: &Database) {
    database.migrate().await.unwrap();
    database.history_start().await.unwrap();
    let (_, request_id) = seed(database).await;
    let pending = database.history_pending(1).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].statistics.as_ref().unwrap().status, "pending");
    finish(database, request_id).await;
    database.history_acknowledge(&pending).await.unwrap();
    let corrected = database.history_pending(1).await.unwrap();
    assert_eq!(corrected.len(), 1);
    assert!(corrected[0].revision > pending[0].revision);
    assert_eq!(
        corrected[0].statistics.as_ref().unwrap().cost_micros,
        Some(17)
    );
    let replay = database.history_pending(1).await.unwrap();
    assert_eq!(
        serde_json::to_value(&corrected).unwrap(),
        serde_json::to_value(&replay).unwrap()
    );
    assert!(database.history_switch(true).await.is_err());
    database.history_acknowledge(&corrected).await.unwrap();
    assert_eq!(database.history_pending_count().await.unwrap(), 0);
}

#[tokio::test]
async fn sqlite_request_history_capture_freezes_retries_and_preserves_newer_corrections() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("history.db").display()
    ))
    .await
    .unwrap();
    capture_replay(&database).await;
}

#[tokio::test]
#[ignore = "existing CI supplies real PostgreSQL and ClickHouse services"]
async fn request_history_real_services_acceptance() {
    let postgres = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL acceptance URL");
    let endpoint = std::env::var("MTC_TEST_CLICKHOUSE_URL").expect("ClickHouse acceptance URL");
    let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
    let schema = format!("history_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&postgres).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect(isolated.as_str()).await.unwrap();
    database.migrate().await.unwrap();
    database.history_start().await.unwrap();
    let (key, request_id) = seed(&database).await;
    let client = ClickHouse::new(
        database
            .http_clients
            .handle(crate::http_settings::HttpPurpose::General),
        &endpoint,
        "default".into(),
        "default".into(),
        String::new(),
    )
    .unwrap();
    client.initialize().await.unwrap();
    let old = database.history_pending(1).await.unwrap();
    client.append(&old).await.unwrap();
    finish(&database, request_id).await;
    let latest = database.history_pending(1).await.unwrap();
    client.append(&latest).await.unwrap();
    client.append(&old).await.unwrap();
    client.append(&latest).await.unwrap();
    database.history_acknowledge(&old).await.unwrap();
    assert_eq!(database.history_pending_count().await.unwrap(), 1);
    let query = HistoryQuery {
        scope: HistoryScope::Key {
            tenant_id: key.tenant_id,
            key_id: key.key_id,
        },
        from_created_at: None,
        to_created_at: None,
        cursor: None,
        limit: 1,
        predicates: Vec::new(),
    };
    let page = client.list(&query).await.unwrap();
    assert_eq!(page.requests.len(), 1);
    assert!(page.requests[0]["credential_identity"].is_null());
    let stats = client.statistics(&query).await.unwrap();
    assert_eq!(stats.summary.total_requests, 1);
    assert_eq!(stats.summary.successful_requests, 1);
    assert_eq!(stats.summary.input_tokens, 10);
    assert_eq!(stats.summary.total_cost.as_deref(), Some("0.000017"));
    let other = HistoryQuery {
        scope: HistoryScope::Key {
            tenant_id: Uuid::new_v4(),
            key_id: key.key_id,
        },
        ..query.clone()
    };
    assert!(client.list(&other).await.unwrap().requests.is_empty());
    let empty = HistoryQuery {
        scope: HistoryScope::EmptyTenant,
        ..query.clone()
    };
    assert!(client.list(&empty).await.unwrap().requests.is_empty());
    let ledger_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    database.history_acknowledge(&latest).await.unwrap();
    while !database.history_backfill_done().await.unwrap() {
        database.history_backfill_page(1).await.unwrap();
    }
    crate::request_history::migration::drain(&database, &client, 1)
        .await
        .unwrap();
    let report = crate::request_history::migration::verify(&database, &client)
        .await
        .unwrap();
    assert_eq!(report["count"], 1);
    database.history_switch(true).await.unwrap();
    database.history_switch(false).await.unwrap();
    let ledger_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(ledger_before, ledger_after);
    database.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}
