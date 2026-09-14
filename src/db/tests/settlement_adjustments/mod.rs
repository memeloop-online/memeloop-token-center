mod lifecycle;
mod postgres;
mod sqlite;

use super::super::*;
use rust_decimal::Decimal;
use tempfile::TempDir;
use uuid::Uuid;

pub(super) struct AdjustmentFixture {
    pub(super) _directory: TempDir,
    pub(super) database: Database,
    pub(super) account_id: Uuid,
    pub(super) settlement_id: Uuid,
    pub(super) request_id: Uuid,
}

pub(super) async fn fixture() -> AdjustmentFixture {
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

pub(super) fn input(
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
