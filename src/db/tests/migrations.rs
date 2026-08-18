use super::super::*;

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
                "INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, 'boundary-model', $5, $6, $7, $8, $9, $10, 1, 2, $11)",
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
                "INSERT INTO request_daily_aggregates (tenant_id, key_id, day_bucket, model, protocol, status_class, error_code, upstream_account_id, model_route_id, requests, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, 'boundary-model', $4, $5, $6, $7, $8, 1, 1, 2, $9)",
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
    assert_eq!(stats.summary.total_cost, "0.0006");
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
    assert_eq!(fact_filtered.summary.total_cost, "0.0003");
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
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query(&format!("SET LOCAL search_path = {schema}"))
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
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query(&format!("SET LOCAL search_path = {schema}"))
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
        sqlx::query(&format!(
            "CREATE TABLE {table}_default PARTITION OF {table} DEFAULT"
        ))
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
