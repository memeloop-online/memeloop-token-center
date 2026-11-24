use super::*;

#[tokio::test]
#[ignore = "requires the CI PostgreSQL and ClickHouse services"]
async fn postgres_clickhouse_watermark_keeps_frozen_head_across_coalescing_and_switch() {
    let postgres = std::env::var("MTC_TEST_POSTGRES_URL").unwrap();
    let endpoint = std::env::var("MTC_TEST_CLICKHOUSE_URL").unwrap();
    let schema = format!("history_watermark_{}", Uuid::now_v7().simple());
    let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut url = url::Url::parse(&postgres).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect(url.as_str()).await.unwrap();
    database.migrate().await.unwrap();
    database.history_start().await.unwrap();
    for source in [
        crate::request_history::HistorySource::Native,
        crate::request_history::HistorySource::Generation,
        crate::request_history::HistorySource::Archive,
        crate::request_history::HistorySource::Event,
    ] {
        assert_eq!(
            database
                .history_backfill_source_page(64, Some(source))
                .await
                .unwrap(),
            0
        );
    }
    let unfinished: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_history_backfill WHERE done=0")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(
        unfinished, 0,
        "empty source backfill must complete before W"
    );
    reqwest::Client::new()
        .post(&endpoint)
        .body(format!("CREATE DATABASE {schema}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let client = crate::request_history::ClickHouse::new(
        database
            .http_clients
            .handle(crate::http_settings::HttpPurpose::General),
        &endpoint,
        schema.clone(),
        "default".into(),
        String::new(),
    )
    .unwrap();
    client.initialize().await.unwrap();
    assert!(database.history_verify_watermark(&client).await.is_err());
    let source_database = database
        .clone()
        .with_auxiliary_pool_max_connections(1)
        .independent_persistence_pool();
    let owner = Uuid::now_v7();
    let mut source = database
        .history_begin_source_snapshot(&source_database, owner)
        .await
        .unwrap();
    while source
        .verify_next_page(&database, &client, &client, 64)
        .await
        .unwrap()
        != 0
    {}
    let extra = crate::request_history::HistorySnapshot {
        identity: crate::request_history::HistoryIdentity {
            source: "native".into(),
            tenant_id: Uuid::now_v7(),
            key_id: Uuid::now_v7(),
            request_id: Uuid::now_v7(),
            created_at: 1,
        },
        revision: 1,
        filter_cost_micros: None,
        payload: Some(serde_json::json!({"model":"extra","protocol":"openai"})),
        statistics: None,
        read_model: Value::Null,
    };
    client.append(&[extra]).await.unwrap();
    assert!(
        source
            .verify_surface_parity(&database, &client)
            .await
            .is_err(),
        "an extra CH identity is invisible to source-only readback but must fail surface parity"
    );
    reqwest::Client::new()
        .post(&endpoint)
        .body(format!("TRUNCATE TABLE {schema}.request_history_v1"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    source
        .verify_surface_parity(&database, &client)
        .await
        .unwrap();
    let receipt = source.finish(&database).await.unwrap();
    let epoch = receipt["epoch"].as_i64().unwrap();
    let other_schema = format!("{schema}_other");
    reqwest::Client::new()
        .post(&endpoint)
        .body(format!("CREATE DATABASE {other_schema}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let other = crate::request_history::ClickHouse::new(
        database
            .http_clients
            .handle(crate::http_settings::HttpPurpose::General),
        &endpoint,
        other_schema.clone(),
        "default".into(),
        String::new(),
    )
    .unwrap();
    other.initialize().await.unwrap();
    assert!(
        database.history_verify_watermark(&other).await.is_err(),
        "a physically different empty CH target cannot reuse baseline A"
    );
    assert!(
        database.history_check_export_target(&other).await.is_err(),
        "worker cannot ACK into target B using A's authority"
    );
    // A complete, version-bound baseline is not sufficient until its delivery
    // fence has been resumed through the existing exact owner/epoch operation.
    assert!(database.history_verify_watermark(&client).await.is_err());
    database
        .history_resume_delivery(owner, epoch)
        .await
        .unwrap();
    // Block the held-barrier scan, not acquisition of the state row. The
    // operation budget must roll back and release that state lock.
    let mut blocker = database.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE request_history_export IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let blocked = database.history_verify_watermark(&client).await;
    assert!(blocked.is_err(), "blocked scan must fail: {blocked:?}");
    eprintln!("WATERMARK_BLOCKED_SCAN_RESULT {blocked:?}");
    let elapsed = started.elapsed();
    assert!(elapsed >= std::time::Duration::from_secs(2));
    assert!(elapsed < std::time::Duration::from_secs(6));
    blocker.rollback().await.unwrap();
    let mut probe = database.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL statement_timeout='1s'")
        .execute(&mut *probe)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM request_history_state WHERE id=1 FOR SHARE NOWAIT")
        .fetch_one(&mut *probe)
        .await
        .unwrap();
    let unsigned: Option<String> =
        sqlx::query_scalar("SELECT verification_json FROM request_history_state WHERE id=1")
            .fetch_one(&mut *probe)
            .await
            .unwrap();
    assert!(
        unsigned.is_none(),
        "timeout cannot publish a watermark proof"
    );
    probe.rollback().await.unwrap();
    eprintln!(
        "WATERMARK_BARRIER_TIMEOUT_ROLLED_BACK elapsed_ms={}",
        elapsed.as_millis()
    );
    let event = Uuid::now_v7();
    let tenant = Uuid::now_v7();
    let key = Uuid::now_v7();
    let parent = Uuid::now_v7();
    sqlx::query("INSERT INTO request_events(event_id,tenant_id,key_id,request_id,event_at,event_kind,protocol,model,input_tokens,output_tokens,cost_micros) VALUES($1,$2,$3,$4,42,'finished','openai','first',1,1,0)").bind(event.to_string()).bind(tenant.to_string()).bind(key.to_string()).bind(parent.to_string()).execute(&database.pool).await.unwrap();
    assert!(
        database.history_verify_watermark(&client).await.is_err(),
        "unfrozen coalescible head cannot establish W"
    );
    database.history_capture_pending(64).await.unwrap();
    let first = database.history_verify_watermark(&client).await.unwrap();
    assert_eq!(first["cutover_ready"], false);
    let w = first["watermark"].as_i64().unwrap();
    sqlx::query("UPDATE request_events SET model='second' WHERE event_id=$1 AND event_at=42")
        .bind(event.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    database.history_capture_pending(64).await.unwrap();
    let snapshots = database.history_immutable_pending(64, None).await.unwrap();
    assert_eq!(snapshots.len(), 2);
    let old = snapshots.iter().find(|r| r.revision == w).unwrap().clone();
    let new = snapshots.iter().find(|r| r.revision > w).unwrap().clone();
    client.append(&snapshots).await.unwrap();
    database
        .history_acknowledge(std::slice::from_ref(&new))
        .await
        .unwrap();
    let pending = database.history_verify_watermark(&client).await.unwrap();
    assert_eq!(pending["watermark"], w);
    assert_eq!(
        pending["cutover_ready"], false,
        "old frozen obligation survives export coalescing and newer ACK"
    );
    assert!(database.history_switch_watermark(&client).await.is_err());
    database
        .history_acknowledge(std::slice::from_ref(&old))
        .await
        .unwrap();
    let ready = database.history_verify_watermark(&client).await.unwrap();
    assert_eq!(ready["watermark"], w);
    assert_eq!(ready["cutover_ready"], true);
    for column in ["read_model_version", "activation_epoch"] {
        let query = format!(
            "UPDATE request_history_baseline_runs SET {column}={column}+1 WHERE owner=$1 AND epoch=$2"
        );
        sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(owner.to_string())
            .bind(epoch)
            .execute(&database.pool)
            .await
            .unwrap();
        assert!(database.history_switch_watermark(&client).await.is_err());
        let query = format!(
            "UPDATE request_history_baseline_runs SET {column}={column}-1 WHERE owner=$1 AND epoch=$2"
        );
        sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(owner.to_string())
            .bind(epoch)
            .execute(&database.pool)
            .await
            .unwrap();
    }
    // Further live capture is allowed: this proof covers heads at W, not zero lag.
    sqlx::query("UPDATE request_events SET model='third' WHERE event_id=$1 AND event_at=42")
        .bind(event.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(database.history_switch_watermark(&other).await.is_err());
    database.history_switch_watermark(&client).await.unwrap();
    assert_eq!(database.history_mode().await.unwrap(), "clickhouse");
    let queued: i64 = sqlx::query_scalar(
        "SELECT revision FROM request_history_export WHERE source='event' AND request_id=$1",
    )
    .bind(event.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert!(queued > w);
    database.history_switch(false).await.unwrap();
    assert_eq!(database.history_mode().await.unwrap(), "shadow");
    source_database.close().await;
    database.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    reqwest::Client::new()
        .post(&endpoint)
        .body(format!("DROP DATABASE {other_schema}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    reqwest::Client::new()
        .post(&endpoint)
        .body(format!("DROP DATABASE {schema}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}
