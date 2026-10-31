use super::*;

fn stats_window() -> StatsFilter {
    let now = unix_millis();
    StatsFilter {
        from_created_at: Some(now - 60 * 60 * 1_000),
        to_created_at: Some(now + 60 * 60 * 1_000),
        ..StatsFilter::default()
    }
}

pub(super) async fn drain_terminal(fixture: &Fixture) {
    let owner = Uuid::now_v7();
    let tasks = fixture
        .database
        .claim_terminal_projection_tasks(owner, 32)
        .await
        .unwrap();
    assert!(tasks.contains(&fixture.claim.request_id));
    for request_id in tasks {
        assert!(
            fixture
                .database
                .project_claimed_terminal_projection_task(owner, request_id)
                .await
                .unwrap()
        );
        assert!(
            !fixture
                .database
                .project_claimed_terminal_projection_task(owner, request_id)
                .await
                .unwrap()
        );
    }
}

#[tokio::test]
async fn sqlite_unknown_bridge_preserves_failures_and_replaces_financial_facts_once() {
    for metered in [false, true] {
        for unknown_first in [false, true] {
            for project_first in [false, true] {
                correction_contract(
                    Fixture::sqlite().await,
                    metered,
                    unknown_first,
                    project_first,
                )
                .await;
            }
        }
    }
}

#[tokio::test]
async fn postgres_unknown_bridge_preserves_failures_and_replaces_financial_facts_once() {
    for metered in [false, true] {
        for unknown_first in [false, true] {
            for project_first in [false, true] {
                let Some(fixture) = Fixture::postgres().await else {
                    return;
                };
                correction_contract(fixture, metered, unknown_first, project_first).await;
            }
        }
    }
}

async fn correction_contract(
    mut fixture: Fixture,
    metered: bool,
    unknown_first: bool,
    project_first: bool,
) {
    if metered {
        let mut replacement = Fixture::create_with_mode(
            fixture.database.clone(),
            fixture.isolated.take(),
            "operational-test",
            EnforcementMode::MeteredUnlimited,
        )
        .await;
        replacement.directory = fixture.directory.take();
        fixture = replacement;
    }
    let request_id = fixture.claim.request_id;
    let receipt_id = Uuid::now_v7();
    let status = match (metered, unknown_first, project_first) {
        (false, false, false) => 413,
        (false, false, true) => 429,
        (false, true, false) => 499,
        (false, true, true) => 503,
        (true, false, false) => 504,
        (true, false, true) => 200,
        _ => 502,
    };
    let terminal = fixture.operational(status);
    if unknown_first {
        fixture
            .database
            .mark_pending_proxy_settlement_unknown(&fixture.claim)
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM terminal_projection_outbox WHERE request_id = $1",
        )
        .bind(request_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
        assert_eq!(count, 0);
    }
    fixture.record(receipt_id, &terminal).await.unwrap();
    fixture
        .database
        .mark_pending_proxy_settlement_unknown(&fixture.claim)
        .await
        .unwrap();
    fixture.record(receipt_id, &terminal).await.unwrap();
    let mut forged = fixture.claim.clone();
    forged.fence = Uuid::now_v7();
    assert!(
        fixture
            .database
            .mark_pending_proxy_settlement_unknown(&forged)
            .await
            .is_err()
    );
    let stale_owner = Uuid::now_v7();
    if project_first {
        drain_terminal(&fixture).await;
        let stats = fixture
            .database
            .stats_filtered(fixture.key.key_id, stats_window())
            .await
            .unwrap();
        assert_eq!(stats.summary.total_requests, 1);
        assert_eq!(stats.summary.failed_requests, 1);
        assert_eq!(stats.summary.total_cost, None);
        assert!(stats.summary.costs.is_empty());
        let cost_filtered = fixture
            .database
            .stats_filtered(
                fixture.key.key_id,
                StatsFilter {
                    max_cost_micros: Some(0),
                    ..stats_window()
                },
            )
            .await
            .unwrap();
        assert_eq!(cost_filtered.summary.total_requests, 0);
        let unknown = sqlx::query("SELECT q.completed_at, q.usage_basis, p.state, r.status, r.actual_micros, (SELECT COUNT(*) FROM metered_usage_projection_outbox m WHERE m.reservation_id = r.id) AS metered, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = r.id) AS ledgers FROM request_records q JOIN pending_proxy_settlements p ON p.request_id = q.id JOIN usage_reservations r ON r.id = p.reservation_id WHERE q.id = $1")
            .bind(request_id.to_string()).fetch_one(&fixture.database.pool).await.unwrap();
        assert_eq!(unknown.get::<Option<i64>, _>("completed_at"), None);
        assert_eq!(unknown.get::<Option<String>, _>("usage_basis"), None);
        assert_eq!(unknown.get::<String, _>("state"), "pending");
        assert_eq!(unknown.get::<String, _>("status"), "reserved");
        assert_eq!(unknown.get::<Option<i64>, _>("actual_micros"), None);
        assert_eq!(unknown.get::<i64, _>("metered"), 0);
        assert_eq!(unknown.get::<i64, _>("ledgers"), 0);
    } else {
        assert!(
            fixture
                .database
                .claim_terminal_projection_tasks(stale_owner, 32)
                .await
                .unwrap()
                .contains(&request_id)
        );
    }
    fixture.finish(receipt_id).await.unwrap();
    fixture.finish(receipt_id).await.unwrap();
    let awaiting_projection = fixture
        .database
        .stats_filtered(fixture.key.key_id, stats_window())
        .await
        .unwrap();
    assert_eq!(awaiting_projection.summary.total_requests, 1);
    assert_eq!(awaiting_projection.summary.failed_requests, 1);
    assert_eq!(
        awaiting_projection.summary.total_cost.as_deref(),
        Some("0.000012")
    );
    for (max_cost, expected_requests) in [(0, 0), (12, 1)] {
        let filtered = fixture
            .database
            .stats_filtered(
                fixture.key.key_id,
                StatsFilter {
                    max_cost_micros: Some(max_cost),
                    ..stats_window()
                },
            )
            .await
            .unwrap();
        assert_eq!(filtered.summary.total_requests, expected_requests);
    }
    assert!(
        !fixture
            .database
            .project_claimed_terminal_projection_task(stale_owner, request_id)
            .await
            .unwrap()
    );
    drain_terminal(&fixture).await;
    fixture.record(receipt_id, &terminal).await.unwrap();
    assert!(
        fixture
            .database
            .claim_terminal_projection_tasks(Uuid::now_v7(), 32)
            .await
            .unwrap()
            .is_empty()
    );
    let fact = sqlx::query("SELECT status_class, error_code, cost_micros, input_tokens, output_tokens FROM request_stats_facts WHERE request_id = $1")
        .bind(request_id.to_string()).fetch_one(&fixture.database.pool).await.unwrap();
    assert_eq!(fact.get::<String, _>("status_class"), "failure");
    assert_eq!(
        fact.get::<String, _>("error_code"),
        terminal.error_code.unwrap()
    );
    assert_eq!(fact.get::<i64, _>("cost_micros"), 12);
    assert_eq!(fact.get::<i64, _>("input_tokens"), 5);
    assert_eq!(fact.get::<i64, _>("output_tokens"), 7);
    for table in [
        "usage_daily_aggregates",
        "request_daily_aggregates",
        "usage_analysis_hourly",
        "usage_analysis_daily",
        "session_usage_totals",
        "session_usage_hourly",
        "session_usage_daily",
    ] {
        let statement = format!(
            "SELECT CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(cost_micros) AS BIGINT) AS cost FROM {table} WHERE key_id = $1"
        );
        let totals = sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(fixture.key.key_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
        assert_eq!(totals.get::<i64, _>("requests"), 1, "{table}");
        assert_eq!(totals.get::<i64, _>("cost"), 12, "{table}");
    }
    let lifetime: i64 = sqlx::query_scalar(
        "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1",
    )
    .bind(fixture.key.account_id.to_string())
    .fetch_one(&fixture.database.pool)
    .await
    .unwrap();
    assert_eq!(lifetime, 12);
    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'finished'",
    )
    .bind(request_id.to_string())
    .fetch_one(&fixture.database.pool)
    .await
    .unwrap();
    assert_eq!(events, 1);
    let stats = fixture
        .database
        .stats_filtered(fixture.key.key_id, stats_window())
        .await
        .unwrap();
    assert_eq!(stats.summary.total_requests, 1);
    assert_eq!(stats.summary.successful_requests, 0);
    assert_eq!(stats.summary.failed_requests, 1);
    assert_eq!(stats.summary.pending_requests, 0);
    assert_eq!(stats.summary.total_cost.as_deref(), Some("0.000012"));
    fixture.close().await;
}

#[tokio::test]
async fn sqlite_unknown_bridge_recovers_existing_annotations() {
    recovery_contract(Fixture::sqlite().await).await;
}

#[tokio::test]
async fn postgres_unknown_bridge_recovers_existing_annotations() {
    let Some(fixture) = Fixture::postgres().await else {
        return;
    };
    recovery_contract(fixture).await;
}

async fn recovery_contract(fixture: Fixture) {
    fixture
        .record(Uuid::now_v7(), &fixture.operational(499))
        .await
        .unwrap();
    sqlx::query("UPDATE pending_proxy_settlements SET unknown_reason = 'authoritative_usage_unavailable' WHERE request_id = $1")
        .bind(fixture.claim.request_id.to_string()).execute(&fixture.database.pool).await.unwrap();
    drain_terminal(&fixture).await;
    let stats = fixture
        .database
        .stats_filtered(fixture.key.key_id, stats_window())
        .await
        .unwrap();
    assert_eq!(stats.summary.total_requests, 1);
    assert_eq!(stats.summary.failed_requests, 1);
    assert_eq!(stats.summary.total_cost, None);
    fixture.close().await;
}

#[tokio::test]
async fn sqlite_unknown_bridge_keeps_tenants_and_currencies_separate() {
    isolation_contract(Fixture::sqlite().await).await;
}

#[tokio::test]
async fn postgres_unknown_bridge_keeps_tenants_and_currencies_separate() {
    let Some(fixture) = Fixture::postgres().await else {
        return;
    };
    isolation_contract(fixture).await;
}

async fn isolation_contract(fixture: Fixture) {
    let foreign = Fixture::create_with_currency(
        fixture.database.clone(),
        None,
        "foreign-tenant",
        EnforcementMode::MeteredUnlimited,
        "CNY",
    )
    .await;
    assert_eq!(fixture.key.currency, "USD");
    assert_eq!(foreign.key.currency, "CNY");
    assert_ne!(fixture.key.tenant_id, foreign.key.tenant_id);
    for current in [&fixture, &foreign] {
        let terminal = current.operational(499);
        let (recorded, marked) = tokio::join!(
            current.record(Uuid::now_v7(), &terminal),
            current
                .database
                .mark_pending_proxy_settlement_unknown(&current.claim),
        );
        recorded.unwrap();
        marked.unwrap();
    }
    drain_terminal(&fixture).await;
    fixture.finish(Uuid::now_v7()).await.unwrap();
    drain_terminal(&fixture).await;
    let global = fixture
        .database
        .global_operator_stats_filtered(stats_window())
        .await
        .unwrap();
    assert_eq!(global.summary.total_requests, 2);
    assert_eq!(global.summary.failed_requests, 2);
    assert_eq!(global.summary.total_cost, None);
    assert_eq!(global.summary.costs.len(), 1);
    assert_eq!(global.summary.costs[0].currency, "USD");
    let own = fixture
        .database
        .operator_stats_filtered("operational-test", stats_window())
        .await
        .unwrap();
    assert_eq!(own.summary.total_requests, 1);
    assert_eq!(own.summary.total_cost.as_deref(), Some("0.000012"));
    let unknown = fixture
        .database
        .stats_filtered(foreign.key.key_id, stats_window())
        .await
        .unwrap();
    assert_eq!(unknown.summary.total_requests, 1);
    assert_eq!(unknown.summary.failed_requests, 1);
    assert_eq!(unknown.summary.total_cost, None);
    assert!(unknown.summary.costs.is_empty());
    let actual: Option<i64> =
        sqlx::query_scalar("SELECT actual_micros FROM usage_reservations WHERE id = $1")
            .bind(foreign.reservation.id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    assert_eq!(actual, None);
    fixture.close().await;
}
