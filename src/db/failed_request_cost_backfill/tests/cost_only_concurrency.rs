use super::*;

async fn exercise_concurrent_contribution(
    fixture: &Fixture,
    database_url: &str,
    observer: &AnyPool,
    generation: bool,
    writer_first: bool,
) {
    let (historical_id, historical_cost) = seed_historical_case(
        fixture,
        499,
        Some("client_cancelled"),
        Some(RequestUsageBasis::ContractCeiling),
    )
    .await;
    let created_at: i64 =
        sqlx::query_scalar("SELECT created_at FROM request_records WHERE id = $1")
            .bind(historical_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    let manifest = FailedRequestCostBackfillExactManifest {
        tenant_id: fixture.key.tenant_id.to_string(),
        currency: "USD".into(),
        requests: vec![FailedRequestCostBackfillExpectedRequest {
            request_id: historical_id.to_string(),
            created_at,
            expected_cost_micros: historical_cost,
            expected_status_code: 499,
            expected_error_code: Some("client_cancelled".into()),
        }],
    };
    let writer = Database::connect_with_max(database_url, 1).await.unwrap();
    let writer_id = Uuid::now_v7();
    let reservation = if generation {
        let upstream = writer
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: fixture.model.clone(),
                    name: "cost-only-generation-writer".into(),
                    driver: "comfyui".into(),
                    config: serde_json::json!({"base_url": "http://127.0.0.1:1"}),
                    credential: UpstreamCredential::None,
                    oauth_session_id: None,
                    oauth_driver: None,
                    oauth_refresh_url: None,
                },
                b"failed cost backfill test pepper longer than thirty-two bytes",
            )
            .await
            .unwrap();
        let price = writer
            .upsert_generation_price(&fixture.model, "USD", "job", Decimal::new(25, 2))
            .await
            .unwrap();
        let reservation = writer
            .reserve_usage(&fixture.key, &price.reservation_price().unwrap(), 0, 2)
            .await
            .unwrap();
        writer
            .create_generation_job(CreateGenerationJobInput {
                job_id: writer_id,
                key: fixture.key.clone(),
                upstream_account_id: upstream.id,
                reservation,
                public_model: fixture.model.clone(),
                upstream_model: "workflow-v1".into(),
                driver: "comfyui".into(),
                request_object: "objects/blake3/cost-only-generation-writer".into(),
                estimated_units: 2,
                billing_unit: price.billing_unit,
                micros_per_unit: price.micros_per_unit,
            })
            .await
            .unwrap();
        assert_eq!(
            writer
                .claim_generation_job("cost-only-contribution-worker")
                .await
                .unwrap()
                .unwrap()
                .job_id,
            writer_id
        );
        None
    } else {
        Some(
            writer
                .start_proxy_request(StartProxyRequest {
                    request_id: writer_id,
                    key: &fixture.key,
                    price: &fixture.price,
                    input_token_ceiling: 10,
                    output_token_ceiling: 10,
                    protocol: "openai",
                    model: &fixture.model,
                    request_object: "gap://failed-cost-backfill/concurrent-contribution/request",
                    upstream_account_id: None,
                    model_route_id: None,
                })
                .await
                .unwrap(),
        )
    };
    if !generation {
        sqlx::query("UPDATE request_records SET created_at = $1 WHERE id = $2")
            .bind(created_at)
            .bind(writer_id.to_string())
            .execute(&writer.pool)
            .await
            .unwrap();
    }
    let writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&writer.pool)
        .await
        .unwrap();
    let correction = Database::connect_with_max(database_url, 1).await.unwrap();
    let correction_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&correction.pool)
        .await
        .unwrap();
    let writer_gate = Database::connect_with_max(database_url, 1).await.unwrap();
    let correction_gate = Database::connect_with_max(database_url, 1).await.unwrap();
    let mut writer_gate_transaction = writer_gate.begin_write_transaction().await.unwrap();
    let mut correction_gate_transaction = correction_gate.begin_write_transaction().await.unwrap();
    let writer_gate_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *writer_gate_transaction)
        .await
        .unwrap();
    let correction_gate_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *correction_gate_transaction)
        .await
        .unwrap();
    let mut lock_bytes = [0_u8; 8];
    lock_bytes.copy_from_slice(&writer_id.as_bytes()[8..]);
    let writer_lock = (i64::from_be_bytes(lock_bytes) & i64::MAX) >> 1;
    let correction_lock = writer_lock + 1;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(writer_lock)
        .execute(&mut *writer_gate_transaction)
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(correction_lock)
        .execute(&mut *correction_gate_transaction)
        .await
        .unwrap();
    let table = if generation {
        "session_usage_totals"
    } else {
        "usage_daily_aggregates"
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION mtc_cost_only_contribution_gate() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.requests > OLD.requests THEN PERFORM pg_advisory_xact_lock({writer_lock}); ELSE PERFORM pg_advisory_xact_lock({correction_lock}); END IF; RETURN NEW; END $$"
    ))).execute(&fixture.database.pool).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER mtc_cost_only_contribution_gate AFTER UPDATE ON {table} FOR EACH ROW EXECUTE FUNCTION mtc_cost_only_contribution_gate()"
    ))).execute(&fixture.database.pool).await.unwrap();
    let tenant_id = fixture.key.tenant_id;
    let writer_operation = async move {
        let cost = if generation {
            writer
                .finish_generation_job(FinishGenerationJobInput {
                    job_id: writer_id,
                    worker_id: "cost-only-contribution-worker",
                    status: "failed",
                    billed_units: 1,
                    error_code: Some("upstream_usage_exceeds_contract"),
                    assets: &[],
                    staged_assets: None,
                })
                .await?
        } else {
            let reservation = reservation.unwrap();
            match writer
                .finish_proxy_request(FinishProxyRequest {
                    terminal_cause: None,
                    usage_basis: Some(RequestUsageBasis::ProviderReported),
                    first_output_ms: None,
                    generation_duration_ms: None,
                    request_id: writer_id,
                    tenant_id,
                    reservation: &reservation,
                    input_token_ceiling: 10,
                    output_token_ceiling: 10,
                    requested_service_tier: None,
                    status_code: 499,
                    duration_ms: 25,
                    usage: TokenUsage {
                        input_tokens: 7,
                        output_tokens: 3,
                        ..TokenUsage::default()
                    },
                    error_code: Some("client_cancelled"),
                    response_object: "gap://failed-cost-backfill/concurrent-contribution/response",
                    routing_session_id: None,
                    routing_terminal_observed_at: None,
                    conversation: None,
                })
                .await?
            {
                FinishProxyRequestResult::Finished { cost_micros, .. }
                | FinishProxyRequestResult::AlreadyFinished { cost_micros, .. } => cost_micros,
            }
        };
        writer.close().await;
        Ok::<i64, AppError>(cost)
    };
    let correction_manifest = manifest.clone();
    let correction_operation = async move {
        let report = correction
            .backfill_failed_request_costs_exact_cost_only(correction_manifest, true)
            .await;
        correction.close().await;
        report
    };
    let (report, writer_cost) = if writer_first {
        let writer_task = tokio::spawn(writer_operation);
        wait_for_postgres_blocker(observer, writer_pid, writer_gate_pid).await;
        let correction_task = tokio::spawn(correction_operation);
        wait_for_postgres_blocker(observer, correction_pid, writer_pid).await;
        writer_gate_transaction.commit().await.unwrap();
        let writer_cost = writer_task.await.unwrap().unwrap();
        wait_for_postgres_blocker(observer, correction_pid, correction_gate_pid).await;
        let financial_before = financial_rows(fixture).await;
        let non_cost_before = statistics_non_cost_rows(fixture).await;
        correction_gate_transaction.commit().await.unwrap();
        let report = correction_task.await.unwrap().unwrap();
        assert_eq!(financial_rows(fixture).await, financial_before);
        assert_eq!(statistics_non_cost_rows(fixture).await, non_cost_before);
        (report, writer_cost)
    } else {
        let financial_before = financial_rows(fixture).await;
        let non_cost_before = statistics_non_cost_rows(fixture).await;
        let correction_task = tokio::spawn(correction_operation);
        wait_for_postgres_blocker(observer, correction_pid, correction_gate_pid).await;
        let writer_task = tokio::spawn(writer_operation);
        wait_for_postgres_blocker(observer, writer_pid, correction_pid).await;
        correction_gate_transaction.commit().await.unwrap();
        let report = correction_task.await.unwrap().unwrap();
        wait_for_postgres_blocker(observer, writer_pid, writer_gate_pid).await;
        assert_eq!(financial_rows(fixture).await, financial_before);
        assert_eq!(statistics_non_cost_rows(fixture).await, non_cost_before);
        writer_gate_transaction.commit().await.unwrap();
        (report, writer_task.await.unwrap().unwrap())
    };
    assert_eq!(report.backfill.changed_rows, 1);
    assert!(writer_cost > 0);
    assert_eq!(
        aggregate_costs(fixture).await,
        if generation {
            vec![
                0,
                0,
                writer_cost,
                writer_cost,
                writer_cost,
                writer_cost,
                writer_cost,
            ]
        } else {
            vec![writer_cost; 7]
        }
    );
    for (index, table) in cost_only::TABLES.iter().enumerate() {
        let requests: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT CAST(SUM(requests) AS BIGINT) FROM {table} WHERE key_id = $1"
        )))
        .bind(fixture.key.key_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
        assert_eq!(
            requests,
            if generation && index < 2 { 1 } else { 2 },
            "{table}"
        );
        if !generation || index == 2 {
            let dimensions: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT CAST(COUNT(*) AS BIGINT) FROM {table} WHERE key_id = $1"
            )))
            .bind(fixture.key.key_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
            assert_eq!(dimensions, 1, "{table} must share the writer dimension");
        }
    }
    if generation {
        let units: i64 = sqlx::query_scalar(
            "SELECT generation_units FROM session_usage_totals WHERE key_id = $1",
        )
        .bind(fixture.key.key_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
        assert_eq!(units, 1);
        for table in ["generation_stats_facts", "generation_daily_aggregates"] {
            let cost: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT cost_micros FROM {table} WHERE key_id = $1"
            )))
            .bind(fixture.key.key_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
            assert_eq!(cost, writer_cost);
        }
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER mtc_cost_only_contribution_gate ON {table}"
    )))
    .execute(&fixture.database.pool)
    .await
    .unwrap();
    sqlx::query("DROP FUNCTION mtc_cost_only_contribution_gate()")
        .execute(&fixture.database.pool)
        .await
        .unwrap();
    let financial_before_replay = financial_rows(fixture).await;
    let non_cost_before_replay = statistics_non_cost_rows(fixture).await;
    let costs_before_replay = aggregate_costs(fixture).await;
    let replay = fixture
        .database
        .backfill_failed_request_costs_exact_cost_only(manifest, true)
        .await
        .unwrap();
    assert_eq!(replay.already_projected_rows, 1);
    assert_eq!(replay.backfill.changed_rows, 0);
    assert_eq!(financial_rows(fixture).await, financial_before_replay);
    assert_eq!(
        statistics_non_cost_rows(fixture).await,
        non_cost_before_replay
    );
    assert_eq!(aggregate_costs(fixture).await, costs_before_replay);
    writer_gate.close().await;
    correction_gate.close().await;
}

#[tokio::test]
async fn postgres_cost_only_concurrent_request_and_generation_preserve_cost_count_and_finance() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!(
            "MTC_TEST_POSTGRES_URL is unset; skipping cost-only concurrent contribution contract"
        );
        return;
    };
    sqlx::any::install_default_drivers();
    let admin = AnyPool::connect(&database_url).await.unwrap();
    let schema = format!("cost_only_concurrent_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = Url::parse(&database_url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-c search_path={schema}"));
    for generation in [false, true] {
        for writer_first in [true, false] {
            let fixture = fixture(isolated.as_str(), None).await;
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                exercise_concurrent_contribution(
                    &fixture,
                    isolated.as_str(),
                    &admin,
                    generation,
                    writer_first,
                ),
            )
            .await
            .unwrap();
            fixture.database.close().await;
        }
    }
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
