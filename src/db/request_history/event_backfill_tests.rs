use super::*;

#[tokio::test]
async fn sqlite_event_backfill_uses_time_cursor_and_reuses_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("history.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    assert_event_cursor(&database).await;
}

#[tokio::test]
#[ignore = "requires the existing CI PostgreSQL service"]
async fn request_history_real_services_acceptance_event_backfill_time_cursor() {
    let postgres = std::env::var("MTC_TEST_POSTGRES_URL").unwrap();
    let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
    let schema = format!("history_event_cursor_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&postgres).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect(isolated.as_str()).await.unwrap();
    database.migrate().await.unwrap();
    assert_event_cursor(&database).await;
    database.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}

async fn assert_event_cursor(database: &Database) {
    sqlx::query("UPDATE request_history_state SET mode='shadow' WHERE id=1")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE request_history_backfill SET done=1 WHERE source<>'event'")
        .execute(&database.pool)
        .await
        .unwrap();
    let tenant = Uuid::now_v7().to_string();
    let key = Uuid::now_v7().to_string();
    let ids = [
        "00000000-0000-4000-8000-000000000003",
        "00000000-0000-4000-8000-000000000001",
        "00000000-0000-4000-8000-000000000002",
    ];
    let now = unix_millis();
    let times = [now - 172800000, now, now];
    // Earlier time has the largest UUID; two later events share a millisecond.
    for (id, time) in ids.iter().zip(times) {
        sqlx::query("INSERT INTO request_events (event_id,tenant_id,key_id,request_id,event_at,event_kind,protocol,model,input_tokens,output_tokens,cost_micros) VALUES ($1,$2,$3,$1,$4,'finished','openai','fixture',0,0,0)")
            .bind(id).bind(&tenant).bind(&key).bind(time).execute(&database.pool).await.unwrap();
    }
    sqlx::query("UPDATE request_history_backfill SET after_id=$1 WHERE source='event'")
        .bind(ids[0])
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO request_history_identities (source,request_id,tenant_id,key_id,created_at,head_revision,acknowledged_revision) VALUES ('event',$1,$2,$3,$4,7,7)")
        .bind(ids[0]).bind(&tenant).bind(&key).bind(times[0]).execute(&database.pool).await.unwrap();
    assert_eq!(database.history_backfill_page(2).await.unwrap(), 2);
    let row = sqlx::query("SELECT after_id,after_created_at,legacy_after_id FROM request_history_backfill WHERE source='event'").fetch_one(&database.pool).await.unwrap();
    assert_eq!(row.try_get::<String, _>("legacy_after_id").unwrap(), ids[0]);
    assert_eq!(row.try_get::<String, _>("after_id").unwrap(), ids[1]);
    assert_eq!(row.try_get::<i64, _>("after_created_at").unwrap(), times[1]);
    // A new handle resumes the durable cursor; no process-local progress exists.
    assert_eq!(database.clone().history_backfill_page(2).await.unwrap(), 1);
    assert!(database.history_backfill_done().await.unwrap());
    let queued: Vec<String> = sqlx::query_scalar(
        "SELECT request_id FROM request_history_export WHERE source='event' ORDER BY request_id",
    )
    .fetch_all(&database.pool)
    .await
    .unwrap();
    assert_eq!(queued, vec![ids[1].to_owned(), ids[2].to_owned()]);
    let revision: i64 = sqlx::query_scalar("SELECT head_revision FROM request_history_identities WHERE source='event' AND request_id=$1").bind(ids[0]).fetch_one(&database.pool).await.unwrap();
    assert_eq!(revision, 7);
    // Replay a committed page: existing queue ownership is reused.
    sqlx::query("UPDATE request_history_backfill SET done=0,after_created_at=NULL,after_id=$1 WHERE source='event'").bind(ids[0]).execute(&database.pool).await.unwrap();
    assert_eq!(database.history_backfill_page(4).await.unwrap(), 3);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_history_export WHERE source='event'")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(count, 2);
    sqlx::query("DELETE FROM request_history_export")
        .execute(&database.pool)
        .await
        .unwrap();
    database.history_refresh().await.unwrap();
    sqlx::query("UPDATE request_history_backfill SET done=1 WHERE source<>'event'")
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(database.history_backfill_page(4).await.unwrap(), 3);
    let refreshed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_history_export WHERE source='event'")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(
        refreshed, 3,
        "explicit read-model refresh re-exports all retained events"
    );
}
