use memeloop_token_center::{
    db::{CreateKeyInput, Database, EntitlementOperation, ReconcileEntitlementInput},
    error::AppError,
    model::KeyPolicy,
};
use rust_decimal::Decimal;
use uuid::Uuid;

#[tokio::test]
async fn postgres_entitlement_versions_are_serialized_and_idempotent() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect_with_max(&database_url, 12).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let tenant = format!("postgres-entitlement-{unique}");
    let pepper = b"postgres entitlement pepper longer than thirty-two bytes";
    let key = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.clone(),
                principal_external_id: "member".into(),
                alias: "stable-account".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let input = |version: i64, desired_micros: i64| ReconcileEntitlementInput {
        tenant_external_id: tenant.clone(),
        account_id: key.account_id,
        provider: "postgres-test".into(),
        external_subscription_id: format!("sub-{unique}"),
        external_cycle_id: format!("cycle-{unique}"),
        period_start: 1_700_000_000_000,
        period_end: 4_100_000_000_000,
        currency: "USD".into(),
        desired_micros,
        version,
        source: "postgres-test".into(),
        proration_json: None,
    };
    let create_key = format!("postgres:{unique}:create");
    let first = database
        .reconcile_entitlement(
            EntitlementOperation::Reconcile(input(1, 1_000_000)),
            &create_key,
        )
        .await
        .unwrap();
    let replay = database
        .reconcile_entitlement(
            EntitlementOperation::Reconcile(input(1, 1_000_000)),
            &create_key,
        )
        .await
        .unwrap();
    assert_eq!(replay, first);

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for index in 0..8 {
        let database = database.clone();
        let barrier = barrier.clone();
        let operation = EntitlementOperation::Reconcile(input(2, 2_000_000));
        let idempotency_key = format!("postgres:{unique}:version-2:{index}");
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database
                .reconcile_entitlement(operation, &idempotency_key)
                .await
        }));
    }
    let mut succeeded = 0;
    let mut conflicted = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(result) => {
                succeeded += 1;
                assert_eq!(result.ledger_delta, "1");
            }
            Err(AppError::Conflict(_)) => conflicted += 1,
            result => panic!("unexpected PostgreSQL reconciliation result: {result:?}"),
        }
    }
    assert_eq!(succeeded, 1);
    assert_eq!(conflicted, 7);
}
