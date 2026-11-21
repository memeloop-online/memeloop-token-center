use super::*;
use std::{sync::Arc, time::Duration};

async fn insert_fact(tx: &mut Transaction<'_, Any>, id: &str) {
    sqlx::query("INSERT INTO request_stats_facts (request_id,tenant_id,key_id,created_at,model,protocol,status_class,error_code,upstream_account_id,model_route_id,duration_ms,input_tokens,output_tokens,currency,cost_micros,session_id) VALUES ($1,'tenant','key',1000,'model','openai','success','','','',10,20,3,'USD',7,'session')")
        .bind(id).execute(&mut **tx).await.unwrap();
}

#[tokio::test]
async fn postgres_rebuild_and_live_add_use_the_same_table_order() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let fixture = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    let db = Arc::new(Database::connect_with_max(fixture.url(), 8).await.unwrap());
    db.migrate().await.unwrap();
    let mut seed = db.begin_write_transaction().await.unwrap();
    insert_fact(&mut seed, "first").await;
    add_request_fact_to_session_projection_in_transaction(&mut seed, "first", 1)
        .await
        .unwrap();
    seed.commit().await.unwrap();
    // A missing totals row is the observed failure case: a DELETE does not
    // reserve the absent identity before a rebuild reaches the hourly table.
    sqlx::query("DELETE FROM session_usage_totals")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("CREATE FUNCTION test_pause_hourly_delete() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(88451721); RETURN OLD; END $$")
        .execute(&db.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER test_pause_hourly_delete BEFORE DELETE ON session_usage_hourly FOR EACH ROW EXECUTE FUNCTION test_pause_hourly_delete()")
        .execute(&db.pool).await.unwrap();
    let mut gate = db.pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(88451721)")
        .execute(&mut *gate)
        .await
        .unwrap();
    let rebuild_db = db.clone();
    let rebuild = tokio::spawn(async move {
        let mut tx = rebuild_db.begin_write_transaction().await.unwrap();
        sqlx::query("SET LOCAL statement_timeout='8s'")
            .execute(&mut *tx)
            .await
            .unwrap();
        rebuild_request_session_projection_in_transaction(&mut tx, "tenant", "key", "session")
            .await?;
        tx.commit().await.map_err(AppError::from)
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_locks WHERE locktype='advisory' AND objid=88451721 AND NOT granted")
                .fetch_one(&db.pool).await.unwrap();
            if waiting > 0 { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    let live_db = db.clone();
    let live = tokio::spawn(async move {
        let mut tx = live_db.begin_write_transaction().await.unwrap();
        sqlx::query("SET LOCAL statement_timeout='8s'")
            .execute(&mut *tx)
            .await
            .unwrap();
        insert_fact(&mut tx, "second").await;
        add_request_fact_to_session_projection_in_transaction(&mut tx, "second", 1).await?;
        tx.commit().await.map_err(AppError::from)
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_locks WHERE locktype='transactionid' AND NOT granted",
            )
            .fetch_one(&db.pool)
            .await
            .unwrap();
            if waiting > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    sqlx::query("SELECT pg_advisory_unlock(88451721)")
        .execute(&mut *gate)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        rebuild.await.unwrap().unwrap();
        live.await.unwrap().unwrap();
    })
    .await
    .unwrap();
    for table in [
        "session_usage_totals",
        "session_usage_hourly",
        "session_usage_daily",
    ] {
        let counts: (i64,i64,i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT CAST(SUM(requests) AS BIGINT), CAST(SUM(input_tokens) AS BIGINT), CAST(SUM(cost_micros) AS BIGINT) FROM {table}")))
            .fetch_one(&db.pool).await.unwrap();
        assert_eq!(
            counts,
            (2, 40, 14),
            "{table} preserves both facts exactly once"
        );
    }
}
