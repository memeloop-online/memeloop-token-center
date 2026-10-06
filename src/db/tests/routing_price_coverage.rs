use super::super::*;

#[test]
fn routing_price_coverage_cli_contracts() {
    let result = std::process::Command::new("node")
        .args(["--test", "tests/ops/routing-price-coverage.test.ts"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

async fn coverage_report(database_url: &str, schema: &str, currencies: &[&str]) -> (i32, Value) {
    let parsed = url::Url::parse(database_url).unwrap();
    let mut command = tokio::process::Command::new("node");
    command
        .arg("scripts/maintenance/check-routing-price-coverage.ts")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("PGHOST", parsed.host_str().unwrap())
        .env("PGPORT", parsed.port().unwrap_or(5432).to_string())
        .env("PGUSER", parsed.username())
        .env("PGPASSWORD", parsed.password().unwrap_or_default())
        .env("PGDATABASE", parsed.path().trim_start_matches('/'))
        .env(
            "PGOPTIONS",
            format!("-csearch_path={schema} -cdefault_transaction_read_only=on"),
        );
    for currency in currencies {
        command.args(["--currency", currency]);
    }
    let result = command.output().await.unwrap();
    let code = result.status.code().unwrap();
    assert_ne!(code, 1, "{}", String::from_utf8_lossy(&result.stderr));
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["read_only"], true);
    for forbidden in [
        "secret-model",
        "secret-alias",
        "credential_ciphertext",
        "input_micros",
    ] {
        assert!(!String::from_utf8_lossy(&result.stdout).contains(forbidden));
    }
    (code, report)
}

async fn insert_candidate(database: &Database, tenant: Uuid, route: Uuid, model: &str) -> Uuid {
    let account = Uuid::now_v7();
    sqlx::query("INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, created_at, updated_at) VALUES ($1, $2, $1, 'openai', 'api_key', '{}', 'active', 1, 0, 0)")
        .bind(account.to_string()).bind(tenant.to_string()).execute(&database.pool).await.unwrap();
    sqlx::query("INSERT INTO model_route_upstream_accounts (tenant_id, model_route_id, upstream_account_id, upstream_model, created_at, catalog_policy) VALUES ($1, $2, $3, $4, 0, 'required')")
        .bind(tenant.to_string()).bind(route.to_string()).bind(account.to_string()).bind(model)
        .execute(&database.pool).await.unwrap();
    account
}

#[tokio::test]
async fn postgres_price_coverage_is_read_only_and_detects_alias_only_failover() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    let schema = format!("price_coverage_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&database_url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect_with_max(isolated.as_str(), 2)
        .await
        .unwrap();
    database.migrate().await.unwrap();
    let tenant = Uuid::now_v7();
    let route = Uuid::now_v7();
    sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $1, 0)")
        .bind(tenant.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO model_routes (id, tenant_id, public_model, upstream_account_id, upstream_model, protocol, priority, enabled, created_at, updated_at) VALUES ($1, $2, 'secret-alias', $3, 'secret-model-group', 'openai', 0, 1, 0, 0)")
        .bind(route.to_string()).bind(tenant.to_string()).bind(Uuid::now_v7().to_string())
        .execute(&database.pool).await.unwrap();
    let primary = insert_candidate(&database, tenant, route, "secret-model-primary").await;
    let fallback = insert_candidate(&database, tenant, route, "secret-model-fallback").await;

    let (code, report) = coverage_report(&database_url, &schema, &[]).await;
    assert_eq!(code, 2);
    assert_eq!(report["gap_count"], 2);
    assert_eq!(report["gap_samples"][0]["reason"], "currency_scope_unknown");

    sqlx::query("INSERT INTO credit_accounts (id, tenant_id, principal_id, currency, available_micros, reserved_micros, created_at, updated_at) VALUES ($1, $2, $3, 'usd', 0, 0, 0, 0)")
        .bind(Uuid::now_v7().to_string()).bind(tenant.to_string()).bind(Uuid::now_v7().to_string())
        .execute(&database.pool).await.unwrap();
    database
        .upsert_model_price("secret-alias", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    database
        .upsert_model_price("secret-model-primary", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    assert!(
        database
            .model_price("secret-model-primary", "usd")
            .await
            .is_ok()
    );
    assert!(matches!(
        database.model_price("secret-model-fallback", "usd").await,
        Err(AppError::UnpricedModel)
    ));
    let (code, report) = coverage_report(&database_url, &schema, &[]).await;
    assert_eq!(code, 2);
    assert_eq!(report["candidate_count"], 2);
    assert_eq!(report["gap_count"], 1);
    assert_eq!(
        report["gap_samples"][0]["upstream_account_id"],
        fallback.to_string()
    );
    assert_eq!(report["gap_samples"][0]["public_alias_price_present"], true);

    database
        .upsert_model_price("SECRET-model-fallback", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    assert_eq!(coverage_report(&database_url, &schema, &[]).await.0, 2);
    database
        .upsert_model_price("secret-model-fallback", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    assert_eq!(coverage_report(&database_url, &schema, &[]).await.0, 0);
    assert_eq!(
        coverage_report(&database_url, &schema, &["CNY"]).await.1["gap_count"],
        2
    );

    sqlx::query("UPDATE model_prices SET id = 'not-a-uuid' WHERE model = 'secret-model-fallback'")
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        database
            .model_price("secret-model-fallback", "USD")
            .await
            .is_err()
    );
    assert_eq!(
        coverage_report(&database_url, &schema, &[]).await.1["gap_samples"][0]["reason"],
        "invalid_actual_price"
    );
    sqlx::query("UPDATE model_prices SET id = $1 WHERE model = 'secret-model-fallback'")
        .bind(Uuid::now_v7().to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE model_price_tiers SET cached_input_micros_per_million = -1 WHERE model = 'secret-model-fallback'")
        .execute(&database.pool).await.unwrap();
    assert_eq!(coverage_report(&database_url, &schema, &[]).await.0, 2);
    sqlx::query("DELETE FROM model_price_tiers WHERE model = 'secret-model-fallback'")
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        database
            .model_price("secret-model-fallback", "USD")
            .await
            .is_ok()
    );
    assert_eq!(coverage_report(&database_url, &schema, &[]).await.0, 0);

    let group = Uuid::now_v7();
    sqlx::query("INSERT INTO provider_groups (id, tenant_id, name, normalized_name, created_at, updated_at) VALUES ($1, $2, 'group', 'group', 0, 0)")
        .bind(group.to_string()).bind(tenant.to_string()).execute(&database.pool).await.unwrap();
    sqlx::query("INSERT INTO upstream_account_provider_groups (tenant_id, provider_group_id, upstream_account_id, created_at) VALUES ($1, $2, $3, 0)")
        .bind(tenant.to_string()).bind(group.to_string()).bind(primary.to_string())
        .execute(&database.pool).await.unwrap();
    sqlx::query("INSERT INTO model_route_included_provider_groups (tenant_id, model_route_id, provider_group_id, created_at) VALUES ($1, $2, $3, 0)")
        .bind(tenant.to_string()).bind(route.to_string()).bind(group.to_string())
        .execute(&database.pool).await.unwrap();
    let (_, report) = coverage_report(&database_url, &schema, &[]).await;
    assert_eq!(report["candidate_count"], 3);
    assert_eq!(report["gap_count"], 1);
    sqlx::query("INSERT INTO model_route_excluded_provider_groups (tenant_id, model_route_id, provider_group_id, created_at) VALUES ($1, $2, $3, 0)")
        .bind(tenant.to_string()).bind(route.to_string()).bind(group.to_string())
        .execute(&database.pool).await.unwrap();
    let (code, report) = coverage_report(&database_url, &schema, &[]).await;
    assert_eq!(code, 0);
    assert_eq!(report["candidate_count"], 1);

    assert_eq!(coverage_report(&database_url, &schema, &[]).await.0, 0);
    let maximum: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_migrations")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(maximum, 117);
    sqlx::query("UPDATE model_routes SET enabled = 0 WHERE id = $1")
        .bind(route.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(coverage_report(&database_url, &schema, &[]).await.0, 2);
    database.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}
