use super::*;
use crate::db::test_support::PostgresTestDatabase;

fn stats_window() -> StatsFilter {
    let now = unix_millis();
    StatsFilter {
        from_created_at: Some(now - 3_600_000),
        to_created_at: Some(now + 3_600_000),
        ..Default::default()
    }
}

async fn historical_unknown(database: &Database) -> PendingProxySettlementClaim {
    let (key, reservation, request_id) = admitted(database, true).await;
    let owner = ProxyRequestOwner {
        request_id,
        tenant_id: key.tenant_id,
        key_id: key.key_id,
        reservation_id: reservation.id,
        token: Uuid::now_v7(),
    };
    assert!(database.acquire_proxy_request_owner(&owner).await.unwrap());
    let claim = database
        .claim_pending_proxy_settlement(&owner, 10, 10, None)
        .await
        .unwrap();
    database
        .record_pending_proxy_operational_terminal(
            &claim,
            Uuid::now_v7(),
            &PendingProxyOperationalTerminal {
                status_code: 502,
                error_code: Some("upstream_read".into()),
                terminal_cause: Some("upstream_stream_read_error".into()),
                stage: Some("response_delivery".into()),
                completed_at: unix_millis(),
                duration_ms: 10,
                first_output_ms: None,
                generation_duration_ms: None,
                response_object: "gap://terminal-recovery/response".into(),
            },
            ProxyRequestUpstreamAttribution::KeepSelected,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE pending_proxy_settlements SET unknown_reason = 'authoritative_usage_unavailable' WHERE request_id = $1")
        .bind(request_id.to_string()).execute(&database.pool).await.unwrap();
    claim
}

async fn recovery_pages_contract(database: &Database) {
    let first = historical_unknown(database).await;
    let late = historical_unknown(database).await;
    sqlx::query("UPDATE pending_proxy_settlements SET unknown_reason = NULL WHERE request_id = $1")
        .bind(late.request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    for index in 0..65 {
        let request_id = format!("00000000-0000-0000-0000-{index:012}");
        sqlx::query("INSERT INTO pending_proxy_settlements (request_id, tenant_id, key_id, account_id, reservation_id, owner_token, fence, contract_digest, contract_json, state, created_at, updated_at) SELECT $1, tenant_id, key_id, account_id, $1, owner_token, fence, contract_digest, contract_json, 'pending', created_at, updated_at FROM pending_proxy_settlements WHERE request_id = $2")
            .bind(request_id).bind(first.request_id.to_string()).execute(&database.pool).await.unwrap();
    }
    let mut after = None;
    for index in [31, 63] {
        after = database
            .recover_unknown_terminal_projections(after.as_deref())
            .await
            .unwrap();
        assert_eq!(after, Some(format!("00000000-0000-0000-0000-{index:012}")));
        assert!(
            database
                .claim_terminal_projection_tasks(Uuid::now_v7(), 32)
                .await
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        database
            .recover_unknown_terminal_projections(after.as_deref())
            .await
            .unwrap(),
        None
    );
    let owner = Uuid::now_v7();
    assert_eq!(
        database
            .claim_terminal_projection_tasks(owner, 32)
            .await
            .unwrap(),
        vec![first.request_id]
    );
    assert!(
        database
            .project_claimed_terminal_projection_task(owner, first.request_id)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE pending_proxy_settlements SET unknown_reason = 'authoritative_usage_unavailable' WHERE request_id = $1")
        .bind(late.request_id.to_string()).execute(&database.pool).await.unwrap();
    sqlx::query(
        "UPDATE proxy_operational_terminals SET fence = 'stale-fence' WHERE request_id = $1",
    )
    .bind(late.request_id.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    for pass in 0..2 {
        let mut after = None;
        for page in 0..3 {
            after = database
                .recover_unknown_terminal_projections(after.as_deref())
                .await
                .unwrap();
            assert_eq!(after.is_none(), page == 2);
        }
        let tasks = database
            .claim_terminal_projection_tasks(owner, 32)
            .await
            .unwrap();
        if pass == 0 {
            assert!(tasks.is_empty());
            sqlx::query("UPDATE proxy_operational_terminals SET fence = $1 WHERE request_id = $2")
                .bind(late.fence.to_string())
                .bind(late.request_id.to_string())
                .execute(&database.pool)
                .await
                .unwrap();
        } else {
            assert_eq!(tasks, vec![late.request_id]);
            assert!(
                database
                    .project_claimed_terminal_projection_task(owner, late.request_id)
                    .await
                    .unwrap()
            );
        }
    }
    for claim in [first, late] {
        let row = sqlx::query("SELECT p.state, r.completed_at, (SELECT COUNT(*) FROM ledger_entries WHERE source = p.reservation_id AND kind = 'usage') AS ledger, (SELECT COUNT(*) FROM request_stats_facts WHERE request_id = p.request_id) AS facts FROM pending_proxy_settlements p JOIN request_records r ON r.id = p.request_id WHERE p.request_id = $1")
            .bind(claim.request_id.to_string()).fetch_one(&database.pool).await.unwrap();
        assert_eq!(row.get::<String, _>("state"), "pending");
        assert_eq!(row.get::<Option<i64>, _>("completed_at"), None);
        assert_eq!(row.get::<i64, _>("ledger"), 0);
        assert_eq!(row.get::<i64, _>("facts"), 1);
        let stats = database
            .stats_filtered(claim.key_id, stats_window())
            .await
            .unwrap();
        assert_eq!(stats.summary.failed_requests, 1);
        assert_eq!(stats.summary.total_cost, None);
    }
}

#[tokio::test]
async fn sqlite_terminal_recovery_pages_and_revisits_without_settling_unknowns() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("recovery.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    recovery_pages_contract(&database).await;
}

#[tokio::test]
async fn postgres_terminal_recovery_does_not_block_ready_projection() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "PostgreSQL required in CI"
        );
        return;
    };
    let isolated = PostgresTestDatabase::create(&url).await;
    let database = Database::connect_with_max(isolated.url(), 8).await.unwrap();
    database.migrate().await.unwrap();
    let unknown = historical_unknown(&database).await;
    let mut ready = Vec::new();
    for status in [200, 503] {
        let (key, reservation, request_id) = admitted(&database, true).await;
        let mut input = finish(&key, &reservation, request_id);
        input.status_code = status;
        if status == 503 {
            input.error_code = Some("upstream_unavailable");
        }
        database.finish_proxy_request_deferred(input).await.unwrap();
        ready.push((key, reservation, request_id));
    }
    let recovery = Database::connect_with_max(isolated.url(), 1).await.unwrap();
    let recovery_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&recovery.pool)
        .await
        .unwrap();
    let mut gate = database.begin_write_transaction().await.unwrap();
    sqlx::query("UPDATE request_records SET completed_at = completed_at WHERE id = $1")
        .bind(unknown.request_id.to_string())
        .execute(&mut *gate)
        .await
        .unwrap();
    let recovering = tokio::spawn(async move {
        let result = recovery.recover_unknown_terminal_projections(None).await;
        recovery.close().await;
        result
    });
    wait_for_projection_lock(&database, recovery_pid).await;
    let owner = Uuid::now_v7();
    let tasks = tokio::time::timeout(
        Duration::from_secs(5),
        database.claim_terminal_projection_tasks(owner, 32),
    )
    .await
    .expect("ready outbox claim must not wait for historical recovery")
    .unwrap();
    assert_eq!(tasks.len(), 2);
    for (key, reservation, request_id) in &ready {
        assert!(tasks.contains(request_id));
        assert!(
            tokio::time::timeout(
                Duration::from_secs(5),
                database.project_claimed_terminal_projection_task(owner, *request_id),
            )
            .await
            .expect("ready projection must not wait for another request's recovery")
            .unwrap()
        );
        assert!(
            !database
                .project_claimed_terminal_projection_task(owner, *request_id)
                .await
                .unwrap()
        );
        assert_counts(&database, *request_id, reservation.id, 1).await;
        let lifetime: i64 = sqlx::query_scalar(
            "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1",
        )
        .bind(key.account_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(lifetime, 10);
        let filter = stats_window();
        let stats = database
            .stats_filtered(key.key_id, filter.clone())
            .await
            .unwrap();
        let usage = database
            .projected_usage_analysis(
                None,
                UsageAnalysisFilter {
                    key_id: Some(key.key_id),
                    from_created_at: filter.from_created_at,
                    to_created_at: filter.to_created_at,
                    ..Default::default()
                },
                UsageAnalysisProjection::Overview,
            )
            .await
            .unwrap();
        assert_eq!(usage.summary.requests, stats.summary.total_requests);
        assert_eq!(usage.summary.success, stats.summary.successful_requests);
        assert_eq!(usage.summary.failed, stats.summary.failed_requests);
    }
    assert!(!recovering.is_finished());
    gate.rollback().await.unwrap();
    assert_eq!(recovering.await.unwrap().unwrap(), None);
    let recovered = database
        .claim_terminal_projection_tasks(owner, 32)
        .await
        .unwrap();
    assert_eq!(recovered, vec![unknown.request_id]);
    assert!(
        database
            .project_claimed_terminal_projection_task(owner, unknown.request_id)
            .await
            .unwrap()
    );
    recovery_pages_contract(&database).await;
    database.close().await;
    isolated.cleanup().await;
}
