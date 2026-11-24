use super::*;
use std::time::Duration;

fn draft(now: i64) -> EventDraft {
    EventDraft {
        event_id: Uuid::nil().to_string(),
        tenant_id: Uuid::now_v7().to_string(),
        key_id: Uuid::now_v7().to_string(),
        request_id: Uuid::now_v7().to_string(),
        event_at: now,
        event_kind: "started".into(),
        protocol: "openai".into(),
        model: "prepared-event-contract".into(),
        status_code: None,
        duration_ms: None,
        input_tokens: 0,
        output_tokens: 0,
        cost_micros: 0,
        error_code: None,
    }
}

#[tokio::test]
#[ignore = "requires CI PostgreSQL"]
async fn postgres_slow_event_preparation_preserves_cursor_progress_originals_and_rollback() {
    let url = std::env::var("MTC_TEST_POSTGRES_URL").expect("CI PostgreSQL required");
    let isolated = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    let database = Database::connect(isolated.url()).await.unwrap();
    database.migrate().await.unwrap();
    sqlx::query("UPDATE request_history_state SET mode='shadow' WHERE id=1")
        .execute(&database.pool)
        .await
        .unwrap();
    let now = unix_millis();
    let first = draft(now);
    let second = draft(now);
    let first_id = first.request_id.clone();
    let mut gate = database.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE generation_jobs IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *gate)
        .await
        .unwrap();
    let mut preparing = database.pool.begin().await.unwrap();
    let preparing_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *preparing)
        .await
        .unwrap();
    let work = tokio::spawn(async move {
        let prepared = first.prepare(&mut preparing).await.unwrap();
        (preparing, prepared)
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock' AND wait_event='relation')")
                .bind(preparing_pid).fetch_one(&database.pool).await.unwrap();
            if waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("draft reaches real parent-table lock");
    // Another independent writer can commit a cursor while preparation is held.
    let mut writer = database.pool.begin().await.unwrap();
    let second_cursor = tokio::time::timeout(Duration::from_secs(5), async {
        let cursor = crate::db::allocate_request_event_cursor(&mut writer, now, &second.tenant_id, &second.key_id, &second.request_id).await.unwrap();
        let mut row = second.clone(); row.event_id=cursor.event_id.clone(); row.event_at=cursor.event_at;
        row.bind(sqlx::query("INSERT INTO request_events (event_id,tenant_id,key_id,request_id,event_at,event_kind,protocol,model,status_code,duration_ms,input_tokens,output_tokens,cost_micros,error_code) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)"))
            .execute(&mut *writer).await.unwrap();
        writer.commit().await.unwrap();
        cursor
    }).await.expect("parent capture holds no global cursor lock");
    gate.rollback().await.unwrap();
    let (mut transaction, prepared) = work.await.unwrap();
    PreparedEventCapture::publish(&mut transaction, Some(prepared), now)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    let row = sqlx::query("SELECT event_id,event_at FROM request_events WHERE request_id=$1")
        .bind(&first_id)
        .fetch_one(&database.pool)
        .await
        .unwrap();
    let event_id: String = row.try_get("event_id").unwrap();
    let event_at: i64 = row.try_get("event_at").unwrap();
    assert!((event_at, &event_id) > (second_cursor.event_at, &second_cursor.event_id));
    let mut compare = database.pool.begin().await.unwrap();
    // Reusing the original capture must match the prepared immutable bytes exactly.
    super::super::capture_event_in_transaction(&mut compare, &event_id, event_at)
        .await
        .unwrap();
    compare.rollback().await.unwrap();
    let rolled = draft(now);
    let rolled_id = rolled.request_id.clone();
    let mut rollback = database.pool.begin().await.unwrap();
    let prepared = rolled.prepare(&mut rollback).await.unwrap();
    PreparedEventCapture::publish(&mut rollback, Some(prepared), now)
        .await
        .unwrap();
    rollback.rollback().await.unwrap();
    let counts: (i64,i64,i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM request_events WHERE request_id=$1),(SELECT COUNT(*) FROM request_event_locators WHERE request_id=$1),(SELECT COUNT(*) FROM request_history_immutable_outbox WHERE snapshot LIKE $2)")
        .bind(&rolled_id).bind(format!("%{rolled_id}%")).fetch_one(&database.pool).await.unwrap();
    assert_eq!(counts, (0, 0, 0));
    assert_eq!(
        PreparedEventCapture::publish(&mut database.pool.begin().await.unwrap(), None, now)
            .await
            .unwrap(),
        0
    );
    database.close().await;
    isolated.cleanup().await;
}
