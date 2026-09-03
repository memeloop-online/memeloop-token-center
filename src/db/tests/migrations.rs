use super::super::*;

#[tokio::test]
async fn sqlite_v60_moves_opaque_credentials_into_the_normal_model() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("normal-key-credentials.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    sqlx::query(
        "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 1, 59)
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let key_id = Uuid::now_v7();
    let tenant_id = Uuid::now_v7();
    let principal_id = Uuid::now_v7();
    let account_id = Uuid::now_v7();
    let credential_id = Uuid::now_v7();
    let credential = "opaque-cpa-credential-normalized-by-v60";
    let pepper = b"v60 credential normalization pepper is long enough";
    let (secret_hash, fingerprint) = crypto::hash_credential(credential, pepper);
    let source_hash = format!("{:x}", Sha256::digest(credential.as_bytes()));
    sqlx::query(
        "INSERT INTO key_records (id,tenant_id,principal_id,account_id,alias,currency,policy_json,status,credential_generation,created_at,updated_at) VALUES ($1,$2,$3,$4,'imported','USD',$5,'active',0,1,1)",
    )
    .bind(key_id.to_string())
    .bind(tenant_id.to_string())
    .bind(principal_id.to_string())
    .bind(account_id.to_string())
    .bind(serde_json::to_string(&KeyPolicy::default()).unwrap())
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO legacy_key_credentials (id,key_id,generation,secret_hash,fingerprint,source_hash,created_at) VALUES ($1,$2,0,$3,$4,$5,1)",
    )
    .bind(credential_id.to_string())
    .bind(key_id.to_string())
    .bind(secret_hash.clone())
    .bind(&fingerprint)
    .bind(&source_hash)
    .execute(&database.pool)
    .await
    .unwrap();

    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 60, 60)
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let copied = sqlx::query(
        "SELECT key_id,generation,secret_hash,fingerprint,created_at,revoked_at FROM key_credentials WHERE id=$1",
    )
    .bind(credential_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(copied.get::<String, _>("key_id"), key_id.to_string());
    assert_eq!(copied.get::<i64, _>("generation"), 0);
    assert_eq!(copied.get::<Vec<u8>, _>("secret_hash"), secret_hash);
    assert_eq!(copied.get::<String, _>("fingerprint"), fingerprint);
    assert_eq!(copied.get::<i64, _>("created_at"), 1);
    assert_eq!(copied.get::<Option<i64>, _>("revoked_at"), None);
    let proof = sqlx::query(
        "SELECT proof_kind,source_digest,created_at FROM key_credential_source_proofs WHERE credential_id=$1",
    )
    .bind(credential_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(proof.get::<String, _>("proof_kind"), "legacy-source-hash-v1");
    assert_eq!(proof.get::<String, _>("source_digest"), source_hash);
    assert_eq!(proof.get::<i64, _>("created_at"), 1);
    assert!(
        sqlx::query("SELECT id FROM legacy_key_credentials WHERE id=$1")
            .bind(credential_id.to_string())
            .fetch_optional(&database.pool)
            .await
            .unwrap()
            .is_some()
    );
    let authenticated = database.authenticate_key(credential, pepper).await.unwrap();
    assert_eq!(authenticated.key_id, key_id);
    assert_eq!(authenticated.credential_generation, 0);
}

#[tokio::test]
async fn sqlite_global_model_history_indexes_drive_both_top_n_sources() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("global-model-history.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();

    for index in [
        "request_records_global_model_time_idx",
        "generation_jobs_global_model_time_idx",
    ] {
        let installed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = $1",
        )
        .bind(index)
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(installed, 1, "missing v59 index {index}");
    }

    let request_plan: Vec<String> = sqlx::query(
        "EXPLAIN QUERY PLAN SELECT id, created_at FROM request_records WHERE created_at >= 0 AND created_at <= 9223372036854775807 AND model = 'needle' ORDER BY created_at DESC, id DESC LIMIT 5",
    )
    .fetch_all(&database.pool)
    .await
    .unwrap()
    .into_iter()
    .map(|row| row.try_get("detail").unwrap())
    .collect();
    assert!(
        request_plan
            .iter()
            .any(|detail| detail.contains("request_records_global_model_time_idx")),
        "request-record model Top-N must use v59 index: {request_plan:?}"
    );

    let generation_plan: Vec<String> = sqlx::query(
        "EXPLAIN QUERY PLAN SELECT id, created_at FROM generation_jobs WHERE created_at >= 0 AND created_at <= 9223372036854775807 AND public_model = 'needle' ORDER BY created_at DESC, id DESC LIMIT 5",
    )
    .fetch_all(&database.pool)
    .await
    .unwrap()
    .into_iter()
    .map(|row| row.try_get("detail").unwrap())
    .collect();
    assert!(
        generation_plan
            .iter()
            .any(|detail| detail.contains("generation_jobs_global_model_time_idx")),
        "generation model Top-N must use v59 index: {generation_plan:?}"
    );
}

#[tokio::test]
async fn durable_oauth_migration_retires_bridge_routes_without_deleting_history() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("oauth-retirement.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    sqlx::query(
        "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 1, 44)
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    sqlx::query(
        "INSERT INTO tenants (id, external_id, created_at) VALUES ('tenant-oauth-retirement', 'oauth-retirement', 1)",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, oauth_session_id, oauth_driver, oauth_refresh_url, created_at, updated_at) VALUES ('bridge-account', 'tenant-oauth-retirement', 'historical connection', 'cpa-subscription-bridge', 'oauth', '{"base_url":"http://legacy.invalid"}', 'active', 1, NULL, NULL, NULL, 1, 1)"#,
    )
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO model_routes (id, tenant_id, public_model, upstream_account_id, upstream_model, protocol, priority, enabled, created_at, updated_at) VALUES ('bridge-route', 'tenant-oauth-retirement', 'historical-model', 'bridge-account', 'historical-upstream', 'openai', 0, 1, 1, 1)",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id, upstream_account_id, model_route_id) VALUES ('historical-request', 'tenant-oauth-retirement', 'historical-key', 1, 'openai', 'historical-model', 1, 1, 0, '{}', 'historical-reservation', 'bridge-account', 'bridge-route')",
    )
    .execute(&database.pool)
    .await
    .unwrap();

    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 45, 45)
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let account_status: String =
        sqlx::query("SELECT status FROM upstream_accounts WHERE id = 'bridge-account'")
            .fetch_one(&database.pool)
            .await
            .unwrap()
            .try_get("status")
            .unwrap();
    let route_enabled: i64 =
        sqlx::query("SELECT enabled FROM model_routes WHERE id = 'bridge-route'")
            .fetch_one(&database.pool)
            .await
            .unwrap()
            .try_get("enabled")
            .unwrap();
    let historical_links: (String, String) = {
        let row = sqlx::query(
            "SELECT upstream_account_id, model_route_id FROM request_records WHERE id = 'historical-request'",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
        (
            row.try_get("upstream_account_id").unwrap(),
            row.try_get("model_route_id").unwrap(),
        )
    };
    assert_eq!(account_status, "disabled");
    assert_eq!(route_enabled, 0);
    assert_eq!(
        historical_links,
        ("bridge-account".to_owned(), "bridge-route".to_owned())
    );
    assert!(
        sqlx::query("SELECT id FROM oauth_login_sessions LIMIT 1")
            .fetch_optional(&database.pool)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn budget_rollup_migration_backfills_existing_usage_and_reservations() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("budget-backfill.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    for statement in [
        "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
        "CREATE TABLE key_records (id TEXT PRIMARY KEY, updated_at BIGINT NOT NULL)",
        "CREATE TABLE credit_accounts (id TEXT PRIMARY KEY, updated_at BIGINT NOT NULL)",
        "CREATE TABLE usage_reservations (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, key_id TEXT NOT NULL, reserved_micros BIGINT NOT NULL, status TEXT NOT NULL)",
        "CREATE TABLE ledger_entries (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, key_id TEXT, kind TEXT NOT NULL, amount_micros BIGINT NOT NULL, currency TEXT NOT NULL, source TEXT NOT NULL, idempotency_key TEXT, created_at BIGINT NOT NULL, reference_entry_id TEXT, entitlement_cycle_id TEXT)",
    ] {
        sqlx::query(statement)
            .execute(&database.pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO key_records VALUES ('key', 300)")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO credit_accounts VALUES ('account', 300)")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO usage_reservations VALUES ('active', 'account', 'key', 250, 'reserved'), ('settled-one', 'account', 'key', 400, 'settled'), ('settled-two', 'account', 'key', 600, 'settled')")
            .execute(&database.pool)
            .await
            .unwrap();
    sqlx::query("INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) VALUES ('usage-one', 'account', 'key', 'usage', -400, 'USD', 'settled-one', 100), ('grant', 'account', NULL, 'grant', 2000, 'USD', 'test', 150), ('usage-two', 'account', 'key', 'usage', -600, 'USD', 'settled-two', 200)")
            .execute(&database.pool)
            .await
            .unwrap();

    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 22, 22)
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let state = sqlx::query("SELECT settled_lifetime_micros, reserved_micros FROM key_budget_state WHERE key_id = 'key'")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(state.get::<i64, _>("settled_lifetime_micros"), 1_000);
    assert_eq!(state.get::<i64, _>("reserved_micros"), 250);
    let account: i64 = sqlx::query(
        "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = 'account'",
    )
    .fetch_one(&database.pool)
    .await
    .unwrap()
    .get("settled_lifetime_micros");
    assert_eq!(account, 1_000);
    let snapshot: i64 =
        sqlx::query("SELECT account_usage_micros_snapshot FROM ledger_entries WHERE id = 'grant'")
            .fetch_one(&database.pool)
            .await
            .unwrap()
            .get("account_usage_micros_snapshot");
    assert_eq!(snapshot, 400);
}

#[test]
fn terminal_stats_do_not_scan_wide_request_or_generation_history() {
    assert!(!FILTERED_ACTIVITY_SOURCE_ROLLUPS.contains("FROM request_records"));
    assert!(!FILTERED_ACTIVITY_SOURCE_ROLLUPS.contains("FROM generation_jobs"));
    assert!(FILTERED_ACTIVITY_SOURCE_ROLLUPS.contains("request_daily_aggregates"));
    assert!(FILTERED_ACTIVITY_SOURCE_ROLLUPS.contains("request_stats_facts"));
    assert!(FILTERED_ACTIVITY_SOURCE_ROLLUPS.contains("generation_daily_aggregates"));
    assert!(FILTERED_ACTIVITY_SOURCE_ROLLUPS.contains("generation_stats_facts"));
    assert!(!FILTERED_ACTIVITY_SOURCE_FACTS.contains("FROM request_records"));
    assert!(!FILTERED_ACTIVITY_SOURCE_FACTS.contains("FROM generation_jobs"));
    assert!(FILTERED_ACTIVITY_SOURCE_PENDING.contains("FROM request_records"));
    assert!(FILTERED_ACTIVITY_SOURCE_PENDING.contains("FROM generation_jobs"));
    assert!(FILTERED_ACTIVITY_SOURCE_PENDING.contains("g.created_at >= $3"));
    assert!(FILTERED_ACTIVITY_SOURCE_PENDING.contains("g.created_at <= $4"));
}

#[tokio::test]
async fn sqlite_complete_session_usage_migration_backfills_all_usage_once() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("session-usage-v54.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    sqlx::query(
        "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 1, 53)
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    sqlx::query(
        r#"INSERT INTO request_stats_facts (
               request_id, tenant_id, key_id, created_at, model, protocol,
               status_class, error_code, upstream_account_id, model_route_id,
               duration_ms, input_tokens, output_tokens, cached_input_tokens,
               cache_write_tokens, service_tier, currency, cost_micros, session_id)
           VALUES ('request-v54', 'tenant-v54', 'key-v54', 100, 'text-model',
                   'openai-responses', 'success', '', '', '', 30, 100, 7,
                   30, 20, 'default', 'USD', 100, 'unlinked:key-v54')"#,
    )
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO generation_stats_facts (
               job_id, tenant_id, key_id, created_at, model, status_class,
               error_code, upstream_account_id, duration_ms, cost_micros,
               billed_units, currency, modality, billing_unit, model_route_id)
           VALUES ('generation-v54', 'tenant-v54', 'key-v54', 200,
                   'image-model', 'success', '', '', 40, 200, 8, 'USD',
                   'image', 'job', '')"#,
    )
    .execute(&database.pool)
    .await
    .unwrap();

    for _ in 0..2 {
        let mut transaction = database.pool.begin().await.unwrap();
        apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 54, 54)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
    }

    let totals = sqlx::query(
        r#"SELECT last_activity_at, requests, errors, input_tokens, output_tokens,
                  cached_input_tokens, cache_write_tokens, generation_units,
                  duration_count, duration_sum_ms, cost_micros
             FROM session_usage_totals
            WHERE tenant_id = 'tenant-v54' AND key_id = 'key-v54'
              AND session_id = 'unlinked:key-v54' AND currency = 'USD'"#,
    )
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(totals.get::<i64, _>("last_activity_at"), 200);
    assert_eq!(totals.get::<i64, _>("requests"), 2);
    assert_eq!(totals.get::<i64, _>("errors"), 0);
    assert_eq!(totals.get::<i64, _>("input_tokens"), 50);
    assert_eq!(totals.get::<i64, _>("output_tokens"), 7);
    assert_eq!(totals.get::<i64, _>("cached_input_tokens"), 30);
    assert_eq!(totals.get::<i64, _>("cache_write_tokens"), 20);
    assert_eq!(totals.get::<i64, _>("generation_units"), 8);
    assert_eq!(totals.get::<i64, _>("duration_count"), 2);
    assert_eq!(totals.get::<i64, _>("duration_sum_ms"), 70);
    assert_eq!(totals.get::<i64, _>("cost_micros"), 300);
    for table in ["session_usage_hourly", "session_usage_daily"] {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT SUM(requests) AS requests, SUM(input_tokens) AS input_tokens, SUM(cached_input_tokens) AS cached_input_tokens, SUM(cache_write_tokens) AS cache_write_tokens, SUM(generation_units) AS generation_units, SUM(cost_micros) AS cost_micros FROM {table} WHERE tenant_id = 'tenant-v54' AND key_id = 'key-v54' AND session_id = 'unlinked:key-v54'"
        )))
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(row.get::<i64, _>("requests"), 2, "{table}");
        assert_eq!(row.get::<i64, _>("input_tokens"), 50, "{table}");
        assert_eq!(row.get::<i64, _>("cached_input_tokens"), 30, "{table}");
        assert_eq!(row.get::<i64, _>("cache_write_tokens"), 20, "{table}");
        assert_eq!(row.get::<i64, _>("generation_units"), 8, "{table}");
        assert_eq!(row.get::<i64, _>("cost_micros"), 300, "{table}");
    }
}

#[tokio::test]
async fn sqlite_generation_aggregate_migration_backfills_once() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory
            .path()
            .join("generation-aggregate-upgrade.db")
            .display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    sqlx::query(
            "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
            "CREATE TABLE generation_jobs (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL, upstream_account_id TEXT NOT NULL, public_model TEXT NOT NULL, status TEXT NOT NULL, error_code TEXT, billed_units BIGINT, cost_micros BIGINT NOT NULL, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL, completed_at BIGINT)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO generation_jobs (id, tenant_id, key_id, upstream_account_id, public_model, status, error_code, billed_units, cost_micros, created_at, updated_at, completed_at) VALUES ('old-job', 'tenant-1', 'key-1', 'upstream-1', 'image-old', 'failed', 'upstream_error', 2, 750000, 86400123, 86400456, 86400456)",
        )
        .execute(&database.pool)
        .await
        .unwrap();

    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 23, 23)
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let aggregate = sqlx::query(
            "SELECT day_bucket, status_class, error_code, requests, billed_units, cost_micros FROM generation_daily_aggregates WHERE key_id = 'key-1'",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(aggregate.get::<i64, _>("day_bucket"), 1);
    assert_eq!(aggregate.get::<String, _>("status_class"), "failure");
    assert_eq!(aggregate.get::<String, _>("error_code"), "upstream_error");
    assert_eq!(aggregate.get::<i64, _>("requests"), 1);
    assert_eq!(aggregate.get::<i64, _>("billed_units"), 2);
    assert_eq!(aggregate.get::<i64, _>("cost_micros"), 750_000);
    let fact = sqlx::query(
            "SELECT created_at, duration_ms, cost_micros FROM generation_stats_facts WHERE job_id = 'old-job'",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(fact.get::<i64, _>("created_at"), 86_400_123);
    assert_eq!(fact.get::<i64, _>("duration_ms"), 333);
    assert_eq!(fact.get::<i64, _>("cost_micros"), 750_000);
    let marker: Option<i64> =
        sqlx::query_scalar("SELECT stats_aggregated_at FROM generation_jobs WHERE id = 'old-job'")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(marker, Some(86_400_456));

    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 23, 23)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    let requests: i64 = sqlx::query_scalar(
        "SELECT requests FROM generation_daily_aggregates WHERE key_id = 'key-1'",
    )
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn sqlite_request_stats_migration_backfills_terminal_rows_once() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("request-stats-upgrade.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    sqlx::query(
            "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE key_records (id TEXT PRIMARY KEY, currency TEXT NOT NULL)")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO key_records (id, currency) VALUES ('key-1', 'USD')")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
            "CREATE TABLE generation_stats_facts (job_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL, created_at BIGINT NOT NULL, model TEXT NOT NULL, status_class TEXT NOT NULL, error_code TEXT NOT NULL, upstream_account_id TEXT NOT NULL, duration_ms BIGINT NOT NULL, cost_micros BIGINT NOT NULL, billed_units BIGINT NOT NULL)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO generation_stats_facts (job_id, tenant_id, key_id, created_at, model, status_class, error_code, upstream_account_id, duration_ms, cost_micros, billed_units) VALUES ('generation-1', 'tenant-1', 'key-1', 86400600, 'image-a', 'success', '', 'upstream-1', 400, 1250000, 2)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
            "CREATE TABLE request_records (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL, created_at BIGINT NOT NULL, completed_at BIGINT, protocol TEXT NOT NULL, model TEXT NOT NULL, status_code BIGINT, duration_ms BIGINT, input_tokens BIGINT NOT NULL, output_tokens BIGINT NOT NULL, cost_micros BIGINT NOT NULL, error_code TEXT, upstream_account_id TEXT, model_route_id TEXT)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO request_records (id, tenant_id, key_id, created_at, completed_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code, upstream_account_id, model_route_id) VALUES ('success', 'tenant-1', 'key-1', 86400123, 86400456, 'openai', 'model-a', 200, 333, 11, 7, 250000, NULL, 'upstream-1', 'route-1'), ('failure', 'tenant-1', 'key-1', 86400789, 86400899, 'anthropic', 'model-a', 502, 110, 13, 5, 750000, 'upstream_error', NULL, NULL), ('pending', 'tenant-1', 'key-1', 86400999, NULL, 'openai', 'model-a', NULL, NULL, 0, 0, 0, NULL, NULL, NULL)",
        )
        .execute(&database.pool)
        .await
        .unwrap();

    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 24, 24)
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let facts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_stats_facts")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(facts, 2);
    let totals = sqlx::query(
            "SELECT SUM(requests) AS requests, SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens, SUM(cost_micros) AS cost_micros FROM request_daily_aggregates",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(totals.get::<i64, _>("requests"), 2);
    assert_eq!(totals.get::<i64, _>("input_tokens"), 24);
    assert_eq!(totals.get::<i64, _>("output_tokens"), 12);
    assert_eq!(totals.get::<i64, _>("cost_micros"), 1_000_000);
    let generation = sqlx::query(
            "SELECT requests, input_tokens, output_tokens, generation_units, currency, cost_micros FROM usage_analysis_hourly WHERE source_kind = 'generation'",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(generation.get::<i64, _>("requests"), 1);
    assert_eq!(generation.get::<i64, _>("input_tokens"), 0);
    assert_eq!(generation.get::<i64, _>("output_tokens"), 0);
    assert_eq!(generation.get::<i64, _>("generation_units"), 2);
    assert_eq!(generation.get::<String, _>("currency"), "USD");
    assert_eq!(generation.get::<i64, _>("cost_micros"), 1_250_000);

    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 24, 24)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    let requests: i64 = sqlx::query_scalar("SELECT SUM(requests) FROM request_daily_aggregates")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(requests, 2);
}

#[tokio::test]
async fn sqlite_request_stats_rollups_use_exact_utc_boundaries_and_filters() {
    const DAY: i64 = 86_400_000;
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory
            .path()
            .join("request-stats-boundaries.db")
            .display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "stats-boundary-tenant".to_owned(),
                principal_external_id: "Boundary-Principal".to_owned(),
                alias: "Boundary-Credential".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            b"request stats boundary test pepper is sufficiently long",
        )
        .await
        .unwrap();
    let tenant_id: String = sqlx::query_scalar("SELECT tenant_id FROM key_records WHERE id = $1")
        .bind(issued.key_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    let upstream_id = Uuid::now_v7();
    let route_id = Uuid::now_v7();
    let samples = [
        (10 * DAY + 100, "openai", "success", "", 10_i64, 100_i64),
        (
            11 * DAY + 500,
            "anthropic",
            "failure",
            "boundary_error",
            20,
            200,
        ),
        (12 * DAY + 200, "openai", "success", "", 30, 300),
    ];
    for (index, (created_at, protocol, status, error, duration, cost)) in
        samples.into_iter().enumerate()
    {
        let request_id = Uuid::now_v7().to_string();
        sqlx::query(
                "INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, currency, cost_micros) VALUES ($1, $2, $3, $4, 'boundary-model', $5, $6, $7, $8, $9, $10, 1, 2, 'USD', $11)",
            )
            .bind(request_id)
            .bind(&tenant_id)
            .bind(issued.key_id.to_string())
            .bind(created_at)
            .bind(protocol)
            .bind(status)
            .bind(error)
            .bind(upstream_id.to_string())
            .bind(route_id.to_string())
            .bind(duration)
            .bind(cost)
            .execute(&database.pool)
            .await
            .unwrap();
        sqlx::query(
                "INSERT INTO request_daily_aggregates (tenant_id, key_id, day_bucket, model, protocol, status_class, error_code, upstream_account_id, model_route_id, currency, requests, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, 'boundary-model', $4, $5, $6, $7, $8, 'USD', 1, 1, 2, $9)",
            )
            .bind(&tenant_id)
            .bind(issued.key_id.to_string())
            .bind(created_at / DAY)
            .bind(protocol)
            .bind(status)
            .bind(error)
            .bind(upstream_id.to_string())
            .bind(route_id.to_string())
            .bind(cost)
            .execute(&database.pool)
            .await
            .unwrap();
        assert!(index < 3);
    }

    let interval = StatsFilter {
        from_created_at: Some(10 * DAY + 100),
        to_created_at: Some(12 * DAY + 200),
        ..StatsFilter::default()
    };
    let stats = database
        .operator_stats_filtered("stats-boundary-tenant", interval.clone())
        .await
        .unwrap();
    assert_eq!(stats.summary.total_requests, 3);
    assert_eq!(stats.summary.successful_requests, 2);
    assert_eq!(stats.summary.failed_requests, 1);
    assert_eq!(stats.summary.input_tokens, 3);
    assert_eq!(stats.summary.output_tokens, 6);
    assert_eq!(stats.summary.total_cost.as_deref(), Some("0.0006"));
    assert_eq!(stats.by_day.len(), 3);
    assert!(stats.by_day.iter().all(|bucket| bucket.requests == 1));

    let filtered = database
        .operator_stats_filtered(
            "stats-boundary-tenant",
            StatsFilter {
                protocol: Some("anthropic".to_owned()),
                status: Some("error".to_owned()),
                error_code: Some("boundary_error".to_owned()),
                upstream_account_id: Some(upstream_id),
                route_id: Some(route_id),
                key_alias: Some("boundary-c".to_owned()),
                principal: Some("boundary-p".to_owned()),
                ..interval.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(filtered.summary.total_requests, 1);
    assert_eq!(filtered.errors[0].name, "boundary_error");

    let fact_filtered = database
        .operator_stats_filtered(
            "stats-boundary-tenant",
            StatsFilter {
                min_duration_ms: Some(25),
                min_cost_micros: Some(250),
                ..interval
            },
        )
        .await
        .unwrap();
    assert_eq!(fact_filtered.summary.total_requests, 1);
    assert_eq!(fact_filtered.summary.total_cost.as_deref(), Some("0.0003"));

    let cny = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "stats-boundary-tenant".to_owned(),
                principal_external_id: "Boundary-CNY-Principal".to_owned(),
                alias: "Boundary-CNY-Credential".to_owned(),
                currency: "CNY".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            b"request stats boundary test pepper is sufficiently long",
        )
        .await
        .unwrap();
    let cny_request_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, currency, cost_micros) VALUES ($1, $2, $3, $4, 'boundary-model', 'openai', 'success', '', '', '', 15, 4, 5, 'CNY', 400)",
    )
    .bind(cny_request_id)
    .bind(&tenant_id)
    .bind(cny.key_id.to_string())
    .bind(11 * DAY + 600)
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO request_daily_aggregates (tenant_id, key_id, day_bucket, model, protocol, status_class, error_code, upstream_account_id, model_route_id, currency, requests, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, 'boundary-model', 'openai', 'success', '', '', '', 'CNY', 1, 4, 5, 400)",
    )
    .bind(&tenant_id)
    .bind(cny.key_id.to_string())
    .bind(11)
    .execute(&database.pool)
    .await
    .unwrap();

    let mixed = database
        .operator_stats_filtered(
            "stats-boundary-tenant",
            StatsFilter {
                from_created_at: Some(10 * DAY + 100),
                to_created_at: Some(12 * DAY + 200),
                ..StatsFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(mixed.summary.total_requests, 4);
    assert_eq!(mixed.summary.total_cost, None);
    assert_eq!(mixed.summary.costs.len(), 2);
    assert_eq!(mixed.summary.costs[0].currency, "CNY");
    assert_eq!(mixed.summary.costs[0].cost, "0.0004");
    assert_eq!(mixed.summary.costs[1].currency, "USD");
    assert_eq!(mixed.summary.costs[1].cost, "0.0006");
    assert_eq!(mixed.by_model[0].cost, None);
    assert_eq!(mixed.by_model[0].costs.len(), 2);
}

async fn create_locator_migration_fixture(database: &Database) {
    sqlx::query(
            "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
            "CREATE TABLE request_records (id TEXT NOT NULL, created_at BIGINT NOT NULL, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
            "CREATE TABLE request_events (event_id TEXT NOT NULL, event_at BIGINT NOT NULL, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL, request_id TEXT NOT NULL)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn sqlite_locator_migration_backfills_and_claims_idempotently() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("locator-upgrade.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    create_locator_migration_fixture(&database).await;
    sqlx::query(
            "INSERT INTO request_records (id, created_at, tenant_id, key_id) VALUES ('request-1', 100, 'tenant-1', 'key-1')",
        )
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
            "INSERT INTO request_events (event_id, event_at, tenant_id, key_id, request_id) VALUES ('event-1', 101, 'tenant-1', 'key-1', 'request-1')",
        )
        .execute(&database.pool)
        .await
        .unwrap();

    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 21, 21)
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let request = sqlx::query(
        "SELECT created_at, tenant_id, key_id FROM request_record_locators WHERE id = 'request-1'",
    )
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(request.get::<i64, _>("created_at"), 100);
    assert_eq!(request.get::<String, _>("tenant_id"), "tenant-1");
    assert_eq!(request.get::<String, _>("key_id"), "key-1");
    let event = sqlx::query(
        "SELECT created_at, request_id FROM request_event_locators WHERE id = 'event-1'",
    )
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(event.get::<i64, _>("created_at"), 101);
    assert_eq!(event.get::<String, _>("request_id"), "request-1");

    let mut transaction = database.pool.begin().await.unwrap();
    assert!(
        !claim_request_record_locator(&mut transaction, "request-1", 100, "tenant-1", "key-1")
            .await
            .unwrap()
    );
    assert!(
        claim_request_record_locator(&mut transaction, "request-1", 999, "tenant-1", "key-1")
            .await
            .is_err()
    );
    assert!(
        !claim_request_event_locator(
            &mut transaction,
            "event-1",
            101,
            "tenant-1",
            "key-1",
            "request-1"
        )
        .await
        .unwrap()
    );
    assert!(
        claim_request_event_locator(
            &mut transaction,
            "event-1",
            101,
            "tenant-1",
            "key-1",
            "request-2"
        )
        .await
        .is_err()
    );
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn sqlite_locator_migration_fails_closed_on_historical_duplicate_ids() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("locator-duplicate.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    create_locator_migration_fixture(&database).await;
    for created_at in [100_i64, 200] {
        sqlx::query(
                "INSERT INTO request_records (id, created_at, tenant_id, key_id) VALUES ('duplicate-request', $1, 'tenant-1', 'key-1')",
            )
            .bind(created_at)
            .execute(&database.pool)
            .await
            .unwrap();
    }

    let mut transaction = database.pool.begin().await.unwrap();
    let error = apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 21, 21)
        .await
        .expect_err("duplicate historical request ids must abort v21");
    assert!(error.to_string().contains("request_record_locators"));
    transaction.rollback().await.unwrap();
    let applied: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations WHERE version = 21")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(applied, 0);
}

#[tokio::test]
async fn sqlite_request_lifecycle_uses_locators_for_finish_detail_and_events() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("locator-lifecycle.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let request_id = Uuid::now_v7();
    let tenant_id = Uuid::now_v7();
    let key_id = Uuid::now_v7();
    database
        .record_request_started(NewRequest {
            request_id,
            tenant_id,
            key_id,
            protocol: "openai-responses".into(),
            model: "locator-model".into(),
            request_object: "memory://locator-request".into(),
            reservation_id: Uuid::now_v7(),
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let locator_created_at: i64 = sqlx::query_scalar(
            "SELECT created_at FROM request_record_locators WHERE id = $1 AND tenant_id = $2 AND key_id = $3",
        )
        .bind(request_id.to_string())
        .bind(tenant_id.to_string())
        .bind(key_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();

    database
        .record_request_finished(FinishRequest {
            request_id,
            status_code: 200,
            duration_ms: 12,
            input_tokens: 3,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 5,
            service_tier: None,
            cost_micros: 7,
            error_code: None,
            response_object: "memory://locator-response".into(),
        })
        .await
        .unwrap();
    let detail = database
        .request_archive_refs(key_id, request_id)
        .await
        .unwrap();
    assert_eq!(detail.view.created_at, locator_created_at);
    assert_eq!(detail.view.status_code, Some(200));
    assert_eq!(
        detail.response_object.as_deref(),
        Some("memory://locator-response")
    );
    let located_events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM request_event_locators l JOIN request_events e ON e.event_id = l.id AND e.event_at = l.created_at WHERE l.request_id = $1",
        )
        .bind(request_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(located_events, 2);

    // Removing the leaf while leaving its stable owner is treated as
    // corruption, never as permission to fall through to a broad id scan.
    sqlx::query("DELETE FROM request_records WHERE id = $1 AND created_at = $2")
        .bind(request_id.to_string())
        .bind(locator_created_at)
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(matches!(
        database.request_archive_refs(key_id, request_id).await,
        Err(AppError::Internal)
    ));
}

#[tokio::test]
async fn postgres_locator_migration_rejects_duplicates_across_partitions() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    sqlx::any::install_default_drivers();
    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    let mut transaction = pool.begin().await.unwrap();
    let schema = format!("locator_duplicate_{}", Uuid::now_v7().simple());
    // Test-only SQL safety boundary: the identifier consists of a literal prefix plus a
    // library-generated UUID rendered as lowercase hexadecimal; no external input is present.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL search_path = {schema}"
    )))
    .execute(&mut *transaction)
    .await
    .unwrap();
    sqlx::query(
            "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query(
            "CREATE TABLE request_records (id TEXT NOT NULL, created_at BIGINT NOT NULL, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL) PARTITION BY RANGE (created_at)",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query(
            "CREATE TABLE request_records_early PARTITION OF request_records FOR VALUES FROM (0) TO (100)",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query(
            "CREATE TABLE request_records_late PARTITION OF request_records FOR VALUES FROM (100) TO (200)",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query(
            "CREATE TABLE request_events (event_id TEXT NOT NULL, event_at BIGINT NOT NULL, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL, request_id TEXT NOT NULL)",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
    for created_at in [50_i64, 150] {
        sqlx::query(
                "INSERT INTO request_records (id, created_at, tenant_id, key_id) VALUES ('duplicate-request', $1, 'tenant-1', 'key-1')",
            )
            .bind(created_at)
            .execute(&mut *transaction)
            .await
            .unwrap();
    }

    let error = apply_migration_range(&mut transaction, POSTGRES_MIGRATIONS, 21, 21)
        .await
        .expect_err("cross-partition duplicate ids must abort v21");
    assert!(error.to_string().contains("request_record_locators"));
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn postgres_locator_timestamp_prunes_request_detail_to_one_leaf() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let request_id = Uuid::now_v7();
    let tenant_id = Uuid::now_v7();
    let key_id = Uuid::now_v7();
    database
        .record_request_started(NewRequest {
            request_id,
            tenant_id,
            key_id,
            protocol: "openai-responses".into(),
            model: "locator-pruning-model".into(),
            request_object: "memory://locator-pruning-request".into(),
            reservation_id: Uuid::now_v7(),
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let created_at: i64 =
        sqlx::query_scalar("SELECT created_at FROM request_record_locators WHERE id = $1")
            .bind(request_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    let storage_partition: String = sqlx::query_scalar(
        "SELECT tableoid::regclass::TEXT FROM request_records WHERE id = $1 AND created_at = $2",
    )
    .bind(request_id.to_string())
    .bind(created_at)
    .fetch_one(&database.pool)
    .await
    .unwrap();
    let plan = sqlx::query(
            "EXPLAIN (FORMAT TEXT) SELECT id, created_at, request_object FROM request_records WHERE id = $1 AND created_at = $2",
        )
        .bind(request_id.to_string())
        .bind(created_at)
        .fetch_all(&database.pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get::<String, _>(0))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(plan.contains(&storage_partition), "{plan}");
    assert!(!plan.contains("Append"), "{plan}");
}

#[tokio::test]
async fn postgres_partition_maintenance_skips_default_overlap_and_continues() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    sqlx::any::install_default_drivers();
    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    let mut transaction = pool.begin().await.unwrap();
    let schema = format!("partition_maintenance_{}", Uuid::now_v7().simple());
    // Test-only SQL safety boundary: the identifier consists of a literal prefix plus a
    // library-generated UUID rendered as lowercase hexadecimal; no external input is present.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL search_path = {schema}"
    )))
    .execute(&mut *transaction)
    .await
    .unwrap();
    sqlx::query(
            "CREATE TABLE request_records (id TEXT NOT NULL, created_at BIGINT NOT NULL, payload TEXT NOT NULL) PARTITION BY RANGE (created_at)",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query(
            "CREATE TABLE request_events (event_id TEXT NOT NULL, event_at BIGINT NOT NULL, payload TEXT NOT NULL) PARTITION BY RANGE (event_at)",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
    for table in ["request_records", "request_events"] {
        let statement = match table {
            "request_records" => {
                "CREATE TABLE request_records_default PARTITION OF request_records DEFAULT"
            }
            "request_events" => {
                "CREATE TABLE request_events_default PARTITION OF request_events DEFAULT"
            }
            _ => unreachable!("test table names are a closed internal set"),
        };
        sqlx::query(statement)
            .execute(&mut *transaction)
            .await
            .unwrap();
    }

    let today = Utc::now().date_naive();
    let today_start = today
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis();
    sqlx::query(
        "INSERT INTO request_records_default (id, created_at, payload) VALUES ($1, $2, $3)",
    )
    .bind("blocked-row")
    .bind(today_start + 123)
    .bind("must remain unchanged")
    .execute(&mut *transaction)
    .await
    .unwrap();

    let report = maintain_postgres_partitions(&mut transaction)
        .await
        .unwrap();

    assert_eq!(report.ready_partitions, 17);
    assert_eq!(
        report.blocked_partitions,
        vec![BlockedPartition {
            table: "request_records".to_owned(),
            partition: format!("request_records_{}", today.format("%Y%m%d")),
            day: today,
        }]
    );
    let stored = sqlx::query(
            "SELECT id, created_at, payload, tableoid::regclass::TEXT AS storage_partition FROM request_records WHERE id = $1",
        )
        .bind("blocked-row")
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
    assert_eq!(stored.get::<String, _>("id"), "blocked-row");
    assert_eq!(stored.get::<i64, _>("created_at"), today_start + 123);
    assert_eq!(stored.get::<String, _>("payload"), "must remain unchanged");
    assert!(
        stored
            .get::<String, _>("storage_partition")
            .ends_with("request_records_default")
    );

    let tomorrow = today.checked_add_days(Days::new(1)).unwrap();
    for table in ["request_records", "request_events"] {
        let expected = format!("{table}_{}", tomorrow.format("%Y%m%d"));
        let created: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::TEXT")
            .bind(format!("{schema}.{expected}"))
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
        assert_eq!(created.as_deref(), Some(expected.as_str()));
    }
    // The caller's outer transaction remains usable after the rejected partition DDL.
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT 1::BIGINT")
            .fetch_one(&mut *transaction)
            .await
            .unwrap(),
        1
    );
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn sqlite_upgrade_adds_request_routing_columns() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("upgrade.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    sqlx::query(
            "CREATE TABLE request_records (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL, created_at BIGINT NOT NULL, completed_at BIGINT, protocol TEXT NOT NULL, model TEXT NOT NULL, status_code BIGINT, duration_ms BIGINT, input_tokens BIGINT NOT NULL, output_tokens BIGINT NOT NULL, cost_micros BIGINT NOT NULL, error_code TEXT, request_object TEXT NOT NULL, response_object TEXT, reservation_id TEXT NOT NULL, conversation_cluster_id TEXT)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
            "CREATE TABLE upstream_accounts (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, name TEXT NOT NULL, driver TEXT NOT NULL, auth_kind TEXT NOT NULL, config_json TEXT NOT NULL, status TEXT NOT NULL, credential_generation BIGINT NOT NULL, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL, UNIQUE(tenant_id, name))",
        )
        .execute(&database.pool)
        .await
        .unwrap();

    database.migrate().await.unwrap();

    for column in ["upstream_account_id", "model_route_id"] {
        let present =
            sqlx::query("SELECT name FROM pragma_table_info('request_records') WHERE name = $1")
                .bind(column)
                .fetch_optional(&database.pool)
                .await
                .unwrap()
                .is_some();
        assert!(present, "missing upgraded column {column}");
    }
    let oauth_session_present = sqlx::query(
        "SELECT name FROM pragma_table_info('upstream_accounts') WHERE name = 'oauth_session_id'",
    )
    .fetch_optional(&database.pool)
    .await
    .unwrap()
    .is_some();
    assert!(oauth_session_present);
}

#[tokio::test]
async fn sqlite_routing_groups_are_tenant_safe_and_backfill_legacy_route_candidates() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("routing-groups.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&database.pool)
        .await
        .unwrap();
    for statement in [
        "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
        "CREATE TABLE tenants (id TEXT PRIMARY KEY)",
        "CREATE TABLE upstream_accounts (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL)",
        "CREATE TABLE model_routes (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, public_model TEXT NOT NULL, upstream_account_id TEXT NOT NULL, upstream_model TEXT NOT NULL, protocol TEXT NOT NULL, priority BIGINT NOT NULL, enabled BIGINT NOT NULL, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL, UNIQUE(tenant_id, public_model, protocol, priority))",
        "CREATE TABLE key_records (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL)",
        "CREATE TABLE request_records (id TEXT PRIMARY KEY, model_route_id TEXT)",
    ] {
        sqlx::query(statement)
            .execute(&database.pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO tenants (id) VALUES ('tenant-a'), ('tenant-b')")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO upstream_accounts (id, tenant_id) VALUES ('account-a', 'tenant-a'), ('account-b', 'tenant-b')",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO request_records (id, model_route_id) VALUES ('request-a', 'route-a')")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO model_routes (id, tenant_id, public_model, upstream_account_id, upstream_model, protocol, priority, enabled, created_at, updated_at) VALUES ('route-a', 'tenant-a', 'public-model', 'account-a', 'upstream-model-a', 'openai-responses', 10, 1, 100, 100)",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO key_records (id, tenant_id) VALUES ('key-a', 'tenant-a'), ('key-b', 'tenant-b')",
    )
    .execute(&database.pool)
    .await
    .unwrap();

    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 43, 43)
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let candidate = sqlx::query(
        "SELECT tenant_id, upstream_account_id, upstream_model, scheduling_weight FROM model_route_upstream_accounts WHERE model_route_id = 'route-a'",
    )
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(candidate.get::<String, _>("tenant_id"), "tenant-a");
    assert_eq!(
        candidate.get::<String, _>("upstream_account_id"),
        "account-a"
    );
    assert_eq!(
        candidate.get::<String, _>("upstream_model"),
        "upstream-model-a"
    );
    assert_eq!(candidate.get::<i64, _>("scheduling_weight"), 100);
    let preserved_route_id: String =
        sqlx::query_scalar("SELECT id FROM model_routes WHERE id = 'route-a'")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(preserved_route_id, "route-a");
    let historical_route_id: Option<String> =
        sqlx::query_scalar("SELECT model_route_id FROM request_records WHERE id = 'request-a'")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(historical_route_id.as_deref(), Some("route-a"));
    let request_route_foreign_keys: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_foreign_key_list('request_records') WHERE \"table\" = 'model_routes'",
    )
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(
        request_route_foreign_keys, 0,
        "v42 request history stores a stable route ID but has no model_routes foreign key to retarget"
    );

    // The legacy uniqueness rule prevented independent rules from sharing a
    // public model and priority. Route identity and grants now disambiguate them.
    sqlx::query(
        "INSERT INTO model_routes (id, tenant_id, public_model, upstream_account_id, upstream_model, protocol, priority, enabled, created_at, updated_at) VALUES ('route-a-2', 'tenant-a', 'public-model', 'account-a', 'upstream-model-a-2', 'openai-responses', 10, 1, 101, 101)",
    )
    .execute(&database.pool)
    .await
    .expect("v43 must remove the legacy public model/priority uniqueness rule");

    sqlx::query(
        "INSERT INTO provider_groups (id, tenant_id, name, normalized_name, created_at, updated_at) VALUES ('provider-group-a', 'tenant-a', 'Codex', 'codex', 100, 100)",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO route_groups (id, tenant_id, name, normalized_name, created_at, updated_at) VALUES ('route-group-a', 'tenant-a', 'Default', 'default', 100, 100)",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO credential_groups (id, tenant_id, name, normalized_name, created_at, updated_at) VALUES ('credential-group-a', 'tenant-a', 'Reviewers', 'reviewers', 100, 100)",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    for statement in [
        "INSERT INTO upstream_account_provider_groups (tenant_id, provider_group_id, upstream_account_id, created_at) VALUES ('tenant-a', 'provider-group-a', 'account-a', 100)",
        "INSERT INTO model_route_group_memberships (tenant_id, route_group_id, model_route_id, created_at) VALUES ('tenant-a', 'route-group-a', 'route-a', 100)",
        "INSERT INTO credential_group_memberships (tenant_id, credential_group_id, key_id, created_at) VALUES ('tenant-a', 'credential-group-a', 'key-a', 100)",
        "INSERT INTO model_route_included_provider_groups (tenant_id, model_route_id, provider_group_id, created_at) VALUES ('tenant-a', 'route-a', 'provider-group-a', 100)",
        "INSERT INTO model_route_excluded_provider_groups (tenant_id, model_route_id, provider_group_id, created_at) VALUES ('tenant-a', 'route-a', 'provider-group-a', 100)",
        "INSERT INTO routing_grants (tenant_id, key_id, model_route_id, route_group_id, created_at) VALUES ('tenant-a', 'key-a', 'route-a', NULL, 100)",
        "INSERT INTO routing_grants (tenant_id, key_id, model_route_id, route_group_id, created_at) VALUES ('tenant-a', 'key-a', NULL, 'route-group-a', 100)",
    ] {
        sqlx::query(statement)
            .execute(&database.pool)
            .await
            .unwrap();
    }

    for invalid_grant in [
        "INSERT INTO routing_grants (tenant_id, key_id, model_route_id, route_group_id, created_at) VALUES ('tenant-a', 'key-a', NULL, NULL, 100)",
        "INSERT INTO routing_grants (tenant_id, key_id, model_route_id, route_group_id, created_at) VALUES ('tenant-a', 'key-a', 'route-a', 'route-group-a', 100)",
        "INSERT INTO routing_grants (tenant_id, key_id, model_route_id, route_group_id, created_at) VALUES ('tenant-a', 'key-a', 'route-a', NULL, 101)",
    ] {
        assert!(
            sqlx::query(invalid_grant)
                .execute(&database.pool)
                .await
                .is_err(),
            "invalid or duplicate routing grant was accepted"
        );
    }

    let cross_tenant_membership = sqlx::query(
        "INSERT INTO upstream_account_provider_groups (tenant_id, provider_group_id, upstream_account_id, created_at) VALUES ('tenant-a', 'provider-group-a', 'account-b', 100)",
    )
    .execute(&database.pool)
    .await;
    assert!(
        cross_tenant_membership.is_err(),
        "a provider group accepted another tenant's account"
    );

    sqlx::query("DELETE FROM provider_groups WHERE id = 'provider-group-a'")
        .execute(&database.pool)
        .await
        .unwrap();
    for table in [
        "upstream_account_provider_groups",
        "model_route_included_provider_groups",
        "model_route_excluded_provider_groups",
    ] {
        let remaining: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert_eq!(
            remaining, 0,
            "provider group delete did not cascade in {table}"
        );
    }

    // Credential grouping is deliberately classification-only: grants have no
    // credential_group_id column and deleting a credential group leaves grants intact.
    sqlx::query("DELETE FROM credential_groups WHERE id = 'credential-group-a'")
        .execute(&database.pool)
        .await
        .unwrap();
    let grants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM routing_grants WHERE tenant_id = 'tenant-a' AND key_id = 'key-a'",
    )
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(grants, 2);
}

#[tokio::test]
async fn postgres_routing_groups_drop_legacy_route_uniqueness_and_backfill_candidates() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    sqlx::any::install_default_drivers();
    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    let mut transaction = pool.begin().await.unwrap();
    let schema = format!("routing_groups_{}", Uuid::now_v7().simple());
    // Test-only identifier: a fixed literal prefix and a library-generated UUID.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL search_path = {schema}"
    )))
    .execute(&mut *transaction)
    .await
    .unwrap();
    for statement in [
        "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
        "CREATE TABLE tenants (id TEXT PRIMARY KEY)",
        "CREATE TABLE upstream_accounts (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL)",
        "CREATE TABLE model_routes (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, public_model TEXT NOT NULL, upstream_account_id TEXT NOT NULL, upstream_model TEXT NOT NULL, protocol TEXT NOT NULL, priority BIGINT NOT NULL, enabled BIGINT NOT NULL, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL, UNIQUE(tenant_id, public_model, protocol, priority))",
        "CREATE TABLE key_records (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, policy_json TEXT NOT NULL, created_at BIGINT NOT NULL)",
        "INSERT INTO tenants (id) VALUES ('tenant-a')",
        "INSERT INTO upstream_accounts (id, tenant_id) VALUES ('account-a', 'tenant-a')",
        "INSERT INTO model_routes (id, tenant_id, public_model, upstream_account_id, upstream_model, protocol, priority, enabled, created_at, updated_at) VALUES ('route-a', 'tenant-a', 'public-model', 'account-a', 'upstream-model-a', 'openai-responses', 10, 1, 100, 100)",
        "INSERT INTO key_records (id, tenant_id, policy_json, created_at) VALUES ('key-a', 'tenant-a', '{\"allowed_models\":[\"public-model\"]}', 100)",
    ] {
        sqlx::query(statement)
            .execute(&mut *transaction)
            .await
            .unwrap();
    }

    apply_migration_range(&mut transaction, POSTGRES_MIGRATIONS, 43, 43)
        .await
        .unwrap();
    super::super::migrations::backfill_routing_grants_from_legacy_policy(&mut transaction)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO model_routes (id, tenant_id, public_model, upstream_account_id, upstream_model, protocol, priority, enabled, created_at, updated_at) VALUES ('route-b', 'tenant-a', 'public-model', 'account-a', 'upstream-model-b', 'openai-responses', 10, 1, 101, 101)",
    )
    .execute(&mut *transaction)
    .await
    .expect("PostgreSQL v43 must drop the legacy route uniqueness constraint");
    let candidate: (String, String, i64) = sqlx::query_as(
        "SELECT upstream_account_id, upstream_model, scheduling_weight FROM model_route_upstream_accounts WHERE model_route_id = 'route-a'",
    )
    .fetch_one(&mut *transaction)
    .await
    .unwrap();
    assert_eq!(
        candidate,
        ("account-a".into(), "upstream-model-a".into(), 100)
    );
    apply_migration_range(&mut transaction, POSTGRES_MIGRATIONS, 52, 52)
        .await
        .unwrap();
    let policy_json: String =
        sqlx::query_scalar("SELECT policy_json FROM key_records WHERE id = 'key-a'")
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
    assert_eq!(policy_json, "{}");
    let frozen_grant: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM routing_grants WHERE key_id = 'key-a' AND model_route_id = 'route-a'",
    )
    .fetch_one(&mut *transaction)
    .await
    .unwrap();
    assert_eq!(frozen_grant, 1);
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn sqlite_legacy_model_policy_backfill_is_exact_bounded_and_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory
            .path()
            .join("routing-grants-backfill.db")
            .display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&database.pool)
        .await
        .unwrap();
    for statement in [
        "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
        "CREATE TABLE tenants (id TEXT PRIMARY KEY)",
        "CREATE TABLE upstream_accounts (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL)",
        "CREATE TABLE model_routes (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, public_model TEXT NOT NULL, upstream_account_id TEXT NOT NULL, upstream_model TEXT NOT NULL, protocol TEXT NOT NULL, priority BIGINT NOT NULL, enabled BIGINT NOT NULL, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL, UNIQUE(tenant_id, public_model, protocol, priority))",
        "CREATE TABLE key_records (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, policy_json TEXT NOT NULL, created_at BIGINT NOT NULL)",
        "INSERT INTO tenants (id) VALUES ('tenant-a')",
        "INSERT INTO upstream_accounts (id, tenant_id) VALUES ('account-a', 'tenant-a')",
        "INSERT INTO model_routes VALUES ('route-a', 'tenant-a', 'model-a', 'account-a', 'upstream-a', 'openai-responses', 10, 1, 100, 100)",
        "INSERT INTO model_routes VALUES ('route-b', 'tenant-a', 'model-b', 'account-a', 'upstream-b', 'openai-responses', 10, 1, 100, 100)",
        "INSERT INTO key_records VALUES ('key-empty', 'tenant-a', '{\"allowed_models\":[]}', 100)",
        "INSERT INTO key_records VALUES ('key-exact', 'tenant-a', '{\"allowed_models\":[\"model-a\",\"model-a\"]}', 100)",
        "INSERT INTO key_records VALUES ('key-wildcard', 'tenant-a', '{\"allowed_models\":[\"*\"]}', 100)",
    ] {
        sqlx::query(statement)
            .execute(&database.pool)
            .await
            .unwrap();
    }
    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 43, 43)
        .await
        .unwrap();
    let inserted =
        super::super::migrations::backfill_routing_grants_from_legacy_policy(&mut transaction)
            .await
            .unwrap();
    assert_eq!(inserted, 3);
    transaction.commit().await.unwrap();

    let exact_routes: Vec<String> = sqlx::query_scalar(
        "SELECT model_route_id FROM routing_grants WHERE key_id = 'key-exact' ORDER BY model_route_id",
    )
    .fetch_all(&database.pool)
    .await
    .unwrap();
    assert_eq!(exact_routes, ["route-a"]);
    let wildcard_routes: Vec<String> = sqlx::query_scalar(
        "SELECT model_route_id FROM routing_grants WHERE key_id = 'key-wildcard' ORDER BY model_route_id",
    )
    .fetch_all(&database.pool)
    .await
    .unwrap();
    assert_eq!(wildcard_routes, ["route-a", "route-b"]);
    let empty_grants: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM routing_grants WHERE key_id = 'key-empty'")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(empty_grants, 0);

    sqlx::query(
        "INSERT INTO model_routes VALUES ('route-future', 'tenant-a', 'model-a', 'account-a', 'upstream-future', 'openai-responses', 10, 1, 200, 200)",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    let future_grants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM routing_grants WHERE model_route_id = 'route-future'",
    )
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(
        future_grants, 0,
        "a route created after the upgrade boundary must require an explicit grant"
    );

    sqlx::query(
        "INSERT INTO key_records VALUES ('key-z-bad', 'tenant-a', '{\"allowed_models\":\"*\"}', 300)",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    let mut transaction = database.pool.begin().await.unwrap();
    let error =
        super::super::migrations::backfill_routing_grants_from_legacy_policy(&mut transaction)
            .await
            .expect_err("malformed legacy policy must abort the migration");
    assert!(error.to_string().contains("invalid legacy routing policy"));
    transaction.rollback().await.unwrap();
    let bad_grants: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM routing_grants WHERE key_id = 'key-z-bad'")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(bad_grants, 0);

    sqlx::query("UPDATE key_records SET policy_json = 'not-json' WHERE id = 'key-z-bad'")
        .execute(&database.pool)
        .await
        .unwrap();
    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 52, 52)
        .await
        .expect_err("malformed historical policy must abort v52");
    transaction.rollback().await.unwrap();
    let prematurely_applied: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations WHERE version = 52")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(prematurely_applied, 0);

    sqlx::query("DELETE FROM key_records WHERE id = 'key-z-bad'")
        .execute(&database.pool)
        .await
        .unwrap();
    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 52, 52)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    for key_id in ["key-empty", "key-exact", "key-wildcard"] {
        let policy_json: String =
            sqlx::query_scalar("SELECT policy_json FROM key_records WHERE id = $1")
                .bind(key_id)
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert_eq!(
            policy_json, "{}",
            "v52 must retire the legacy policy source"
        );
    }
    let exact_routes_after_v52: Vec<String> = sqlx::query_scalar(
        "SELECT model_route_id FROM routing_grants WHERE key_id = 'key-exact' ORDER BY model_route_id",
    )
    .fetch_all(&database.pool)
    .await
    .unwrap();
    assert_eq!(exact_routes_after_v52, ["route-a"]);
    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 52, 52)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    let applied: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations WHERE version = 52")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(applied, 1, "v52 must be idempotent");
}
