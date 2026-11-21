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

fn first_delta() -> RequestSessionDelta {
    RequestSessionDelta {
        tenant_id: "tenant".into(),
        key_id: "key".into(),
        previous_session_id: "session".into(),
        authoritative_session_id: "new-session".into(),
        created_at: 1000,
        model: "model".into(),
        protocol: "openai".into(),
        status_class: "success".into(),
        error_code: "".into(),
        upstream_account_id: "".into(),
        model_route_id: "".into(),
        currency: "USD".into(),
        duration_ms: 10,
        input_tokens: 20,
        output_tokens: 3,
        cached_input_tokens: 0,
        cache_write_tokens: 0,
        generation_units: 0,
        cost_micros: 7,
    }
}

async fn seed_fallback(db: &Database) {
    let mut tx = db.begin_write_transaction().await.unwrap();
    for id in ["first", "second"] {
        insert_fact(&mut tx, id).await;
        add_request_fact_to_session_projection_in_transaction(&mut tx, id, 1)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    sqlx::query("DELETE FROM session_usage_daily")
        .execute(&db.pool)
        .await
        .unwrap();
}

async fn execute_fallback(db: &Database) -> Result<(), AppError> {
    let mut tx = db.begin_write_transaction().await?;
    sqlx::query("UPDATE request_stats_facts SET session_id='new-session' WHERE request_id='first'")
        .execute(&mut *tx)
        .await?;
    remove_request_fact_from_session_projection_in_transaction(&mut tx, &first_delta()).await?;
    add_request_fact_to_session_projection_in_transaction(&mut tx, "first", 1).await?;
    tx.commit().await?;
    Ok(())
}

async fn assert_fallback(db: &Database) {
    let session: String =
        sqlx::query_scalar("SELECT session_id FROM request_stats_facts WHERE request_id='first'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        session, "new-session",
        "outer membership CAS survives fallback"
    );
    for table in [
        "session_usage_totals",
        "session_usage_hourly",
        "session_usage_daily",
    ] {
        let counts:(i64,i64,i64)=sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT CAST(SUM(requests) AS BIGINT),CAST(SUM(input_tokens) AS BIGINT),CAST(SUM(cost_micros) AS BIGINT) FROM {table}"))).fetch_one(&db.pool).await.unwrap();
        assert_eq!(counts, (2, 40, 14), "{table}: no partial decrement escapes");
    }
}

#[tokio::test]
async fn sqlite_delta_fallback_retains_membership_and_exact_totals() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect_with_max(
        &format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("fallback.db").display()
        ),
        4,
    )
    .await
    .unwrap();
    db.migrate().await.unwrap();
    seed_fallback(&db).await;
    execute_fallback(&db).await.unwrap();
    assert_fallback(&db).await;
}

#[tokio::test]
async fn postgres_delta_fallback_releases_aggregate_locks_but_retains_fact_cas() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let fixture = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    let db = Arc::new(Database::connect_with_max(fixture.url(), 8).await.unwrap());
    db.migrate().await.unwrap();
    seed_fallback(&db).await;
    sqlx::query("CREATE FUNCTION pause_fallback_totals() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(88451722); RETURN NEW; END $$").execute(&db.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER pause_fallback_totals BEFORE INSERT ON session_usage_totals FOR EACH ROW EXECUTE FUNCTION pause_fallback_totals()").execute(&db.pool).await.unwrap();
    let mut gate = db.pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(88451722)")
        .execute(&mut *gate)
        .await
        .unwrap();
    let worker_db = db.clone();
    let worker = tokio::spawn(async move { execute_fallback(&worker_db).await });
    tokio::time::timeout(Duration::from_secs(5),async {loop {
        let waiting:i64=sqlx::query_scalar("SELECT COUNT(*) FROM pg_locks WHERE locktype='advisory' AND objid=88451722 AND NOT granted").fetch_one(&db.pool).await.unwrap();
        if waiting>0 {break;}tokio::time::sleep(Duration::from_millis(10)).await;
    }}).await.unwrap();
    let mut peer = db.begin_write_transaction().await.unwrap();
    sqlx::query("SELECT requests FROM session_usage_hourly FOR UPDATE NOWAIT")
        .fetch_all(&mut *peer)
        .await
        .expect("rollback must release the earlier hourly decrement lock");
    peer.rollback().await.unwrap();
    let mut peer = db.begin_write_transaction().await.unwrap();
    let err = sqlx::query(
        "SELECT request_id FROM request_stats_facts WHERE request_id='first' FOR UPDATE NOWAIT",
    )
    .fetch_one(&mut *peer)
    .await
    .err()
    .expect("outer fact CAS must still hold its row lock");
    assert_eq!(
        err.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("55P03"),
        "outer fact CAS remains locked"
    );
    peer.rollback().await.unwrap();
    sqlx::query("SELECT pg_advisory_unlock(88451722)")
        .execute(&mut *gate)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_fallback(&db).await;
}

#[tokio::test]
async fn postgres_replacement_gate_precedes_live_fact_and_rolls_back_cleanly() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let fixture = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    let db = Arc::new(Database::connect_with_max(fixture.url(), 8).await.unwrap());
    db.migrate().await.unwrap();
    let mut seed = db.begin_write_transaction().await.unwrap();
    lock_request_stats_projection_writer_in_transaction(&mut seed)
        .await
        .unwrap();
    insert_fact(&mut seed, "first").await;
    add_request_fact_to_session_projection_in_transaction(&mut seed, "first", 1)
        .await
        .unwrap();
    seed.commit().await.unwrap();

    // Pause a replacement after DELETE, the exact absent-row race from production.
    let mut replacement = db.begin_write_transaction().await.unwrap();
    lock_request_stats_projection_rebuild_in_transaction(&mut replacement)
        .await
        .unwrap();
    sqlx::query("DELETE FROM session_usage_totals")
        .execute(&mut *replacement)
        .await
        .unwrap();
    let live_db = db.clone();
    let live = tokio::spawn(async move {
        let mut tx = live_db.begin_write_transaction().await.unwrap();
        sqlx::query("SET LOCAL statement_timeout='8s'")
            .execute(&mut *tx)
            .await
            .unwrap();
        lock_request_stats_projection_writer_in_transaction(&mut tx).await?;
        insert_fact(&mut tx, "second").await;
        add_request_fact_to_session_projection_in_transaction(&mut tx, "second", 1).await?;
        tx.commit().await.map_err(AppError::from)
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_locks WHERE locktype='advisory' AND mode='ShareLock' AND NOT granted")
                .fetch_one(&db.pool).await.unwrap();
            if waiting > 0 { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    let absent: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_stats_facts WHERE request_id='second'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        absent, 0,
        "live writer waits before publishing or locking its fact"
    );
    rebuild_request_session_projection_in_transaction(&mut replacement, "tenant", "key", "session")
        .await
        .unwrap();
    replacement.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), live)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    // A failed replacement restores its DELETE and releases the gate, preserving facts.
    let mut failed = db.begin_write_transaction().await.unwrap();
    lock_request_stats_projection_rebuild_in_transaction(&mut failed)
        .await
        .unwrap();
    sqlx::query("DELETE FROM session_usage_totals")
        .execute(&mut *failed)
        .await
        .unwrap();
    failed.rollback().await.unwrap();
    let mut retry = db.begin_write_transaction().await.unwrap();
    lock_request_stats_projection_rebuild_in_transaction(&mut retry)
        .await
        .unwrap();
    rebuild_request_session_projection_in_transaction(&mut retry, "tenant", "key", "session")
        .await
        .unwrap();
    retry.commit().await.unwrap();
    for table in [
        "session_usage_totals",
        "session_usage_hourly",
        "session_usage_daily",
    ] {
        let actual: (i64,i64,i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT CAST(SUM(requests) AS BIGINT), CAST(SUM(input_tokens) AS BIGINT), CAST(SUM(cost_micros) AS BIGINT) FROM {table}")))
            .fetch_one(&db.pool).await.unwrap();
        assert_eq!(
            actual,
            (2, 40, 14),
            "{table} remains exact after replacement, rollback and retry"
        );
    }
}

#[tokio::test]
async fn postgres_public_attach_waits_for_writer_before_source_access() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let fixture = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    let db = Arc::new(Database::connect_with_max(fixture.url(), 8).await.unwrap());
    db.migrate().await.unwrap();
    let mut writer = db.begin_write_transaction().await.unwrap();
    lock_request_stats_projection_writer_in_transaction(&mut writer)
        .await
        .unwrap();
    let attach_db = db.clone();
    let attach = tokio::spawn(async move {
        attach_db
            .attach_conversation_upstream_response(Uuid::new_v4(), "response-test")
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_locks WHERE locktype='advisory' AND mode='ExclusiveLock' AND NOT granted")
                .fetch_one(&db.pool).await.unwrap();
            if waiting > 0 { break; }
            assert!(!attach.is_finished(), "public attach reached its missing-source error before the exclusive gate");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    writer.commit().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), attach)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    // The missing-source error must release the exclusive transaction gate.
    let mut next = db.begin_write_transaction().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        lock_request_stats_projection_writer_in_transaction(&mut next),
    )
    .await
    .unwrap()
    .unwrap();
    next.rollback().await.unwrap();
}

async fn mixed_unlinked_delta_preserves_generation_without_replacement(db: &Database) {
    db.migrate().await.unwrap();
    let mut tx = db.begin_write_transaction().await.unwrap();
    lock_request_stats_projection_rebuild_in_transaction(&mut tx)
        .await
        .unwrap();
    insert_fact(&mut tx, "first").await;
    sqlx::query("UPDATE request_stats_facts SET session_id='unlinked:key',created_at=3000 WHERE request_id='first'").execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO generation_stats_facts(job_id,tenant_id,key_id,created_at,model,status_class,error_code,upstream_account_id,duration_ms,cost_micros,billed_units,currency,modality,billing_unit,model_route_id) VALUES ('generation','tenant','key',2000,'image','success','','',5,11,2,'USD','image','image','')").execute(&mut *tx).await.unwrap();
    rebuild_request_session_projection_in_transaction(&mut tx, "tenant", "key", "unlinked:key")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Forbid replacement: the ordinary membership delta must leave generation intact.
    if db.backend == DatabaseBackend::PostgreSql {
        sqlx::query("CREATE FUNCTION reject_session_replacement() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'unexpected full session replacement'; END $$").execute(&db.pool).await.unwrap();
        sqlx::query("CREATE TRIGGER reject_session_replacement BEFORE INSERT ON session_usage_totals FOR EACH ROW WHEN (NEW.session_id='unlinked:key') EXECUTE FUNCTION reject_session_replacement()").execute(&db.pool).await.unwrap();
    } else {
        sqlx::query("CREATE TRIGGER reject_session_replacement BEFORE INSERT ON session_usage_totals WHEN NEW.session_id='unlinked:key' BEGIN SELECT RAISE(ABORT,'unexpected full session replacement'); END").execute(&db.pool).await.unwrap();
    }
    let mut tx = db.begin_write_transaction().await.unwrap();
    lock_request_stats_projection_rebuild_in_transaction(&mut tx)
        .await
        .unwrap();
    let moved = sqlx::query("UPDATE request_stats_facts SET session_id='new-session' WHERE request_id='first' AND session_id='unlinked:key'").execute(&mut *tx).await.unwrap();
    assert_eq!(moved.rows_affected(), 1);
    let mut delta = first_delta();
    delta.previous_session_id = "unlinked:key".into();
    delta.created_at = 3000;
    remove_request_fact_from_session_projection_in_transaction(&mut tx, &delta)
        .await
        .unwrap();
    add_request_fact_to_session_projection_in_transaction(&mut tx, "first", 1)
        .await
        .unwrap();
    merge_request_session_projection_in_transaction(
        &mut tx,
        "tenant",
        "key",
        "unlinked:key",
        "new-session",
    )
    .await
    .unwrap();
    let replay=sqlx::query("UPDATE request_stats_facts SET session_id='new-session' WHERE request_id='first' AND session_id='unlinked:key'").execute(&mut *tx).await.unwrap();
    assert_eq!(replay.rows_affected(), 0);
    tx.commit().await.unwrap();
    let remaining:(i64,i64,i64,i64)=sqlx::query_as("SELECT requests,generation_units,cost_micros,last_activity_at FROM session_usage_totals WHERE session_id='unlinked:key'").fetch_one(&db.pool).await.unwrap();
    assert_eq!(remaining, (1, 2, 11, 2000));
    for table in [
        "session_usage_totals",
        "session_usage_hourly",
        "session_usage_daily",
    ] {
        let totals:(i64,i64,i64,i64)=sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT CAST(SUM(requests) AS BIGINT),CAST(SUM(input_tokens) AS BIGINT),CAST(SUM(generation_units) AS BIGINT),CAST(SUM(cost_micros) AS BIGINT) FROM {table}"))).fetch_one(&db.pool).await.unwrap();
        assert_eq!(
            totals,
            (2, 20, 2, 18),
            "{table} preserves both sources exactly once"
        );
    }
}

#[tokio::test]
async fn postgres_unlinked_membership_uses_delta_and_preserves_generation() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let fixture = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    let db = Database::connect_with_max(fixture.url(), 8).await.unwrap();
    mixed_unlinked_delta_preserves_generation_without_replacement(&db).await;
}

#[tokio::test]
async fn sqlite_unlinked_membership_uses_delta_and_preserves_generation() {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    mixed_unlinked_delta_preserves_generation_without_replacement(&db).await;
}
