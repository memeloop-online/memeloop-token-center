use super::super::super::*;
use super::{fixture, input};

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
        sqlx::query_scalar::<_, i64>("SELECT funded_micros FROM entitlement_cycles WHERE id = $1")
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
