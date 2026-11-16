use super::*;

async fn projection_contract(database: &Database) {
    database.migrate().await.unwrap();
    database.history_start().await.unwrap();
    if matches!(database.backend, DatabaseBackend::PostgreSql) {
        let old = database
            .history_capture_definitions_with_upstream_projection(false)
            .unwrap()
            .into_iter()
            .find(|d| d.name == "history_upstream_accounts_update")
            .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE OR REPLACE FUNCTION {}() RETURNS trigger LANGUAGE plpgsql VOLATILE SECURITY INVOKER AS $history${}$history$; COMMENT ON FUNCTION {}() IS '{}';",
            old.name, old.body, old.name, old.manifest,
        ))).execute(&database.pool).await.unwrap();
        let oid: i64 = sqlx::query_scalar("SELECT oid::BIGINT FROM pg_trigger WHERE tgrelid='upstream_accounts'::regclass AND tgname='history_upstream_accounts_update'")
            .fetch_one(&database.pool).await.unwrap();
        let epoch: i64 =
            sqlx::query_scalar("SELECT epoch FROM request_history_activation WHERE id=1")
                .fetch_one(&database.pool)
                .await
                .unwrap();
        database.history_upgrade_upstream_capture().await.unwrap();
        database.history_upgrade_upstream_capture().await.unwrap();
        let after: i64 = sqlx::query_scalar("SELECT oid::BIGINT FROM pg_trigger WHERE tgrelid='upstream_accounts'::regclass AND tgname='history_upstream_accounts_update'")
            .fetch_one(&database.pool).await.unwrap();
        assert_eq!(
            oid, after,
            "function-only upgrade preserves the installed trigger"
        );
        let after_epoch: i64 =
            sqlx::query_scalar("SELECT epoch FROM request_history_activation WHERE id=1")
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert_eq!(
            epoch, after_epoch,
            "upgrade must not restart activation coverage"
        );
    }
    let tenant = Uuid::now_v7().to_string();
    let key = Uuid::now_v7().to_string();
    let account = Uuid::now_v7().to_string();
    let moved_account = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO upstream_accounts (id,tenant_id,name,driver,auth_kind,config_json,status,credential_generation,created_at,updated_at) VALUES ($1,$2,'before','openai','api_key','{}','active',1,1,1)")
        .bind(&account).bind(&tenant).execute(&database.pool).await.unwrap();
    for target in [&account, &moved_account] {
        let native = Uuid::now_v7().to_string();
        let generation = Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO request_records (id,tenant_id,key_id,created_at,protocol,model,input_tokens,output_tokens,cost_micros,request_object,reservation_id,currency,upstream_account_id) VALUES ($1,$2,$3,1,'openai','fixture',0,0,0,'gap://fixture/request',$4,'USD',$5)")
            .bind(&native).bind(&tenant).bind(&key).bind(Uuid::now_v7().to_string()).bind(target).execute(&database.pool).await.unwrap();
        sqlx::query("INSERT INTO generation_jobs (id,tenant_id,key_id,upstream_account_id,reservation_id,public_model,upstream_model,driver,status,request_object,estimated_units,next_attempt_at,created_at,updated_at) VALUES ($1,$2,$3,$4,$5,'fixture','fixture','http-json','queued','gap://fixture/generation',1,1,1,1)")
            .bind(&generation).bind(&tenant).bind(&key).bind(target).bind(Uuid::now_v7().to_string()).execute(&database.pool).await.unwrap();
        for parent in [&native, &generation] {
            sqlx::query("INSERT INTO request_events (event_id,tenant_id,key_id,request_id,event_at,event_kind,protocol,model,status_code,input_tokens,output_tokens,cost_micros) VALUES ($1,$2,$3,$4,1,'finished','openai','fixture',200,0,0,0)")
                .bind(Uuid::now_v7().to_string()).bind(&tenant).bind(&key).bind(parent).execute(&database.pool).await.unwrap();
        }
    }
    sqlx::query("DELETE FROM request_history_export")
        .execute(&database.pool)
        .await
        .unwrap();
    let before = database.history_revision().await.unwrap();
    sqlx::query("UPDATE upstream_accounts SET credential_generation=credential_generation+1,updated_at=updated_at+1,config_json='{}',status='inactive' WHERE id=$1")
        .bind(&account).execute(&database.pool).await.unwrap();
    assert_eq!(
        database.history_revision().await.unwrap(),
        before,
        "credential and operational state do not change the historical account projection"
    );
    let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_history_export")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(queued, 0);
    sqlx::query("UPDATE upstream_accounts SET name='after' WHERE id=$1")
        .bind(&account)
        .execute(&database.pool)
        .await
        .unwrap();
    let renamed = database.history_revision().await.unwrap();
    assert_eq!(
        renamed - before,
        4,
        "rename captures native, generation and their two events exactly once"
    );
    let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_history_export")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(queued, 4);
    sqlx::query("DELETE FROM request_history_export")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE upstream_accounts SET id=$1 WHERE id=$2")
        .bind(&moved_account)
        .bind(&account)
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        database.history_revision().await.unwrap() - renamed,
        8,
        "identity move retains both account dependency sets"
    );
    let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_history_export")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(queued, 8);
}

#[tokio::test]
async fn request_history_upstream_projection_ignores_operational_updates() {
    let database = Database::connect("sqlite::memory:").await.unwrap();
    projection_contract(&database).await;
    database.close().await;
}

#[tokio::test]
#[ignore = "existing CI supplies the real PostgreSQL service"]
async fn request_history_real_services_acceptance_upstream_projection_upgrade_and_fanout() {
    let postgres = std::env::var("MTC_TEST_POSTGRES_URL").unwrap();
    let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
    let schema = format!("history_upstream_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut url = url::Url::parse(&postgres).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect_with_max(url.as_str(), 1).await.unwrap();
    projection_contract(&database).await;
    database.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
