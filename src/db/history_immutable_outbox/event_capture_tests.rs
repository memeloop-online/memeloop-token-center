use super::*;

async fn old_event_envelope(
    tx: &mut Transaction<'_, Any>,
    identity: &crate::request_history::HistoryIdentity,
    revision: i64,
) -> Value {
    let payload = Database::history_original_event_in_transaction(tx, identity)
        .await
        .unwrap();
    let archive = sqlx::query("SELECT e.request_id,CASE WHEN EXISTS(SELECT 1 FROM generation_jobs g WHERE g.id=e.request_id AND g.tenant_id=e.tenant_id AND g.key_id=e.key_id) THEN 'generation' ELSE 'native' END AS source FROM request_events e WHERE e.event_id=$1 AND e.tenant_id=$2 AND e.key_id=$3 AND e.event_at=$4")
        .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(identity.key_id.to_string()).bind(identity.created_at).fetch_optional(&mut **tx).await.unwrap();
    let archive = archive.map(|r| serde_json::json!({"tenant_id":identity.tenant_id,"request_id":r.try_get::<String,_>("request_id").unwrap(),"source":r.try_get::<String,_>("source").unwrap()}));
    let original = original_event_in_transaction(tx, identity).await.unwrap();
    serde_json::json!({"originals":{"event":original},"version":1,"source":"event","revision":revision,"request_id":identity.request_id,"tenant_id":identity.tenant_id,"key_id":identity.key_id,"created_at":identity.created_at,"payload":payload,"read_model":null,"statistics":null,"filter_cost_micros":null,"archive_identity":archive})
}

async fn event_capture_parity(db: &Database) {
    db.migrate().await.unwrap();
    sqlx::query("UPDATE request_history_state SET mode='shadow' WHERE id=1")
        .execute(&db.pool)
        .await
        .unwrap();
    let tenant = Uuid::now_v7();
    let key = Uuid::now_v7();
    let principal = Uuid::now_v7();
    sqlx::query("INSERT INTO tenants(id,external_id,created_at) VALUES($1,$1,1)")
        .bind(tenant.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO principals(id,tenant_id,external_id,created_at) VALUES($1,$2,$1,1)")
        .bind(principal.to_string())
        .bind(tenant.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO key_records(id,tenant_id,principal_id,account_id,alias,currency,policy_json,status,credential_generation,created_at,updated_at) VALUES($1,$2,$3,$1,'fixture','USD','{}','active',1,1,1)").bind(key.to_string()).bind(tenant.to_string()).bind(principal.to_string()).execute(&db.pool).await.unwrap();
    for (index, kind) in ["native", "generation", "deleted", "missing", "wrong-scope"]
        .into_iter()
        .enumerate()
    {
        let parent = Uuid::now_v7();
        let event = Uuid::now_v7();
        let revision = 100 + index as i64;
        if kind == "native" {
            sqlx::query("INSERT INTO request_records(id,tenant_id,key_id,created_at,completed_at,protocol,model,status_code,input_tokens,output_tokens,cost_micros,request_object,reservation_id) VALUES($1,$2,$3,1,2,'openai','fixture',200,3,2,5,'gap://fixture/request',$4)").bind(parent.to_string()).bind(tenant.to_string()).bind(key.to_string()).bind(Uuid::now_v7().to_string()).execute(&db.pool).await.unwrap();
        } else if kind == "generation" {
            sqlx::query("INSERT INTO generation_jobs(id,tenant_id,key_id,upstream_account_id,reservation_id,public_model,upstream_model,driver,status,request_object,estimated_units,next_attempt_at,created_at,updated_at) VALUES($1,$2,$3,$4,$5,'fixture','fixture','http-json','queued','gap://fixture/request',1,1,1,1)").bind(parent.to_string()).bind(tenant.to_string()).bind(key.to_string()).bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).execute(&db.pool).await.unwrap();
        }
        if kind != "missing" {
            sqlx::query("INSERT INTO request_events(event_id,tenant_id,key_id,request_id,event_at,event_kind,protocol,model,status_code,duration_ms,input_tokens,output_tokens,cost_micros,error_code) VALUES($1,$2,$3,$4,42,'finished','openai','fixture',200,NULL,3,2,5,NULL)").bind(event.to_string()).bind(tenant.to_string()).bind(key.to_string()).bind(parent.to_string()).execute(&db.pool).await.unwrap();
        }
        let identity = crate::request_history::HistoryIdentity {
            source: "event".into(),
            request_id: event,
            tenant_id: if kind == "wrong-scope" {
                Uuid::now_v7()
            } else {
                tenant
            },
            key_id: key,
            created_at: 42,
        };
        sqlx::query("INSERT INTO request_history_export(revision,source,request_id,tenant_id,key_id,created_at) VALUES($1,'event',$2,$3,$4,42)").bind(revision).bind(event.to_string()).bind(identity.tenant_id.to_string()).bind(key.to_string()).execute(&db.pool).await.unwrap();
        let mut tx = db.pool.begin().await.unwrap();
        let old = old_event_envelope(&mut tx, &identity, revision).await;
        capture_event_envelope_in_transaction(&mut tx, &identity, revision)
            .await
            .unwrap();
        let frozen: String = sqlx::query_scalar(
            "SELECT snapshot FROM request_history_immutable_outbox WHERE revision=$1",
        )
        .bind(revision)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&frozen).unwrap(),
            old,
            "{kind}"
        );
        assert!(!frozen.contains("capture_archive_source"));
        tx.commit().await.unwrap();
        let snapshot = db
            .history_immutable_pending(64, None)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.revision == revision)
            .unwrap();
        let hash = crate::request_history::canonical_hash(&snapshot).unwrap();
        // Source changes after freezing must not recreate this revision.
        sqlx::query("UPDATE request_events SET model='changed-after-capture' WHERE event_id=$1 AND event_at=42").bind(event.to_string()).execute(&db.pool).await.unwrap();
        db.history_capture_pending(64).await.unwrap();
        let again = db
            .history_immutable_pending(64, None)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.revision == revision)
            .unwrap();
        assert_eq!(
            crate::request_history::canonical_hash(&again).unwrap(),
            hash
        );
        println!(
            "EVENT_CAPTURE_PARITY kind={kind} static_source_reads_before=3 static_source_reads_after=1 static_direct_roundtrips_before=3 static_direct_roundtrips_after=2 hash={hash}"
        );
    }
}

#[tokio::test]
async fn event_single_source_capture_matches_original_envelopes() {
    let root = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        root.path().join("events.db").display()
    ))
    .await
    .unwrap();
    event_capture_parity(&db).await;
}

#[tokio::test]
#[ignore = "requires the CI PostgreSQL and ClickHouse services"]
async fn postgres_clickhouse_event_single_source_capture_matches_original_envelopes() {
    let postgres = std::env::var("MTC_TEST_POSTGRES_URL").unwrap();
    let endpoint = std::env::var("MTC_TEST_CLICKHOUSE_URL").unwrap();
    let schema = format!("event_capture_{}", Uuid::now_v7().simple());
    let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut url = url::Url::parse(&postgres).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let db = Database::connect(url.as_str()).await.unwrap();
    event_capture_parity(&db).await;
    reqwest::Client::new()
        .post(&endpoint)
        .body(format!("CREATE DATABASE {schema}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let ch = crate::request_history::ClickHouse::new(
        db.http_clients
            .handle(crate::http_settings::HttpPurpose::General),
        &endpoint,
        schema.clone(),
        "default".into(),
        String::new(),
    )
    .unwrap();
    ch.initialize().await.unwrap();
    let pending = db.history_immutable_pending(64, None).await.unwrap();
    ch.append(&pending).await.unwrap();
    ch.append(&pending).await.unwrap();
    let ids: Vec<_> = pending.iter().map(|r| r.identity.clone()).collect();
    let actual = ch.baseline_snapshots(&ids).await.unwrap();
    for (expected, actual) in pending.iter().zip(actual) {
        assert_eq!(
            crate::request_history::canonical_hash(expected).unwrap(),
            crate::request_history::canonical_hash(&actual.unwrap()).unwrap()
        );
    }
    db.history_acknowledge(&pending).await.unwrap();
    assert_eq!(db.history_pending_count().await.unwrap(), 0);
    db.history_capture_pending(0).await.unwrap();
    assert!(
        db.history_immutable_pending(64, None)
            .await
            .unwrap()
            .is_empty()
    );
    db.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    reqwest::Client::new()
        .post(&endpoint)
        .body(format!("DROP DATABASE {schema}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    admin.close().await;
}
