use super::super::*;

struct AdjustmentFixture {
    _directory: tempfile::TempDir,
    database: Database,
    account_id: Uuid,
    settlement_id: Uuid,
    request_id: Uuid,
}

async fn fixture() -> AdjustmentFixture {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("settlement-adjustments.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "adjustments".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "adjustments".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            b"settlement adjustment fixture pepper over thirty-two bytes",
        )
        .await
        .unwrap();
    let settlement_id = Uuid::now_v7();
    let request_id = Uuid::now_v7();
    let now = unix_millis();
    database
        .grant(
            issued.account_id,
            Decimal::new(1, 4),
            "adjustment-fixture",
            "adjustment-fixture:funding",
        )
        .await
        .unwrap();
    // Keep the fixture's ordinary funding ledger and account aggregate in
    // sync with the synthetic gross settlement below.
    sqlx::query(
        "UPDATE credit_accounts SET available_micros = available_micros - 100 WHERE id = $1 AND available_micros >= 100",
    )
    .bind(issued.account_id.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) VALUES ($1, $2, $3, 'usage', -100, 'USD', 'adjustment-fixture', $4)",
    )
    .bind(settlement_id.to_string())
    .bind(issued.account_id.to_string())
    .bind(issued.key_id.to_string())
    .bind(now)
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO account_settlement_feed (settlement_id, account_id, settlement_sequence, request_id, request_kind, key_id, model, cost_micros, currency, settled_at, completed_at) VALUES ($1, $2, 1, $3, 'text', $4, 'fixture', 100, 'USD', $5, $5)",
    )
    .bind(settlement_id.to_string())
    .bind(issued.account_id.to_string())
    .bind(request_id.to_string())
    .bind(issued.key_id.to_string())
    .bind(now)
    .execute(&database.pool)
    .await
    .unwrap();
    AdjustmentFixture {
        _directory: directory,
        database,
        account_id: issued.account_id,
        settlement_id,
        request_id,
    }
}

fn input(
    fixture: &AdjustmentFixture,
    namespace: &str,
    desired: i64,
    version: i64,
    key: &str,
) -> ReconcileSettlementAdjustmentInput {
    ReconcileSettlementAdjustmentInput {
        account_id: fixture.account_id,
        settlement_id: fixture.settlement_id,
        namespace: namespace.to_owned(),
        request_kind: AccountSettlementKind::Text,
        request_id: fixture.request_id,
        currency: "USD".to_owned(),
        desired_rebate_micros: desired,
        version,
        decision_digest: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .to_owned(),
        source: "memeloop-cloud:usage-discount".to_owned(),
        idempotency_key: key.to_owned(),
    }
}

#[tokio::test]
async fn settlement_adjustment_replays_rejects_conflicts_and_caps_namespaces() {
    let fixture = fixture().await;
    let zero = fixture
        .database
        .reconcile_settlement_adjustment(input(
            &fixture,
            "memeloop-cloud:usage-discount",
            0,
            1,
            "adjustment:zero",
        ))
        .await
        .unwrap();
    assert_eq!(zero.applied_delta_micros, 0);
    assert!(zero.adjustment_entry_id.is_none());
    assert!(
        fixture
            .database
            .reconcile_settlement_adjustment(input(
                &fixture,
                "memeloop-cloud:usage-discount",
                0,
                1,
                "adjustment:zero",
            ))
            .await
            .unwrap()
            .replayed
    );
    assert!(
        fixture
            .database
            .reconcile_settlement_adjustment(input(
                &fixture,
                "memeloop-cloud:usage-discount",
                0,
                1,
                "adjustment:zero-alias",
            ))
            .await
            .unwrap()
            .replayed
    );
    assert!(matches!(
        fixture
            .database
            .reconcile_settlement_adjustment(input(
                &fixture,
                "memeloop-cloud:usage-discount",
                1,
                1,
                "adjustment:zero",
            ))
            .await,
        Err(AppError::Conflict(_))
    ));
    let first = fixture
        .database
        .reconcile_settlement_adjustment(input(
            &fixture,
            "memeloop-cloud:usage-discount",
            20,
            2,
            "adjustment:one",
        ))
        .await
        .unwrap();
    assert_eq!(first.applied_delta_micros, 20);
    assert_eq!(first.cumulative_rebate_micros, 20);
    assert!(!first.replayed);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM entitlement_usage_allocations WHERE usage_ledger_entry_id = $1",
        )
        .bind(fixture.settlement_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap(),
        0,
        "a fully generic settlement never manufactures allocation changes",
    );
    let replay = fixture
        .database
        .reconcile_settlement_adjustment(input(
            &fixture,
            "memeloop-cloud:usage-discount",
            20,
            2,
            "adjustment:one",
        ))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.event_id, first.event_id);
    let mut conflict = input(
        &fixture,
        "memeloop-cloud:usage-discount",
        21,
        2,
        "adjustment:conflict",
    );
    assert!(matches!(
        fixture
            .database
            .reconcile_settlement_adjustment(conflict.clone())
            .await,
        Err(AppError::Conflict(_))
    ));
    conflict.version = 1;
    assert!(matches!(
        fixture
            .database
            .reconcile_settlement_adjustment(conflict)
            .await,
        Err(AppError::Conflict(_))
    ));
    assert!(matches!(
        fixture
            .database
            .reconcile_settlement_adjustment(input(
                &fixture,
                "memeloop-cloud:usage-discount",
                10,
                3,
                "adjustment:decrease",
            ))
            .await,
        Err(AppError::Conflict(_))
    ));
    assert!(matches!(
        fixture
            .database
            .reconcile_settlement_adjustment(input(
                &fixture,
                "another:discount",
                81,
                1,
                "adjustment:over-gross",
            ))
            .await,
        Err(AppError::Conflict(_))
    ));
    let mut wrong_request = input(
        &fixture,
        "memeloop-cloud:usage-discount",
        20,
        3,
        "adjustment:wrong-request",
    );
    wrong_request.request_id = Uuid::now_v7();
    assert!(matches!(
        fixture
            .database
            .reconcile_settlement_adjustment(wrong_request)
            .await,
        Err(AppError::Conflict(_))
    ));
    let mut wrong_settlement = input(
        &fixture,
        "memeloop-cloud:usage-discount",
        20,
        3,
        "adjustment:wrong-settlement",
    );
    wrong_settlement.settlement_id = Uuid::now_v7();
    assert!(matches!(
        fixture
            .database
            .reconcile_settlement_adjustment(wrong_settlement)
            .await,
        Err(AppError::NotFound)
    ));
}

#[tokio::test]
async fn settlement_adjustment_mixed_funding_waits_for_the_generic_threshold() {
    let fixture = fixture().await;
    let now = unix_millis();
    let entitlement_id = Uuid::now_v7();
    let cycle_id = Uuid::now_v7();
    let tenant_id: String =
        sqlx::query_scalar("SELECT tenant_id FROM credit_accounts WHERE id = $1")
            .bind(fixture.account_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO subscription_entitlements (id, tenant_id, account_id, provider, external_subscription_id, status, version, current_cycle_id, created_at, updated_at) VALUES ($1, $2, $3, 'test', 'mixed-adjustment', 'active', 1, $4, $5, $5)",
    )
    .bind(entitlement_id.to_string())
    .bind(&tenant_id)
    .bind(fixture.account_id.to_string())
    .bind(cycle_id.to_string())
    .bind(now)
    .execute(&fixture.database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO entitlement_cycles (id, entitlement_id, external_cycle_id, period_start, period_end, currency, desired_micros, funded_micros, consumed_micros, status, created_at, updated_at) VALUES ($1, $2, $3, 0, 9999999999999, 'USD', 40, 60, 60, 'active', $4, $4)",
    )
    .bind(cycle_id.to_string())
    .bind(entitlement_id.to_string())
    .bind(cycle_id.to_string())
    .bind(now)
    .execute(&fixture.database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO entitlement_usage_allocations (id, entitlement_cycle_id, usage_ledger_entry_id, amount_micros, created_at) VALUES ($1, $2, $3, 60, $4)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(cycle_id.to_string())
    .bind(fixture.settlement_id.to_string())
    .bind(now)
    .execute(&fixture.database.pool)
    .await
    .unwrap();
    let below_threshold = fixture
        .database
        .reconcile_settlement_adjustment(input(
            &fixture,
            "memeloop-cloud:usage-discount",
            20,
            1,
            "adjustment:mixed-one",
        ))
        .await
        .unwrap();
    assert_eq!(below_threshold.applied_delta_micros, 20);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT consumed_micros FROM entitlement_cycles WHERE id = $1",
        )
        .bind(cycle_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap(),
        60,
        "the first 40 micros are generic funding",
    );
    let after_threshold = fixture
        .database
        .reconcile_settlement_adjustment(input(
            &fixture,
            "memeloop-cloud:usage-discount",
            50,
            2,
            "adjustment:mixed-two",
        ))
        .await
        .unwrap();
    assert_eq!(after_threshold.applied_delta_micros, 30);
    assert_eq!(after_threshold.cumulative_rebate_micros, 50);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT consumed_micros FROM entitlement_cycles WHERE id = $1",
        )
        .bind(cycle_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap(),
        50,
        "only the 10 micros beyond generic funding are returned to entitlement",
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT funded_micros FROM entitlement_cycles WHERE id = $1",)
            .bind(cycle_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap(),
        50,
        "an active cycle whose desired amount is below revised consumption loses excess funding",
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT amount_micros FROM settlement_adjustment_entitlement_funding_reductions WHERE event_id = $1 AND entitlement_cycle_id = $2",
        )
        .bind(after_threshold.event_id.to_string())
        .bind(cycle_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap(),
        10,
    );
}

#[tokio::test]
async fn postgres_settlement_adjustments_serialize_versions_and_namespace_caps() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!("MTC_TEST_POSTGRES_URL is unset; skipping PostgreSQL settlement adjustment test");
        return;
    };
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let label = format!("settlement-adjustment-postgres-{}", Uuid::now_v7());
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: label.clone(),
                principal_external_id: "member".to_owned(),
                alias: label.clone(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            b"PostgreSQL settlement adjustment fixture pepper over thirty-two bytes",
        )
        .await
        .unwrap();
    database
        .grant(
            issued.account_id,
            Decimal::new(1, 4),
            "adjustment-postgres-fixture",
            &format!("{label}:funding"),
        )
        .await
        .unwrap();
    let settlement_id = Uuid::now_v7();
    let request_id = Uuid::now_v7();
    let now = unix_millis();
    sqlx::query(
        "UPDATE credit_accounts SET available_micros = available_micros - 100 WHERE id = $1 AND available_micros >= 100",
    )
    .bind(issued.account_id.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) VALUES ($1, $2, $3, 'usage', -100, 'USD', $4, $5)",
    )
    .bind(settlement_id.to_string())
    .bind(issued.account_id.to_string())
    .bind(issued.key_id.to_string())
    .bind(&label)
    .bind(now)
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO account_settlement_feed (settlement_id, account_id, settlement_sequence, request_id, request_kind, key_id, model, cost_micros, currency, settled_at, completed_at) VALUES ($1, $2, 1, $3, 'text', $4, 'fixture', 100, 'USD', $5, $5)",
    )
    .bind(settlement_id.to_string())
    .bind(issued.account_id.to_string())
    .bind(request_id.to_string())
    .bind(issued.key_id.to_string())
    .bind(now)
    .execute(&database.pool)
    .await
    .unwrap();
    let fixture = AdjustmentFixture {
        _directory: tempfile::tempdir().unwrap(),
        database,
        account_id: issued.account_id,
        settlement_id,
        request_id,
    };
    let same_v1 = input(
        &fixture,
        "memeloop-cloud:usage-discount",
        10,
        1,
        &format!("{label}:same-v1"),
    );
    let same_v2 = input(
        &fixture,
        "memeloop-cloud:usage-discount",
        20,
        2,
        &format!("{label}:same-v2"),
    );
    let (first, second) = tokio::join!(
        fixture.database.reconcile_settlement_adjustment(same_v1),
        fixture.database.reconcile_settlement_adjustment(same_v2),
    );
    assert!(first.is_ok() || second.is_ok());
    let state: (i64, i64) = sqlx::query_as(
        "SELECT desired_rebate_micros, version FROM settlement_adjustment_states WHERE account_id = $1 AND settlement_id = $2 AND namespace = 'memeloop-cloud:usage-discount'",
    )
    .bind(fixture.account_id.to_string())
    .bind(fixture.settlement_id.to_string())
    .fetch_one(&fixture.database.pool)
    .await
    .unwrap();
    assert!(state.0 <= 20 && state.1 <= 2);
    let namespace_one = input(
        &fixture,
        "first:discount",
        60,
        1,
        &format!("{label}:namespace-one"),
    );
    let namespace_two = input(
        &fixture,
        "second:discount",
        60,
        1,
        &format!("{label}:namespace-two"),
    );
    let (first, second) = tokio::join!(
        fixture
            .database
            .reconcile_settlement_adjustment(namespace_one),
        fixture
            .database
            .reconcile_settlement_adjustment(namespace_two),
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
}

#[tokio::test]
async fn settlement_adjustment_restores_only_entitlement_tail() {
    let fixture = fixture().await;
    let now = unix_millis();
    let entitlement_id = Uuid::now_v7();
    let first_cycle = Uuid::now_v7();
    let second_cycle = Uuid::now_v7();
    let tenant_id: String =
        sqlx::query_scalar("SELECT tenant_id FROM credit_accounts WHERE id = $1")
            .bind(fixture.account_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO subscription_entitlements (id, tenant_id, account_id, provider, external_subscription_id, status, version, current_cycle_id, created_at, updated_at) VALUES ($1, $2, $3, 'test', 'adjustment', 'cancelled', 1, $4, $5, $5)",
    )
    .bind(entitlement_id.to_string())
    .bind(&tenant_id)
    .bind(fixture.account_id.to_string())
    .bind(second_cycle.to_string())
    .bind(now)
    .execute(&fixture.database.pool)
    .await
    .unwrap();
    for (cycle_id, period_end) in [(first_cycle, 10_i64), (second_cycle, 20_i64)] {
        sqlx::query(
            "INSERT INTO entitlement_cycles (id, entitlement_id, external_cycle_id, period_start, period_end, currency, desired_micros, funded_micros, consumed_micros, status, created_at, updated_at) VALUES ($1, $2, $3, 0, $4, 'USD', 50, 50, 50, 'cancelled', $5, $5)",
        )
        .bind(cycle_id.to_string())
        .bind(entitlement_id.to_string())
        .bind(cycle_id.to_string())
        .bind(period_end)
        .bind(now)
        .execute(&fixture.database.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO entitlement_usage_allocations (id, entitlement_cycle_id, usage_ledger_entry_id, amount_micros, created_at) VALUES ($1, $2, $3, 50, $4)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(cycle_id.to_string())
        .bind(fixture.settlement_id.to_string())
        .bind(now)
        .execute(&fixture.database.pool)
        .await
        .unwrap();
    }
    let result = fixture
        .database
        .reconcile_settlement_adjustment(input(
            &fixture,
            "memeloop-cloud:usage-discount",
            60,
            1,
            "adjustment:tail",
        ))
        .await
        .unwrap();
    assert_eq!(result.applied_delta_micros, 60);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT consumed_micros FROM entitlement_cycles WHERE id = $1",
        )
        .bind(first_cycle.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap(),
        40,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT consumed_micros FROM entitlement_cycles WHERE id = $1",
        )
        .bind(second_cycle.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap(),
        0,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT available_micros FROM credit_accounts WHERE id = $1",)
            .bind(fixture.account_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap(),
        0,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM entitlement_usage_allocations WHERE usage_ledger_entry_id = $1 AND entitlement_cycle_id = $2",
        )
        .bind(fixture.settlement_id.to_string())
        .bind(second_cycle.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap(),
        0,
    );
    let rollbacks = sqlx::query(
        "SELECT rollback_sequence, entitlement_cycle_id, amount_micros FROM settlement_adjustment_entitlement_rollbacks WHERE event_id = $1 ORDER BY rollback_sequence ASC",
    )
    .bind(result.event_id.to_string())
    .fetch_all(&fixture.database.pool)
    .await
    .unwrap();
    assert_eq!(rollbacks.len(), 2);
    assert_eq!(rollbacks[0].get::<i64, _>("rollback_sequence"), 1);
    assert_eq!(
        rollbacks[0].get::<String, _>("entitlement_cycle_id"),
        second_cycle.to_string(),
    );
    assert_eq!(rollbacks[0].get::<i64, _>("amount_micros"), 50);
    assert_eq!(rollbacks[1].get::<i64, _>("rollback_sequence"), 2);
    assert_eq!(
        rollbacks[1].get::<String, _>("entitlement_cycle_id"),
        first_cycle.to_string(),
    );
    assert_eq!(rollbacks[1].get::<i64, _>("amount_micros"), 10);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(SUM(amount_micros), 0) FROM settlement_adjustment_entitlement_funding_reductions WHERE event_id = $1",
        )
        .bind(result.event_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap(),
        60,
        "late cancellation rebates lower cancelled funding instead of creating generic credit",
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM entitlement_cycles WHERE id = $1",)
            .bind(second_cycle.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap(),
        "cancelled",
    );
}
