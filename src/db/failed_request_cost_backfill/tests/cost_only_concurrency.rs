use super::*;
use std::io::Write;

fn record_stage(case: &str, stage: &str, outcome: &str) {
    writeln!(
        std::io::stderr().lock(),
        "cost-only PG case={case} stage={stage} {outcome}"
    )
    .unwrap();
}

fn unexpected_completion<Output>(
    outcome: Result<Result<Output, AppError>, tokio::task::JoinError>,
    case: &str,
    stage: &str,
) -> ! {
    let category = match outcome {
        Ok(Ok(_)) => "completed_before_expected_checkpoint",
        Ok(Err(error)) => error.diagnostic_category(),
        Err(_) => "task_join_failure",
    };
    panic!("cost-only PG case={case} stage={stage} outcome={category}");
}

async fn observe_blocker<Output>(
    observer: &AnyPool,
    waiter_pid: i32,
    blocker_pid: i32,
    task: &mut tokio::task::JoinHandle<Result<Output, AppError>>,
    case: &str,
    stage: &str,
) {
    record_stage(case, stage, "waiting");
    tokio::select! {
        biased;
        outcome = task => unexpected_completion(outcome, case, stage),
        outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND state = 'active' AND wait_event_type = 'Lock' AND $2 = ANY(pg_blocking_pids(pid)))",
                )
                .bind(waiter_pid)
                .bind(blocker_pid)
                .fetch_one(observer)
                .await
                .unwrap();
                if waiting {
                    return;
                }
                tokio::task::yield_now().await;
            }
        }) => {
            if outcome.is_err() {
                let activity = sqlx::query(
                    "SELECT state, COALESCE(wait_event_type, '') AS wait_type, COALESCE(wait_event, '') AS wait_event, CAST(pg_blocking_pids(pid) AS TEXT) AS blockers, COALESCE((SELECT string_agg(locktype || ':' || mode, ',' ORDER BY locktype, mode) FROM pg_locks WHERE pg_locks.pid = pg_stat_activity.pid AND NOT granted), '') AS waiting_locks FROM pg_stat_activity WHERE pid = $1",
                )
                .bind(waiter_pid)
                .fetch_one(observer)
                .await
                .unwrap();
                panic!(
                    "cost-only PG case={case} stage={stage} expected_blocker={blocker_pid} state={} wait_type={} wait_event={} blockers={} waiting_locks={}",
                    activity.try_get::<String, _>("state").unwrap(),
                    activity.try_get::<String, _>("wait_type").unwrap(),
                    activity.try_get::<String, _>("wait_event").unwrap(),
                    activity.try_get::<String, _>("blockers").unwrap(),
                    activity.try_get::<String, _>("waiting_locks").unwrap(),
                );
            }
            record_stage(case, stage, "reached");
        },
    }
}

async fn observe_precommit(
    reached: &mut tokio::sync::oneshot::Receiver<()>,
    task: &mut tokio::task::JoinHandle<Result<FailedRequestCostBackfillExactReport, AppError>>,
    case: &str,
) {
    tokio::select! {
        biased;
        outcome = task => unexpected_completion(outcome, case, "correction_precommit"),
        outcome = reached => outcome.expect("precommit notification must be acknowledged"),
    }
    record_stage(case, "correction_precommit", "reached");
}

async fn exercise_concurrent_contribution(
    fixture: &Fixture,
    database_url: &str,
    observer: &AnyPool,
    generation: bool,
    writer_first: bool,
) {
    let case = match (generation, writer_first) {
        (false, true) => "request_writer_first",
        (false, false) => "request_correction_first",
        (true, true) => "generation_writer_first",
        (true, false) => "generation_correction_first",
    };
    record_stage(case, "fixture", "begin");
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
        let owned: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM generation_jobs job JOIN usage_reservations reservation ON reservation.id = job.reservation_id AND reservation.key_id = job.key_id JOIN credit_accounts account ON account.id = reservation.account_id WHERE job.id = $1 AND job.tenant_id = $2 AND job.key_id = $3 AND account.id = $4 AND account.currency = 'USD' AND reservation.status = 'reserved' AND job.status = 'queued' AND job.lease_owner = 'cost-only-contribution-worker' AND job.estimated_units = 2 AND job.micros_per_unit_snapshot > 0",
        )
        .bind(writer_id.to_string())
        .bind(fixture.key.tenant_id.to_string())
        .bind(fixture.key.key_id.to_string())
        .bind(fixture.account_id.to_string())
        .fetch_one(&writer.pool).await.unwrap();
        assert_eq!(
            owned, 1,
            "generation fixture must match real worker ownership"
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
        let mut transaction = writer.begin_write_transaction().await.unwrap();
        for table in ["request_records", "request_record_locators"] {
            let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {table} SET created_at = $1 WHERE id = $2"
            )))
            .bind(created_at)
            .bind(writer_id.to_string())
            .execute(&mut *transaction)
            .await
            .unwrap();
            assert_eq!(updated.rows_affected(), 1, "{table} fixture identity");
        }
        transaction.commit().await.unwrap();
        let matching_locator: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM request_records request JOIN request_record_locators locator ON locator.id = request.id AND locator.created_at = request.created_at AND locator.tenant_id = request.tenant_id AND locator.key_id = request.key_id JOIN usage_reservations reservation ON reservation.id = request.reservation_id AND reservation.key_id = request.key_id WHERE request.id = $1 AND request.created_at = $2 AND request.tenant_id = $3 AND request.key_id = $4 AND reservation.account_id = $5 AND reservation.status = 'reserved' AND reservation.reserved_tokens = 20 AND request.completed_at IS NULL",
        )
        .bind(writer_id.to_string())
        .bind(created_at)
        .bind(fixture.key.tenant_id.to_string())
        .bind(fixture.key.key_id.to_string())
        .bind(fixture.account_id.to_string())
        .fetch_one(&writer.pool)
        .await
        .unwrap();
        assert_eq!(
            matching_locator, 1,
            "writer fixture must retain its locator"
        );
    }
    record_stage(case, "production_lookup", "matched");
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
    let mut writer_gate_transaction = writer_gate.begin_write_transaction().await.unwrap();
    let writer_gate_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *writer_gate_transaction)
        .await
        .unwrap();
    let mut lock_bytes = [0_u8; 8];
    lock_bytes.copy_from_slice(&writer_id.as_bytes()[8..]);
    let writer_lock = (i64::from_be_bytes(lock_bytes) & i64::MAX) >> 1;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(writer_lock)
        .execute(&mut *writer_gate_transaction)
        .await
        .unwrap();
    let table = if generation {
        "session_usage_totals"
    } else {
        "usage_daily_aggregates"
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION mtc_cost_only_contribution_gate() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.requests > OLD.requests THEN PERFORM pg_advisory_xact_lock({writer_lock}); END IF; RETURN NEW; END $$"
    ))).execute(&fixture.database.pool).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER mtc_cost_only_contribution_gate AFTER UPDATE ON {table} FOR EACH ROW EXECUTE FUNCTION mtc_cost_only_contribution_gate()"
    ))).execute(&fixture.database.pool).await.unwrap();
    let tenant_id = fixture.key.tenant_id;
    let writer_database = writer.clone();
    let writer_operation = async move {
        let writer = writer_database;
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
        Ok::<i64, AppError>(cost)
    };
    let correction_manifest = manifest.clone();
    let correction_database = correction.clone();
    let (precommit_sender, mut precommit_reached) = tokio::sync::oneshot::channel();
    let (commit_release, commit_receiver) = tokio::sync::oneshot::channel();
    let correction_operation = async move {
        correction_database
            .cost_only_before_commit_for_test(correction_manifest, async move {
                precommit_sender
                    .send(())
                    .expect("precommit observer must exist");
                commit_receiver
                    .await
                    .expect("commit must be explicitly released");
            })
            .await
    };
    let (report, writer_cost) = if writer_first {
        let mut writer_task = tokio::spawn(writer_operation);
        observe_blocker(
            observer,
            writer_pid,
            writer_gate_pid,
            &mut writer_task,
            case,
            "writer_aggregate_gate",
        )
        .await;
        let mut correction_task = tokio::spawn(correction_operation);
        observe_blocker(
            observer,
            correction_pid,
            writer_pid,
            &mut correction_task,
            case,
            "correction_waits_writer_row",
        )
        .await;
        writer_gate_transaction.commit().await.unwrap();
        let writer_cost = writer_task.await.unwrap().unwrap();
        observe_precommit(&mut precommit_reached, &mut correction_task, case).await;
        let financial_before = financial_rows(fixture).await;
        let non_cost_before = statistics_non_cost_rows(fixture).await;
        commit_release.send(()).unwrap();
        let report = correction_task.await.unwrap().unwrap();
        assert_eq!(financial_rows(fixture).await, financial_before);
        assert_eq!(statistics_non_cost_rows(fixture).await, non_cost_before);
        (report, writer_cost)
    } else {
        let financial_before = financial_rows(fixture).await;
        let non_cost_before = statistics_non_cost_rows(fixture).await;
        let mut correction_task = tokio::spawn(correction_operation);
        observe_precommit(&mut precommit_reached, &mut correction_task, case).await;
        let mut writer_task = tokio::spawn(writer_operation);
        observe_blocker(
            observer,
            writer_pid,
            correction_pid,
            &mut writer_task,
            case,
            "writer_waits_correction_row",
        )
        .await;
        commit_release.send(()).unwrap();
        let report = correction_task.await.unwrap().unwrap();
        observe_blocker(
            observer,
            writer_pid,
            writer_gate_pid,
            &mut writer_task,
            case,
            "writer_aggregate_gate_after_correction",
        )
        .await;
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
    record_stage(case, "cost_count_finance_and_replay", "PASS");
    writer.close().await;
    correction.close().await;
    writer_gate.close().await;
}

pub(super) async fn exercise_postgres_contribution_matrix(database_url: &str, admin: &AnyPool) {
    record_stage("matrix", "existing_contracts", "PASS");
    let schema = format!("cost_only_concurrent_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(admin)
        .await
        .unwrap();
    let mut isolated = Url::parse(database_url).unwrap();
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
                    admin,
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
        .execute(admin)
        .await
        .unwrap();
    record_stage("matrix", "four_cases_and_cleanup", "PASS");
}
