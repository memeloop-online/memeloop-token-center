use super::*;
use crate::db::{TerminalRecoveryIdentity, test_support::PostgresTestDatabase};

async fn exact_contract(db: &Database) {
    let (key, reservation, id) = admitted(db, true).await;
    db.finish_proxy_request_deferred(finish(&key, &reservation, id))
        .await
        .unwrap();
    sqlx::query("INSERT INTO proxy_operational_terminals (request_id,request_created_at,tenant_id,key_id,account_id,reservation_id,owner_token,fence,contract_digest,receipt_id,operational_digest,status_code,error_code,completed_at,duration_ms,response_object,recorded_at) SELECT request_id,created_at,tenant_id,key_id,account_id,reservation_id,'fixture-owner','fixture-fence','fixture-contract','fixture-receipt','fixture-operational',status_code,error_code,completed_at,duration_ms,response_object,completed_at FROM terminal_projection_outbox WHERE request_id=$1")
        .bind(id.to_string()).execute(&db.pool).await.unwrap();
    let created_at: i64 =
        sqlx::query_scalar("SELECT created_at FROM terminal_projection_outbox WHERE request_id=$1")
            .bind(id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let identity = TerminalRecoveryIdentity {
        request_id: id,
        tenant_id: key.tenant_id,
        key_id: key.key_id,
        account_id: key.account_id,
        reservation_id: reservation.id,
        created_at,
    };
    let before = db
        .recover_one_terminal_projection(identity, false)
        .await
        .unwrap();
    assert_eq!(before.state, "eligible");
    assert!(!before.applied);
    assert!(!before.fact_exists);
    for wrong in [
        TerminalRecoveryIdentity {
            tenant_id: Uuid::now_v7(),
            ..identity
        },
        TerminalRecoveryIdentity {
            created_at: created_at + 1,
            ..identity
        },
        TerminalRecoveryIdentity {
            account_id: Uuid::now_v7(),
            ..identity
        },
    ] {
        assert!(
            db.recover_one_terminal_projection(wrong, true)
                .await
                .is_err()
        );
    }
    let (neighbor_key, neighbor_reservation, neighbor) = admitted(db, true).await;
    db.finish_proxy_request_deferred(finish(&neighbor_key, &neighbor_reservation, neighbor))
        .await
        .unwrap();
    let busy_owner = Uuid::now_v7().to_string();
    sqlx::query("UPDATE terminal_projection_outbox SET lease_owner=$2,lease_expires_at=$3 WHERE request_id=$1").bind(id.to_string()).bind(&busy_owner).bind(unix_millis()+60_000).execute(&db.pool).await.unwrap();
    assert_eq!(
        db.recover_one_terminal_projection(identity, true)
            .await
            .unwrap()
            .state,
        "busy"
    );
    let retained: String = sqlx::query_scalar(
        "SELECT lease_owner FROM terminal_projection_outbox WHERE request_id=$1",
    )
    .bind(id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(retained, busy_owner);
    sqlx::query("UPDATE terminal_projection_outbox SET lease_expires_at=0 WHERE request_id=$1")
        .bind(id.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    let ledger_before:i64=sqlx::query_scalar("SELECT SUM(amount_micros) FROM ledger_entries WHERE account_id=$1 AND key_id=$2 AND source=$3 AND kind='usage'").bind(key.account_id.to_string()).bind(key.key_id.to_string()).bind(reservation.id.to_string()).fetch_one(&db.pool).await.unwrap();
    if db.backend == DatabaseBackend::PostgreSql {
        let mut blocker = db.begin_write_transaction().await.unwrap();
        sqlx::query(
            "SELECT request_id FROM terminal_projection_outbox WHERE request_id=$1 FOR UPDATE",
        )
        .bind(id.to_string())
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
        let skipped = tokio::time::timeout(
            Duration::from_secs(3),
            db.recover_one_terminal_projection(identity, true),
        )
        .await
        .expect("exact claim must skip locked row")
        .unwrap();
        assert_eq!(skipped.state, "not_claimed");
        assert!(!skipped.applied);
        blocker.rollback().await.unwrap();
    }
    let first = db
        .recover_one_terminal_projection(identity, true)
        .await
        .unwrap();
    assert!(first.applied);
    assert!(!first.replay_applied);
    assert_eq!(first.state, "already_done");
    assert!(first.fact_exists && first.account_acknowledged && first.statistics_acknowledged);
    let totals_before: i64 =
        sqlx::query_scalar("SELECT requests FROM request_daily_aggregates WHERE key_id=$1")
            .bind(key.key_id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let account_total: i64 = sqlx::query_scalar(
        "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id=$1",
    )
    .bind(key.account_id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let second = db
        .recover_one_terminal_projection(identity, true)
        .await
        .unwrap();
    assert!(!second.applied && !second.replay_applied);
    assert_eq!(second.state, "already_done");
    let replay_total: i64 = sqlx::query_scalar(
        "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id=$1",
    )
    .bind(key.account_id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(account_total, replay_total);
    let reservation_state =
        sqlx::query("SELECT status,actual_micros FROM usage_reservations WHERE id=$1")
            .bind(reservation.id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(reservation_state.get::<String, _>("status"), "settled");
    assert_eq!(reservation_state.get::<i64, _>("actual_micros"), 10);
    let row=sqlx::query("SELECT (SELECT COUNT(*) FROM ledger_entries WHERE account_id=$1 AND key_id=$2 AND source=$3 AND kind='usage') AS ledger_count,(SELECT SUM(amount_micros) FROM ledger_entries WHERE account_id=$1 AND key_id=$2 AND source=$3 AND kind='usage') AS ledger_amount,(SELECT COUNT(*) FROM request_stats_facts WHERE request_id=$4) AS facts,(SELECT attempts FROM terminal_projection_outbox WHERE request_id=$5) AS neighbor_attempts,(SELECT requests FROM request_daily_aggregates WHERE key_id=$2) AS requests")
        .bind(key.account_id.to_string()).bind(key.key_id.to_string()).bind(reservation.id.to_string()).bind(id.to_string()).bind(neighbor.to_string()).fetch_one(&db.pool).await.unwrap();
    assert_eq!(row.get::<i64, _>("ledger_count"), 1);
    assert_eq!(row.get::<i64, _>("ledger_amount"), ledger_before);
    assert_eq!(row.get::<i64, _>("facts"), 1);
    assert_eq!(row.get::<i64, _>("neighbor_attempts"), 0);
    assert_eq!(row.get::<i64, _>("requests"), totals_before);
}

#[tokio::test]
async fn sqlite_exact_terminal_recovery_preserves_identity_lease_and_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("exact.db").display()
    ))
    .await
    .unwrap();
    db.migrate().await.unwrap();
    exact_contract(&db).await;
}
#[tokio::test]
async fn postgres_exact_terminal_recovery_preserves_identity_lease_and_once() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        assert!(std::env::var_os("CI").is_none(), "PostgreSQL required");
        return;
    };
    let isolated = PostgresTestDatabase::create(&url).await;
    let db = Database::connect(isolated.url()).await.unwrap();
    db.migrate().await.unwrap();
    exact_contract(&db).await;
    db.pool.close().await;
    isolated.cleanup().await;
}
