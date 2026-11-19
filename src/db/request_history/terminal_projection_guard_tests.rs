use super::*;

#[tokio::test]
#[ignore = "existing CI supplies the real PostgreSQL service"]
async fn request_history_real_services_acceptance_terminal_claim_ignores_export_lock() {
    let postgres = std::env::var("MTC_TEST_POSTGRES_URL").unwrap();
    let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
    let schema = format!("history_terminal_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut url = url::Url::parse(&postgres).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect_with_max(url.as_str(), 4).await.unwrap();
    database.migrate().await.unwrap();
    database.history_start().await.unwrap();
    let old = database
        .history_capture_definitions_with_projection_guards(true, false)
        .unwrap()
        .into_iter()
        .find(|d| d.name == "history_terminal_projection_outbox_update")
        .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE OR REPLACE FUNCTION {}() RETURNS trigger LANGUAGE plpgsql VOLATILE SECURITY INVOKER AS $history${}$history$; COMMENT ON FUNCTION {}() IS '{}';",old.name,old.body,old.name,old.manifest))).execute(&database.pool).await.unwrap();
    let oid: i64 = sqlx::query_scalar("SELECT oid::BIGINT FROM pg_trigger WHERE tgrelid='terminal_projection_outbox'::regclass AND tgname='history_terminal_projection_outbox_update'").fetch_one(&database.pool).await.unwrap();
    database.history_upgrade_terminal_capture().await.unwrap();
    database.history_upgrade_terminal_capture().await.unwrap();
    let after: i64 = sqlx::query_scalar("SELECT oid::BIGINT FROM pg_trigger WHERE tgrelid='terminal_projection_outbox'::regclass AND tgname='history_terminal_projection_outbox_update'").fetch_one(&database.pool).await.unwrap();
    assert_eq!(oid, after);
    let id = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO request_records (id,tenant_id,key_id,created_at,protocol,model,input_tokens,output_tokens,cost_micros,request_object,reservation_id,currency) VALUES ($1,'tenant','key',1,'openai','fixture',1,1,1,'gap://fixture/request',$2,'USD')").bind(&id).bind(Uuid::now_v7().to_string()).execute(&database.pool).await.unwrap();
    sqlx::query("INSERT INTO terminal_projection_outbox (request_id,reservation_id,tenant_id,key_id,account_id,created_at,completed_at,model,protocol,status_code,error_code,upstream_account_id,model_route_id,duration_ms,input_tokens,output_tokens,cached_input_tokens,cache_write_tokens,generation_units,billing_unit,service_tier,currency,cost_micros,usage_basis,session_id,response_object) VALUES ($1,$2,'tenant','key','account',1,2,'fixture','openai',200,'','','',1,1,1,0,0,0,'','default','USD',1,'provider_reported','session','gap://fixture/response')").bind(&id).bind(Uuid::now_v7().to_string()).execute(&database.pool).await.unwrap();
    let before = database.history_revision().await.unwrap();
    let mut blocker = database.pool.begin().await.unwrap();
    sqlx::query("SELECT request_id FROM request_history_export WHERE source='native' AND request_id=$1 FOR UPDATE").bind(&id).fetch_one(&mut *blocker).await.unwrap();
    let owner = Uuid::now_v7();
    let tasks = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        database.claim_terminal_projection_tasks(owner, 1),
    )
    .await
    .expect("claim must not wait on history export")
    .unwrap();
    assert_eq!(tasks, vec![Uuid::parse_str(&id).unwrap()]);
    blocker.rollback().await.unwrap();
    assert_eq!(database.history_revision().await.unwrap(), before);
    sqlx::query("UPDATE terminal_projection_outbox SET cost_micros=2 WHERE request_id=$1")
        .bind(&id)
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        database.history_revision().await.unwrap() > before,
        "business updates retain capture"
    );
    let before_ack = database.history_revision().await.unwrap();
    sqlx::query("UPDATE terminal_projection_outbox SET projected_at=3 WHERE request_id=$1")
        .bind(&id)
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        database.history_revision().await.unwrap() > before_ack,
        "financial acknowledgment retains capture"
    );
    database.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
