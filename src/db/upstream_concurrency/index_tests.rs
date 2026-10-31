use super::*;
use serde_json::Value;

async fn seed_live_owners(database: &Database) {
    sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ('tenant-a', 'tenant-a', 0), ('tenant-b', 'tenant-b', 0)")
        .execute(&database.pool).await.unwrap();
    // The oldest request still has a renewed live lease. A creation-time
    // cutoff would silently lose it, including when the sample is truncated.
    sqlx::query("WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM numbers WHERE n < 8)
        INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id, proxy_owner_expires_at)
        SELECT 'live-' || CAST(n AS TEXT), CASE WHEN n % 2 = 0 THEN 'tenant-a' ELSE 'tenant-b' END, 'key',
               CASE WHEN n = 8 THEN $1 - 63072000000 ELSE $1 END,
               'openai', 'model', 0, 0, 0, 'inline-json:{}', 'live-' || CAST(n AS TEXT), $1 + 3600000 + n
        FROM numbers")
        .bind(unix_millis()).execute(&database.pool).await.unwrap();
    sqlx::query("INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id, proxy_owner_expires_at)
        VALUES ('terminal', 'tenant-a', 'key', $1, 'openai', 'model', 0, 0, 0, 'inline-json:{}', 'terminal', $1 + 7200000)")
        .bind(unix_millis()).execute(&database.pool).await.unwrap();
    sqlx::query("INSERT INTO proxy_operational_terminals (request_id, request_created_at, tenant_id, key_id, account_id, reservation_id, owner_token, fence, contract_digest, receipt_id, operational_digest, status_code, completed_at, duration_ms, response_object, recorded_at)
        SELECT id, created_at, tenant_id, key_id, 'account', reservation_id, 'owner', 'fence', 'contract', 'receipt', 'operation', 200, created_at, 0, 'inline-json:{}', created_at
        FROM request_records WHERE id = 'terminal'")
        .execute(&database.pool).await.unwrap();
}

async fn grow_history(database: &Database, size: i64) {
    // Include cancellations with no HTTP status, expired unsettled leases,
    // missing leases, and completed rows whose old lease is still in the future.
    sqlx::query("WITH RECURSIVE numbers(n) AS (SELECT CAST(1 AS BIGINT) UNION ALL SELECT n + 1 FROM numbers WHERE n < $1),
        kinds(kind) AS (SELECT 'completed' UNION ALL SELECT 'cancelled' UNION ALL SELECT 'expired' UNION ALL SELECT 'missing')
        INSERT INTO request_records (id, tenant_id, key_id, created_at, completed_at, status_code, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id, proxy_owner_expires_at)
        SELECT kind || '-' || CAST($1 AS TEXT) || '-' || CAST(n AS TEXT), 'tenant-a', 'key', $2 - CASE WHEN n % 2 = 0 THEN 31536000000 ELSE 0 END,
               CASE WHEN kind IN ('completed', 'cancelled') THEN $2 ELSE NULL END,
               CASE WHEN kind = 'completed' THEN 200 ELSE NULL END,
               'openai', 'model', 0, 0, 0, 'inline-json:{}', kind || '-' || CAST($1 AS TEXT) || '-' || CAST(n AS TEXT),
               CASE WHEN kind = 'expired' THEN 0 WHEN kind = 'missing' THEN NULL ELSE $2 + 7200000 END
        FROM numbers CROSS JOIN kinds")
        .bind(size).bind(unix_millis()).execute(&database.pool).await.unwrap();
    sqlx::query("ANALYZE request_records")
        .execute(&database.pool)
        .await
        .unwrap();
}

async fn assert_membership(database: &Database) {
    for limit in [3, 4097] {
        let mut samples = database.upstream_concurrency_samples(limit).await.unwrap();
        samples.sort_by(|a, b| a.sample_id.cmp(&b.sample_id));
        let first = if limit == 3 { 6 } else { 1 };
        let expected: Vec<String> = (first..=8).map(|n| format!("live-{n}")).collect();
        assert_eq!(
            samples.iter().map(|s| &s.sample_id).collect::<Vec<_>>(),
            expected.iter().collect::<Vec<_>>()
        );
        for (sample, n) in samples.iter().zip(first..=8) {
            assert_eq!(sample.source, "request_owner");
            assert_eq!(sample.lease_state, "live");
            assert_eq!(sample.egress_state, "missing_metadata");
            assert_eq!(
                sample.tenant_external_id,
                if n % 2 == 0 { "tenant-a" } else { "tenant-b" }
            );
        }
    }
}

async fn replay_index_upgrade(database: &Database) {
    // Model upgrading a populated schema, then verify the normal migration
    // runner can be called again without replaying the CREATE INDEX.
    sqlx::query("DROP INDEX request_records_live_owner_expiry_idx")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM schema_migrations WHERE version = 146")
        .execute(&database.pool)
        .await
        .unwrap();
    database.migrate().await.unwrap();
    database.migrate().await.unwrap();
}

#[tokio::test]
async fn sqlite_live_owner_sampling_seeks_past_expired_history() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("owner-index.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    seed_live_owners(&database).await;
    for size in [128, 8192] {
        grow_history(&database, size).await;
        replay_index_upgrade(&database).await;
        assert_membership(&database).await;
        let statement = format!(
            "EXPLAIN QUERY PLAN {}",
            database.upstream_concurrency_samples_query()
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(4097_i64)
            .fetch_all(&database.pool)
            .await
            .unwrap();
        let details: Vec<String> = rows
            .iter()
            .map(|row| row.try_get("detail").unwrap())
            .collect();
        assert!(
            details.iter().any(|detail| detail.contains(
                "SEARCH request_records USING INDEX request_records_live_owner_expiry_idx (proxy_owner_expires_at>?)"
            )),
            "live owner sampling must range-seek the expiry index: {details:?}"
        );
        assert!(
            !details
                .iter()
                .any(|detail| detail.starts_with("SCAN request_records")),
            "history must not be scanned: {details:?}"
        );
    }
}

fn inspect_request_scans(node: &Value, visited: &mut u64, seeks: &mut u64) {
    if node["Relation Name"]
        .as_str()
        .is_some_and(|name| name.starts_with("request_records"))
    {
        let loops = node["Actual Loops"].as_u64().unwrap_or(0);
        let rows = node["Actual Rows"].as_u64().unwrap_or(0)
            + node["Rows Removed by Filter"].as_u64().unwrap_or(0)
            + node["Rows Removed by Index Recheck"].as_u64().unwrap_or(0);
        *visited += rows * loops;
        if rows * loops > 0 {
            assert!(
                node["Index Cond"]
                    .as_str()
                    .is_some_and(|condition| condition.contains("proxy_owner_expires_at >")),
                "populated request partitions must range-seek live leases: {node}"
            );
            *seeks += 1;
        }
    }
    if let Some(children) = node["Plans"].as_array() {
        for child in children {
            inspect_request_scans(child, visited, seeks);
        }
    }
}

#[tokio::test]
async fn postgres_live_owner_sampling_bounds_history_scans_and_indexes_all_partitions() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "PostgreSQL regression requires MTC_TEST_POSTGRES_URL in CI"
        );
        return;
    };
    sqlx::any::install_default_drivers();
    let admin = AnyPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("owner_index_{}", Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut url = url::Url::parse(&database_url).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect(url.as_str()).await.unwrap();
    let postgres = sqlx::PgPool::connect(url.as_str()).await.unwrap();
    database.migrate().await.unwrap();
    seed_live_owners(&database).await;
    for size in [128, 8192] {
        grow_history(&database, size).await;
        replay_index_upgrade(&database).await;
        assert_membership(&database).await;
        // Use the default planner settings and the exact production statement.
        let statement = format!(
            "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, TIMING OFF) {}",
            database.upstream_concurrency_samples_query()
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(4097_i64)
            .fetch_one(&postgres)
            .await
            .unwrap();
        let raw: String = row.try_get_unchecked(0).unwrap();
        let plan: Value = serde_json::from_str(&raw).unwrap();
        let (mut visited, mut seeks) = (0, 0);
        inspect_request_scans(&plan[0]["Plan"], &mut visited, &mut seeks);
        assert!(seeks > 0, "missing live lease range seek: {plan}");
        assert!(
            visited <= 16,
            "request history visits grew beyond the nine live leases: {plan}"
        );
    }
    // A partition created after the migration must inherit the same parent
    // index. Old live requests in DEFAULT above also exercise existing leaves.
    let start = unix_millis() + 30 * 86_400_000;
    let end = start + 86_400_000;
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TABLE request_records_future PARTITION OF request_records FOR VALUES FROM ({start}) TO ({end})")))
        .execute(&database.pool).await.unwrap();
    let missing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_inherits leaf
        WHERE leaf.inhparent = 'request_records'::regclass AND NOT EXISTS (
            SELECT 1 FROM pg_index idx JOIN pg_inherits inherited ON inherited.inhrelid = idx.indexrelid
            WHERE idx.indrelid = leaf.inhrelid AND idx.indisvalid
              AND inherited.inhparent = 'request_records_live_owner_expiry_idx'::regclass)")
        .fetch_one(&database.pool).await.unwrap();
    assert_eq!(
        missing, 0,
        "existing and future partitions must inherit the expiry index"
    );
    postgres.close().await;
    database.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
