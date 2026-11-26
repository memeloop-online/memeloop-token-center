use super::*;

async fn fixture_pool() -> AnyPool {
    sqlx::any::install_default_drivers();
    AnyPool::from(
        AnyPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap(),
    )
}

fn identity(source: &str) -> HistoryIdentity {
    HistoryIdentity {
        source: source.into(),
        request_id: Uuid::from_u128(1),
        tenant_id: Uuid::from_u128(2),
        key_id: Uuid::from_u128(3),
        created_at: 17,
    }
}

#[tokio::test]
async fn unsupported_sources_do_not_query_either_context() {
    let pool = fixture_pool().await;
    // Deliberately omit the history tables: a SQL read would fail.
    let unknown = identity("event");
    let completed = serde_json::json!({"completed_at": 18});
    let mut read = HistoryRead::Pool(&pool);
    assert_eq!(read.analysis_fact(&unknown).await.unwrap(), Value::Null);
    assert_eq!(read.statistics(&unknown, &completed).await.unwrap(), None);
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        Database::history_analysis_fact_in_transaction(&mut tx, &unknown)
            .await
            .unwrap(),
        Value::Null
    );
    assert_eq!(
        Database::history_statistics_in_transaction(&mut tx, &unknown, &completed)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn snapshot_reads_keep_native_generation_and_pending_contracts() {
    let pool = fixture_pool().await;
    for ddl in [
        "CREATE TABLE pending_proxy_settlements (request_id TEXT, state TEXT)",
        "CREATE TABLE terminal_projection_outbox (request_id TEXT, projected_at BIGINT)",
        "CREATE TABLE request_records (id TEXT, tenant_id TEXT, key_id TEXT, created_at BIGINT, completed_at BIGINT, protocol TEXT, input_tokens BIGINT, output_tokens BIGINT, cache_known_read_tokens BIGINT, cache_known_input_tokens BIGINT, cost_micros BIGINT, currency TEXT)",
        "CREATE TABLE request_stats_facts (request_id TEXT, tenant_id TEXT, key_id TEXT, created_at BIGINT, model TEXT, protocol TEXT, error_code TEXT, upstream_account_id TEXT, model_route_id TEXT, billing_unit TEXT, status_class TEXT, input_tokens BIGINT, output_tokens BIGINT, cached_input_tokens BIGINT, cache_write_tokens BIGINT, generation_units BIGINT, duration_ms BIGINT, cache_known_read_tokens BIGINT, cache_known_input_tokens BIGINT, cost_micros BIGINT, currency TEXT)",
        "CREATE TABLE generation_stats_facts (job_id TEXT, tenant_id TEXT, key_id TEXT, created_at BIGINT, model TEXT, error_code TEXT, upstream_account_id TEXT, model_route_id TEXT, billing_unit TEXT, status_class TEXT, billed_units BIGINT, duration_ms BIGINT, cost_micros BIGINT, currency TEXT, modality TEXT)",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(ddl))
            .execute(&pool)
            .await
            .unwrap();
    }

    let native = identity("native");
    let mut tx = pool.begin().await.unwrap();
    for sql in [
        "INSERT INTO request_stats_facts VALUES ($1, $2, $3, $4, 'speech', 'audio-transcription', NULL, 'account', 'route', 'token', 'success', 41, 9, 2, 1, 0, 19, 2, 41, 700, 'USD')",
        "INSERT INTO generation_stats_facts VALUES ($1, $2, $3, $4, 'image-model', NULL, 'account', 'route', 'image', 'success', 2, 25, 1500, 'EUR', 'image')",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(native.request_id.to_string())
            .bind(native.tenant_id.to_string())
            .bind(native.key_id.to_string())
            .bind(native.created_at)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    // Check uncommitted facts through the transaction; the single-slot pool
    // cannot supply another connection. Check the pool after committing.
    assert_snapshot_contract(HistoryRead::Connection(&mut tx)).await;
    tx.commit().await.unwrap();
    assert_snapshot_contract(HistoryRead::Pool(&pool)).await;
}

async fn assert_snapshot_contract(mut read: HistoryRead<'_>) {
    let completed = serde_json::json!({"completed_at": 18, "status_code": 200});
    let native = identity("native");
    let fact = read.analysis_fact(&native).await.unwrap();
    assert_eq!(fact["settled_fact"], true);
    assert_eq!(fact["input_tokens"], 41);
    assert_eq!(fact["protocol"], "audio-transcription");
    assert_eq!(
        read.statistics(&native, &completed).await.unwrap(),
        Some(HistoryStatistics {
            status: "success".into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_known_read_tokens: Some(2),
            cache_known_input_tokens: Some(41),
            cost_micros: Some(700),
            currency: Some("USD".into()),
        })
    );

    let generation = identity("generation");
    let fact = read.analysis_fact(&generation).await.unwrap();
    assert_eq!(fact["generation_units"], 2);
    assert_eq!(fact["modality"], "image");
    assert_eq!(fact["protocol"], "generation");
    assert_eq!(
        read.statistics(&generation, &completed).await.unwrap(),
        Some(HistoryStatistics {
            status: "success".into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_known_read_tokens: None,
            cache_known_input_tokens: None,
            cost_micros: Some(1500),
            currency: Some("EUR".into()),
        })
    );

    let missing = HistoryIdentity {
        created_at: native.created_at + 1,
        ..native
    };
    let payload = serde_json::json!({
        "completed_at": 18, "status_code": 200, "billing": {"currency": "JPY"}
    });
    assert_eq!(read.analysis_fact(&missing).await.unwrap(), Value::Null);
    assert_eq!(
        read.statistics(&missing, &payload).await.unwrap(),
        Some(HistoryStatistics {
            status: "pending".into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_known_read_tokens: None,
            cache_known_input_tokens: None,
            cost_micros: None,
            currency: Some("JPY".into()),
        })
    );
}
