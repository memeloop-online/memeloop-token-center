use super::*;
use crate::db::history_immutable_outbox::{EventDraft, PreparedEventCapture};
use std::time::Duration;

#[tokio::test]
#[ignore = "requires CI PostgreSQL"]
async fn postgres_rich_event_parent_preparation_does_not_hold_global_cursor() {
    let fixture = Fixture::postgres().await.expect("CI PostgreSQL required");
    let db = &fixture.database;
    // Build two owned native requests through the production admission path.
    let price = db
        .upsert_model_price("operational-model", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let second_id = Uuid::now_v7();
    db.start_proxy_request(StartProxyRequest {
        request_id: second_id,
        key: &fixture.key,
        price: &price,
        input_token_ceiling: 10,
        output_token_ceiling: 20,
        protocol: "openai",
        model: "operational-model",
        request_object: "gap://second/request",
        upstream_account_id: None,
        model_route_id: None,
    })
    .await
    .unwrap();
    let terminal = fixture.operational(200);
    db.record_pending_proxy_operational_terminal(
        &fixture.claim,
        Uuid::now_v7(),
        &terminal,
        ProxyRequestUpstreamAttribution::KeepSelected,
    )
    .await
    .unwrap();
    let first_id = fixture.claim.request_id;
    for request in [first_id, second_id] {
        sqlx::query("UPDATE request_records SET completed_at=$1,status_code=200,input_tokens=3,output_tokens=2,cached_input_tokens=1,cost_micros=7,response_object='gap://completed/response' WHERE id=$2")
            .bind(unix_millis()).bind(request.to_string()).execute(&db.pool).await.unwrap();
        sqlx::query("INSERT INTO request_egress_attempts (request_id,attempt_id,outbound_attempt,connect_attempt,route_id,upstream_account_id,credential_generation,transport_revision,observed_at,snapshot_json,transport_json,final_response) VALUES ($1,$2,1,1,$3,$4,1,1,$5,'{\"kind\":\"direct\"}','{}',1)")
            .bind(request.to_string()).bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).bind(unix_millis()).execute(&db.pool).await.unwrap();
    }
    sqlx::query("UPDATE request_history_state SET mode='shadow' WHERE id=1")
        .execute(&db.pool)
        .await
        .unwrap();
    // A test-only view makes exactly one native parent's MVCC read wait on a
    // real PG advisory gate; unrelated native parents remain readable.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE FUNCTION prepared_parent_gate(id TEXT) RETURNS BOOLEAN LANGUAGE plpgsql VOLATILE AS $$ BEGIN IF id='{}' THEN PERFORM pg_advisory_xact_lock(91327,1); END IF; RETURN TRUE; END $$; ALTER TABLE request_records RENAME TO prepared_request_records; CREATE VIEW request_records AS SELECT * FROM prepared_request_records WHERE prepared_parent_gate(id)",first_id)))
        .execute(&db.pool).await.unwrap();
    let mut gate = db.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(91327,1)")
        .execute(&mut *gate)
        .await
        .unwrap();
    let mut first_tx = db.pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *first_tx)
        .await
        .unwrap();
    let draft = EventDraft {
        event_id: Uuid::nil().to_string(),
        tenant_id: fixture.key.tenant_id.to_string(),
        key_id: fixture.key.key_id.to_string(),
        request_id: first_id.to_string(),
        event_at: unix_millis(),
        event_kind: "projected".into(),
        protocol: "openai".into(),
        model: "operational-model".into(),
        status_code: Some(200),
        duration_ms: Some(10),
        input_tokens: 3,
        output_tokens: 2,
        cost_micros: 7,
        error_code: None,
    };
    let preparing = tokio::spawn(async move {
        let prepared = draft.prepare(&mut first_tx).await.unwrap();
        (first_tx, prepared)
    });
    tokio::time::timeout(Duration::from_secs(5),async {loop {
        let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event='advisory')").bind(pid).fetch_one(&db.pool).await.unwrap();
        if waiting {break;} tokio::task::yield_now().await;
    }}).await.expect("native parent enrichment reaches targeted PG gate");
    let mut second_tx = db.pool.begin().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        crate::db::requests::conversations::emit_conversation_projected_event_in_transaction(
            &mut second_tx,
            second_id,
            fixture.key.tenant_id,
            fixture.key.key_id,
        )
        .await
        .unwrap();
        second_tx.commit().await.unwrap();
    })
    .await
    .expect("real unrelated writer commits while first parent is held");
    gate.rollback().await.unwrap();
    let (mut first_tx, prepared) = preparing.await.unwrap();
    PreparedEventCapture::publish(&mut first_tx, Some(prepared), unix_millis())
        .await
        .unwrap();
    first_tx.commit().await.unwrap();
    let events=sqlx::query("SELECT event_id,event_at,request_id FROM request_events WHERE event_kind='projected' AND request_id IN ($1,$2) ORDER BY event_at,event_id")
        .bind(first_id.to_string()).bind(second_id.to_string()).fetch_all(&db.pool).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].try_get::<String, _>("request_id").unwrap(),
        second_id.to_string()
    );
    let first = &events[1];
    let event_id: String = first.try_get("event_id").unwrap();
    let event_at: i64 = first.try_get("event_at").unwrap();
    let mut compare = db.pool.begin().await.unwrap();
    crate::db::history_immutable_outbox::capture_event_in_transaction(
        &mut compare,
        &event_id,
        event_at,
    )
    .await
    .unwrap();
    compare.rollback().await.unwrap();
    let envelope:String=sqlx::query_scalar("SELECT snapshot FROM request_history_immutable_outbox WHERE source='event' AND request_id=$1").bind(&event_id).fetch_one(&db.pool).await.unwrap();
    let envelope: serde_json::Value = serde_json::from_str(&envelope).unwrap();
    assert!(envelope["payload"]["operational_terminal"].is_object());
    assert!(envelope["payload"]["egress"].is_object());
    assert_eq!(envelope["originals"]["event"]["input_tokens"], 3);
    let mut rollback = db.pool.begin().await.unwrap();
    let mut rolled = EventDraft::from_row(
        &sqlx::query("SELECT * FROM request_events WHERE event_id=$1 AND event_at=$2")
            .bind(&event_id)
            .bind(event_at)
            .fetch_one(&mut *rollback)
            .await
            .unwrap(),
    )
    .unwrap();
    rolled.event_id = Uuid::nil().to_string();
    let prepared = rolled.prepare(&mut rollback).await.unwrap();
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_events")
        .fetch_one(&mut *rollback)
        .await
        .unwrap();
    PreparedEventCapture::publish(&mut rollback, Some(prepared), unix_millis())
        .await
        .unwrap();
    rollback.rollback().await.unwrap();
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_events")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(before, after);
    db.close().await;
    fixture.isolated.unwrap().cleanup().await;
}
