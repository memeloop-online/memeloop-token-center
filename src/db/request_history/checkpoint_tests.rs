use super::*;
use crate::request_history::{HistoryBackfillCheckpoint, HistorySource};

async fn checkpoint(db: &Database, source: &str) -> HistoryBackfillCheckpoint {
    let row=sqlx::query("SELECT source,after_id,after_created_at,legacy_after_id,done,(SELECT epoch FROM request_history_activation WHERE id=1) AS epoch FROM request_history_backfill WHERE source=$1")
        .bind(source).fetch_one(&db.pool).await.unwrap();
    HistoryBackfillCheckpoint {
        source: row.try_get("source").unwrap(),
        after_id: row.try_get("after_id").unwrap(),
        after_created_at: row.try_get("after_created_at").unwrap(),
        legacy_after_id: row.try_get("legacy_after_id").unwrap(),
        epoch: row.try_get("epoch").unwrap(),
        done: row.try_get::<i64, _>("done").unwrap() != 0,
    }
}

async fn fingerprint(db: &Database) -> Value {
    let rows=sqlx::query("SELECT source,after_id,after_created_at,legacy_after_id,done FROM request_history_backfill ORDER BY source").fetch_all(&db.pool).await.unwrap();
    let cursors = rows
        .iter()
        .map(|row| {
            serde_json::json!([
                row.try_get::<String, _>("source").unwrap(),
                row.try_get::<String, _>("after_id").unwrap(),
                row.try_get::<Option<i64>, _>("after_created_at").unwrap(),
                row.try_get::<Option<String>, _>("legacy_after_id").unwrap(),
                row.try_get::<i64, _>("done").unwrap()
            ])
        })
        .collect::<Vec<_>>();
    let rows=sqlx::query("SELECT source,request_id,revision,tenant_id,key_id,created_at FROM request_history_export ORDER BY source,request_id").fetch_all(&db.pool).await.unwrap();
    let queue = rows
        .iter()
        .map(|row| {
            serde_json::json!([
                row.try_get::<String, _>("source").unwrap(),
                row.try_get::<String, _>("request_id").unwrap(),
                row.try_get::<i64, _>("revision").unwrap(),
                row.try_get::<String, _>("tenant_id").unwrap(),
                row.try_get::<String, _>("key_id").unwrap(),
                row.try_get::<i64, _>("created_at").unwrap()
            ])
        })
        .collect::<Vec<_>>();
    serde_json::json!({"cursors":cursors,"queue":queue})
}

async fn exercise(db: &Database, client: Option<&crate::request_history::ClickHouse>) {
    db.migrate().await.unwrap();
    // Synthetic pre-capture history: no source triggers enqueue these seeds.
    sqlx::query("UPDATE request_history_state SET mode='shadow' WHERE id=1")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE request_history_backfill SET done=1 WHERE source IN ('archive','generation')",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let untouched = [
        checkpoint(db, "archive").await,
        checkpoint(db, "generation").await,
    ];
    let pepper = b"checkpoint-fixture-pepper-at-least-32-bytes";
    let issued = db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "checkpoint-tenant".into(),
                principal_external_id: "checkpoint-user".into(),
                alias: "checkpoint-key".into(),
                currency: "USD".into(),
                policy: crate::model::KeyPolicy::default(),
                initial_balance: rust_decimal::Decimal::ZERO,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = db.authenticate_key(&issued.key, pepper).await.unwrap();
    let ids = [
        "00000000-0000-4000-8000-000000000002",
        "00000000-0000-4000-8000-000000000001",
    ];
    for id in ids {
        sqlx::query("INSERT INTO request_events(event_id,tenant_id,key_id,request_id,event_at,event_kind,protocol,model,status_code,input_tokens,output_tokens,cost_micros) VALUES($1,$2,$3,$1,100,'finished','openai','checkpoint',200,1,2,0)")
            .bind(id).bind(key.tenant_id.to_string()).bind(key.key_id.to_string()).execute(&db.pool).await.unwrap();
        sqlx::query("INSERT INTO request_records(id,tenant_id,key_id,created_at,protocol,model,input_tokens,output_tokens,cost_micros,request_object,reservation_id,currency) VALUES($1,$2,$3,101,'openai','checkpoint',0,0,0,'gap://fixture/request',$4,'USD')")
            .bind(id).bind(key.tenant_id.to_string()).bind(key.key_id.to_string()).bind(Uuid::now_v7().to_string()).execute(&db.pool).await.unwrap();
        sqlx::query("INSERT INTO request_record_locators(id,created_at,tenant_id,key_id) VALUES($1,101,$2,$3)")
            .bind(id).bind(key.tenant_id.to_string()).bind(key.key_id.to_string()).execute(&db.pool).await.unwrap();
    }
    // Preserve the original UUID-to-time cursor conversion exactly.
    sqlx::query("UPDATE request_history_backfill SET after_id='ffffffff-ffff-4fff-8fff-ffffffffffff' WHERE source='event'").execute(&db.pool).await.unwrap();
    let expected = checkpoint(db, "event").await;
    let before = fingerprint(db).await;
    let mut stale = expected.clone();
    stale.epoch += 1;
    assert!(matches!(
        db.history_backfill_expected_page(1, &stale).await,
        Err(HistoryError::Changed)
    ));
    assert_eq!(fingerprint(db).await, before);

    if matches!(db.backend, DatabaseBackend::PostgreSql) {
        // Interrupt after the first INSERT but while the second unique key is
        // held elsewhere. The whole page, including its cursor, must roll back.
        let mut held = db.pool.begin().await.unwrap();
        sqlx::query("INSERT INTO request_history_export(source,request_id,tenant_id,key_id,created_at) VALUES('event',$1,$2,$3,100)")
            .bind(ids[0]).bind(key.tenant_id.to_string()).bind(key.key_id.to_string()).execute(&mut *held).await.unwrap();
        let timed_out = db
            .history_backfill_expected_page(2, &expected)
            .await
            .unwrap_err();
        match timed_out {
            HistoryError::Sql(error) => assert_eq!(
                error
                    .as_database_error()
                    .and_then(|error| error.code())
                    .as_deref(),
                Some("55P03")
            ),
            other => panic!("expected maintenance lock budget failure: {other:?}"),
        }
        // The unrelated writer was not cancelled by maintenance timeout.
        let alive: i64 = sqlx::query_scalar("SELECT 1::BIGINT")
            .fetch_one(&mut *held)
            .await
            .unwrap();
        assert_eq!(alive, 1);
        held.rollback().await.unwrap();
        // Waiting for the rolled-back checkpoint row proves the dropped SQLx
        // transaction released its connection before checking partial inserts.
        let mut check = db.pool.begin().await.unwrap();
        sqlx::query("SELECT source FROM request_history_backfill WHERE source='event' FOR UPDATE")
            .fetch_one(&mut *check)
            .await
            .unwrap();
        check.rollback().await.unwrap();
        assert_eq!(fingerprint(db).await, before);
    }
    // A source key with another tenant is not a delivered/pending obligation.
    sqlx::query("INSERT INTO request_history_export(source,request_id,tenant_id,key_id,created_at) VALUES('event',$1,$2,$3,100)")
        .bind(ids[1]).bind(Uuid::now_v7().to_string()).bind(key.key_id.to_string()).execute(&db.pool).await.unwrap();
    let collision = fingerprint(db).await;
    assert!(matches!(
        db.history_backfill_expected_page(1, &expected).await,
        Err(HistoryError::Changed)
    ));
    assert_eq!(fingerprint(db).await, collision);
    sqlx::query("DELETE FROM request_history_export WHERE source='event' AND request_id=$1")
        .bind(ids[1])
        .execute(&db.pool)
        .await
        .unwrap();
    let first = db
        .history_backfill_expected_page(1, &expected)
        .await
        .unwrap();
    assert_eq!(
        (
            first.visited,
            first.actually_enqueued,
            first.covered,
            first.conflict
        ),
        (1, 1, 0, 0)
    );
    assert_eq!(first.identities[0]["request_id"], ids[1]);
    assert!(first.identities[0]["revision"].as_i64().unwrap() > 0);
    let second_expected = first.after.unwrap();
    assert_eq!(second_expected.after_created_at, Some(100));
    assert_eq!(
        second_expected.legacy_after_id,
        Some(expected.after_id.clone())
    );
    let advanced = fingerprint(db).await;
    assert!(matches!(
        db.history_backfill_expected_page(1, &expected).await,
        Err(HistoryError::Changed)
    ));
    assert_eq!(fingerprint(db).await, advanced);
    // A fresh handle resumes from the committed time+ID boundary, not memory.
    // A pre-existing journal row is pending conflict, never covered or ACK.
    sqlx::query("INSERT INTO request_history_export(source,request_id,tenant_id,key_id,created_at) VALUES('event',$1,$2,$3,100)")
        .bind(ids[0]).bind(key.tenant_id.to_string()).bind(key.key_id.to_string()).execute(&db.pool).await.unwrap();
    let pending_revision: i64 = sqlx::query_scalar(
        "SELECT revision FROM request_history_export WHERE source='event' AND request_id=$1",
    )
    .bind(ids[0])
    .fetch_one(&db.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO request_history_identities(source,request_id,tenant_id,key_id,created_at,head_revision,acknowledged_revision,acknowledged_hash) VALUES('event',$1,$2,$3,100,$4,$4,$5)")
        .bind(ids[0]).bind(key.tenant_id.to_string()).bind(key.key_id.to_string()).bind(pending_revision-1).bind("a".repeat(64)).execute(&db.pool).await.unwrap();
    let second = db
        .clone()
        .history_backfill_expected_page(1, &second_expected)
        .await
        .unwrap();
    assert_eq!(second.identities[0]["request_id"], ids[0]);
    assert_eq!(
        (
            second.visited,
            second.actually_enqueued,
            second.covered,
            second.conflict
        ),
        (1, 0, 0, 1)
    );
    assert_eq!(second.identities[0]["proof"], "journal_pending");
    assert_eq!(second.identities[0]["revision"], pending_revision);
    assert_eq!(
        second.identities[0]["acknowledged_revision"],
        pending_revision - 1
    );
    assert_eq!(second.identities[0]["revision_acknowledged"], false);
    let final_page = db
        .history_backfill_expected_page(1, &second.after.unwrap())
        .await
        .unwrap();
    assert_eq!(final_page.visited, 0);
    assert!(final_page.after.unwrap().done);

    let native = checkpoint(db, "native").await;
    let first_native = db.history_backfill_expected_page(1, &native).await.unwrap();
    assert_eq!(first_native.identities[0]["request_id"], ids[1]);
    let second_native = db
        .history_backfill_expected_page(1, &first_native.after.unwrap())
        .await
        .unwrap();
    assert_eq!(second_native.identities[0]["request_id"], ids[0]);
    let end = db
        .history_backfill_expected_page(1, &second_native.after.unwrap())
        .await
        .unwrap();
    assert!(end.after.unwrap().done);
    assert_eq!(
        [
            checkpoint(db, "archive").await,
            checkpoint(db, "generation").await
        ],
        untouched
    );

    if let Some(client) = client {
        let cursors = fingerprint(db).await["cursors"].clone();
        for id in ids {
            let revision:i64=sqlx::query_scalar("SELECT revision FROM request_history_export WHERE source='event' AND request_id=$1").bind(id).fetch_one(&db.pool).await.unwrap();
            let identity = crate::request_history::HistoryIdentity {
                source: "event".into(),
                request_id: Uuid::parse_str(id).unwrap(),
                tenant_id: key.tenant_id,
                key_id: key.key_id,
                created_at: 100,
            };
            let scope = crate::request_history::ExactEventExport {
                identity: identity.clone(),
                revision,
            };
            let frozen = db
                .history_pending_scoped(1, None, Some(&scope))
                .await
                .unwrap();
            assert_eq!(frozen.len(), 1);
            let hash = crate::request_history::canonical_hash(&frozen[0]).unwrap();
            client.append(&frozen).await.unwrap();
            assert_eq!(db.history_acknowledge_count(&frozen).await.unwrap(), 1);
            let stored = client.baseline_snapshot(&identity).await.unwrap().unwrap();
            assert_eq!(
                crate::request_history::canonical_hash(&stored).unwrap(),
                hash
            );
            let ack:String=sqlx::query_scalar("SELECT acknowledged_hash FROM request_history_identities WHERE source='event' AND request_id=$1 AND acknowledged_revision=$2").bind(id).bind(revision).fetch_one(&db.pool).await.unwrap();
            assert_eq!(ack, hash);
        }
        assert_eq!(
            fingerprint(db).await["cursors"],
            cursors,
            "delivery never advances source cursors"
        );
        // Replay a legacy conversion with real ACK receipts. No new revision
        // or queue entry is needed and visited remains distinct from covered.
        sqlx::query("UPDATE request_history_backfill SET after_id='ffffffff-ffff-4fff-8fff-ffffffffffff',after_created_at=NULL,legacy_after_id=NULL,done=0 WHERE source='event'").execute(&db.pool).await.unwrap();
        let legacy = checkpoint(db, "event").await;
        let replay = db
            .history_backfill_expected_page(64, &legacy)
            .await
            .unwrap();
        assert_eq!(
            (
                replay.visited,
                replay.actually_enqueued,
                replay.covered,
                replay.conflict
            ),
            (2, 0, 2, 0)
        );
        assert!(
            replay
                .identities
                .iter()
                .all(|identity| identity["proof"] == "acknowledged")
        );
    }
    // Existing non-CAS API still follows its original shared implementation.
    assert_eq!(
        db.history_backfill_source_page(1, Some(HistorySource::Event))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn sqlite_expected_checkpoint_preserves_cursor_conflicts_and_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("checkpoint.db").display()
    ))
    .await
    .unwrap();
    exercise(&db, None).await;
}

#[tokio::test]
#[ignore = "requires existing isolated GHA PostgreSQL and ClickHouse services"]
async fn request_history_real_services_acceptance_expected_checkpoint_cancel_resume_and_ack() {
    let pg = std::env::var("MTC_TEST_POSTGRES_URL").unwrap();
    let endpoint = std::env::var("MTC_TEST_CLICKHOUSE_URL").unwrap();
    let schema = format!("checkpoint_{}", Uuid::now_v7().simple());
    let admin = sqlx::PgPool::connect(&pg).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut url = url::Url::parse(&pg).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let db = Database::connect(url.as_str())
        .await
        .unwrap()
        .with_archive(crate::archive::ArchiveStore::memory_for_test());
    reqwest::Client::new()
        .post(&endpoint)
        .body(format!("CREATE DATABASE {schema}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let client = crate::request_history::ClickHouse::new(
        db.http_clients
            .handle(crate::http_settings::HttpPurpose::General),
        &endpoint,
        schema.clone(),
        "default".into(),
        String::new(),
    )
    .unwrap();
    client.initialize().await.unwrap();
    exercise(&db, Some(&client)).await;
    db.close().await;
    reqwest::Client::new()
        .post(&endpoint)
        .body(format!("DROP DATABASE {schema}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
