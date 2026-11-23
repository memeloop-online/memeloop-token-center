use super::*;
use crate::db::test_support::PostgresTestDatabase;

async fn replacement_and_conflict(database: &Database) {
    let (key, reservation, request_id) = admitted(database, true).await;
    let mut zero = finish(&key, &reservation, request_id);
    zero.usage = TokenUsage::default();
    zero.usage_basis = Some(crate::model::RequestUsageBasis::NotObserved);
    database.finish_proxy_request_deferred(zero).await.unwrap();
    let owner = Uuid::now_v7();
    database
        .claim_terminal_projection_tasks(owner, 32)
        .await
        .unwrap();
    assert!(
        database
            .project_claimed_terminal_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    let ledger_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries WHERE source = $1")
            .bind(reservation.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    sqlx::query("INSERT INTO pending_proxy_settlements (request_id,tenant_id,key_id,account_id,reservation_id,owner_token,fence,contract_digest,contract_json,state,unknown_reason,created_at,updated_at) SELECT request_id,tenant_id,key_id,account_id,reservation_id,'test-owner','test-fence','test-contract','{}','pending','authoritative_usage_unavailable',created_at,completed_at FROM terminal_projection_outbox WHERE request_id = $1")
        .bind(request_id.to_string()).execute(&database.pool).await.unwrap();
    sqlx::query("UPDATE terminal_projection_outbox SET projected_at = NULL, statistics_outcome = NULL, lease_owner = $2, lease_expires_at = $3, input_tokens = 7, output_tokens = 3, usage_basis = 'provider_reported' WHERE request_id = $1")
        .bind(request_id.to_string()).bind(owner.to_string()).bind(unix_millis() + 60_000).execute(&database.pool).await.unwrap();
    assert!(
        database
            .project_claimed_terminal_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    let row = sqlx::query(
        "SELECT input_tokens,output_tokens FROM request_stats_facts WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("input_tokens"), 7);
    assert_eq!(row.get::<i64, _>("output_tokens"), 3);
    // A non-placeholder contribution must still conflict rather than be applied twice.
    sqlx::query("UPDATE terminal_projection_outbox SET projected_at = NULL, lease_owner = $2, lease_expires_at = $3 WHERE request_id = $1")
        .bind(request_id.to_string()).bind(owner.to_string()).bind(unix_millis() + 60_000).execute(&database.pool).await.unwrap();
    assert!(matches!(
        database
            .project_claimed_terminal_projection_task(owner, request_id)
            .await,
        Err(AppError::Conflict(_))
    ));
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_stats_facts WHERE request_id = $1")
            .bind(request_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    let ledger_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries WHERE source = $1")
            .bind(reservation.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(ledger_before, ledger_after);
}

#[tokio::test]
async fn sqlite_terminal_fact_read_preserves_unknown_replacement_and_conflict() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("fact-read.db").display()
    ))
    .await
    .unwrap();
    db.migrate().await.unwrap();
    replacement_and_conflict(&db).await;
}

#[tokio::test]
async fn postgres_terminal_fact_read_preserves_unknown_replacement_and_conflict() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "PostgreSQL required in CI"
        );
        return;
    };
    let isolated = PostgresTestDatabase::create(&url).await;
    let db = Database::connect(isolated.url()).await.unwrap();
    db.migrate().await.unwrap();
    replacement_and_conflict(&db).await;
    db.pool.close().await;
    isolated.cleanup().await;
}
