//! Actual relation-lock footprints, including the publication/history triggers.
use super::*;

async fn relation_footprint(db: &Database, pid: i32) -> (i64, i64) {
    let row = sqlx::query("WITH leaves AS (SELECT relid FROM pg_partition_tree('request_records') WHERE isleaf), family AS (SELECT relid AS oid FROM leaves UNION SELECT indexrelid FROM pg_index JOIN leaves ON indrelid=relid) SELECT COUNT(DISTINCT l.relation) AS family, COUNT(DISTINCT l.relation) FILTER (WHERE l.relation IN (SELECT relid FROM leaves)) AS leaves FROM pg_locks l WHERE l.pid=$1 AND l.granted AND l.relation IN (SELECT oid FROM family)")
        .bind(pid).fetch_one(&db.pool).await.unwrap();
    (
        row.try_get("leaves").unwrap(),
        row.try_get("family").unwrap(),
    )
}

async fn held_reopen_footprint(db: &Database, original: bool) -> (i64, i64) {
    let mut blocker = db.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(73426, 119)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let worker = db.clone();
    let task = tokio::spawn(async move {
        if original {
            // The old production query, retained only as the measured baseline.
            sqlx::query("UPDATE conversation_projection_outbox SET projected_at=NULL, lease_owner='session-body-rebuild', statistics_outcome='session_body' WHERE request_id IN (SELECT o.request_id FROM conversation_projection_outbox o JOIN request_records r ON r.id=o.request_id WHERE o.projected_at IS NOT NULL AND o.statistics_outcome IS DISTINCT FROM 'session_body' AND r.request_object<>'' AND r.response_object IS NOT NULL AND r.response_object<>'' ORDER BY o.observed_at,o.request_id LIMIT 1 FOR UPDATE OF o SKIP LOCKED)")
                .execute(&worker.pool).await.unwrap().rows_affected()
        } else {
            worker.reopen_session_body_rebuilds(1).await.unwrap()
        }
    });
    let pid = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let pid: Option<i32> = sqlx::query_scalar("SELECT pid FROM pg_locks WHERE locktype='advisory' AND classid=73426 AND objid=119 AND NOT granted")
                .fetch_optional(&db.pool).await.unwrap();
            if let Some(pid) = pid { break pid; }
            assert!(!task.is_finished(), "reopen must reach the real UPDATE trigger");
            tokio::task::yield_now().await;
        }
    }).await;
    let footprint = match pid {
        Ok(pid) => relation_footprint(db, pid).await,
        Err(error) => {
            task.abort();
            blocker.rollback().await.unwrap();
            panic!("reopen did not reach the lock observation boundary: {error}");
        }
    };
    blocker.rollback().await.unwrap();
    assert_eq!(task.await.unwrap(), 1);
    footprint
}

#[tokio::test]
async fn postgres_rebuild_partition_locks_and_ineligible_prefix() {
    let database_url = std::env::var("MTC_TEST_POSTGRES_URL")
        .expect("partition lock regression requires PostgreSQL");
    for mode in ["force_generic_plan", "force_custom_plan"] {
        let isolated = PostgresTestDatabase::create(&database_url).await;
        let mut url = url::Url::parse(isolated.url()).unwrap();
        url.query_pairs_mut()
            .append_pair("options", &format!("-c plan_cache_mode={mode}"));
        let fixture = Fixture::new(url.to_string(), true).await;
        let db = &fixture.state.db;
        let actual: String = sqlx::query_scalar("SHOW plan_cache_mode")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(actual, mode);
        let schema: String = sqlx::query_scalar("SELECT current_schema()::text")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(schema, "public");
        // Add empty historical leaves outside the product's normal current horizon.
        for day in 1..=8_i64 {
            sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TABLE request_records_lock_probe_{day} PARTITION OF request_records FOR VALUES FROM ({}) TO ({})", day*86400000, (day+1)*86400000)))
                .execute(&db.pool).await.unwrap();
        }
        sqlx::raw_sql("CREATE FUNCTION rebuild_lock_probe() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.lease_owner='session-body-rebuild' AND OLD.projected_at IS NOT NULL THEN PERFORM pg_advisory_xact_lock(73426,119); END IF; RETURN NEW; END $$; CREATE TRIGGER zzzz_rebuild_lock_probe AFTER UPDATE ON conversation_projection_outbox FOR EACH ROW EXECUTE FUNCTION rebuild_lock_probe();")
            .execute(&db.pool).await.unwrap();
        sqlx::query("UPDATE conversation_projection_outbox SET projected_at=1, statistics_outcome=NULL WHERE request_id=$1")
            .bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
        let original = held_reopen_footprint(db, true).await;
        sqlx::query("UPDATE conversation_projection_outbox SET projected_at=1, statistics_outcome=NULL WHERE request_id=$1")
            .bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
        // Checked but source-incomplete rows span the old leaves. Their locks
        // must be released before the real eligible row reaches its UPDATE.
        for day in 1..=8_i64 {
            let id = Uuid::now_v7().to_string();
            sqlx::query("INSERT INTO request_records SELECT (jsonb_populate_record(NULL::request_records,to_jsonb(r)||jsonb_build_object('id',$1::text,'created_at',$2::bigint,'request_object',''))).* FROM request_records r WHERE r.id=$3")
                .bind(&id).bind(day*86400000+1).bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
            sqlx::query("INSERT INTO request_record_locators(id,tenant_id,key_id,created_at) SELECT $1,tenant_id,key_id,$2 FROM request_record_locators WHERE id=$3")
                .bind(&id).bind(day*86400000+1).bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
            sqlx::query("INSERT INTO conversation_projection_outbox(request_id,tenant_id,key_id,principal_id,request_json,hints_json,observed_at,projected_at) SELECT $1,tenant_id,key_id,principal_id,request_json,hints_json,$2,1 FROM conversation_projection_outbox WHERE request_id=$3")
                .bind(&id).bind(day).bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
        }
        let routed = held_reopen_footprint(db, false).await;
        eprintln!(
            "rebuild_partition_locks mode={mode} original={original:?} authoritative={routed:?}"
        );
        assert!(
            original.0 >= 8,
            "baseline must really touch historical leaves"
        );
        assert_eq!(
            routed.0, 1,
            "only the authoritative request leaf may be locked"
        );
        assert!(
            routed.1 < original.1,
            "table and index footprint must decrease"
        );
        // More than one page without authoritative locators cannot hide real work.
        sqlx::query("UPDATE conversation_projection_outbox SET projected_at=1, statistics_outcome=NULL, lease_owner=NULL WHERE request_id=$1")
            .bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
        for index in 0..105_i64 {
            sqlx::query("INSERT INTO conversation_projection_outbox(request_id,tenant_id,key_id,principal_id,request_json,hints_json,observed_at,projected_at) SELECT $1,tenant_id,key_id,principal_id,request_json,hints_json,$2,1 FROM conversation_projection_outbox WHERE request_id=$3")
                .bind(Uuid::now_v7().to_string()).bind(index).bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
        }
        assert_eq!(db.reopen_session_body_rebuilds(1).await.unwrap(), 1);
        assert_eq!(db.reopen_session_body_rebuilds(1).await.unwrap(), 0);
        // A locked earlier eligible row must not prevent a later candidate
        // from filling the remaining capacity, and then returns after unlock.
        let later = Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO request_records SELECT (jsonb_populate_record(NULL::request_records,to_jsonb(r)||jsonb_build_object('id',$1::text))).* FROM request_records r WHERE r.id=$2")
            .bind(&later).bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
        sqlx::query("INSERT INTO request_record_locators(id,tenant_id,key_id,created_at) SELECT $1,tenant_id,key_id,created_at FROM request_record_locators WHERE id=$2")
            .bind(&later).bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
        sqlx::query("INSERT INTO conversation_projection_outbox(request_id,tenant_id,key_id,principal_id,request_json,hints_json,observed_at,projected_at) SELECT $1,tenant_id,key_id,principal_id,request_json,hints_json,observed_at+1,1 FROM conversation_projection_outbox WHERE request_id=$2")
            .bind(&later).bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
        sqlx::query("UPDATE conversation_projection_outbox SET projected_at=1,statistics_outcome=NULL,lease_owner=NULL WHERE request_id=$1")
            .bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
        let mut held = db.pool.begin().await.unwrap();
        sqlx::query(
            "SELECT request_id FROM conversation_projection_outbox WHERE request_id=$1 FOR UPDATE",
        )
        .bind(fixture.request_id.to_string())
        .fetch_one(&mut *held)
        .await
        .unwrap();
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                db.reopen_session_body_rebuilds(1)
            )
            .await
            .expect("locked head must be skipped")
            .unwrap(),
            1
        );
        let picked: String = sqlx::query_scalar(
            "SELECT lease_owner FROM conversation_projection_outbox WHERE request_id=$1",
        )
        .bind(&later)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(picked, SESSION_BODY_REBUILD_LEASE_OWNER);
        held.rollback().await.unwrap();
        assert_eq!(db.reopen_session_body_rebuilds(2).await.unwrap(), 1);
        // Consume each newly pending row before the next candidate reads capacity.
        // Replenishment still owes only its initial allowance, not the full scan.
        let third = Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO request_records SELECT (jsonb_populate_record(NULL::request_records,to_jsonb(r)||jsonb_build_object('id',$1::text))).* FROM request_records r WHERE r.id=$2")
            .bind(&third).bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
        sqlx::query("INSERT INTO request_record_locators(id,tenant_id,key_id,created_at) SELECT $1,tenant_id,key_id,created_at FROM request_record_locators WHERE id=$2")
            .bind(&third).bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
        sqlx::query("INSERT INTO conversation_projection_outbox(request_id,tenant_id,key_id,principal_id,request_json,hints_json,observed_at,projected_at) SELECT $1,tenant_id,key_id,principal_id,request_json,hints_json,observed_at+2,1 FROM conversation_projection_outbox WHERE request_id=$2")
            .bind(&third).bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
        sqlx::query("UPDATE conversation_projection_outbox SET projected_at=1,statistics_outcome=NULL,lease_owner=NULL WHERE request_id IN ($1,$2)")
            .bind(fixture.request_id.to_string()).bind(&later).execute(&db.pool).await.unwrap();
        sqlx::raw_sql("CREATE OR REPLACE FUNCTION rebuild_lock_probe() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.lease_owner='session-body-rebuild' AND OLD.projected_at IS NOT NULL AND NEW.projected_at IS NULL THEN UPDATE conversation_projection_outbox SET projected_at=1 WHERE projected_at IS NULL AND lease_owner='session-body-rebuild'; END IF; RETURN NEW; END $$;")
            .execute(&db.pool).await.unwrap();
        assert_eq!(
            db.reopen_session_body_rebuilds(2).await.unwrap(),
            2,
            "a concurrent consumer cannot expand this call's initial allowance"
        );
        let untouched: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_projection_outbox WHERE request_id IN ($1,$2,$3) AND statistics_outcome IS DISTINCT FROM 'session_body'")
            .bind(fixture.request_id.to_string()).bind(&later).bind(&third).fetch_one(&db.pool).await.unwrap();
        assert_eq!(untouched, 1);
        drop(fixture);
        isolated.cleanup().await;
    }
}
