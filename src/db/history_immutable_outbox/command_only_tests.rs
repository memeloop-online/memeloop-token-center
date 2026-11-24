use super::*;
use futures_util::TryStreamExt;

fn envelope(
    identity: &crate::request_history::HistoryIdentity,
    revision: i64,
    size: usize,
) -> Value {
    serde_json::json!({"version":1,"source":identity.source,"revision":revision,"request_id":identity.request_id,"tenant_id":identity.tenant_id,"key_id":identity.key_id,"created_at":identity.created_at,"payload":null,"read_model":null,"statistics":null,"filter_cost_micros":null,"originals":{"event":{"error_code":"x".repeat(size)}}})
}

async fn command_only_contract(db: &Database) {
    db.migrate().await.unwrap();
    for (index, size) in [0, 65536, 2 * 1024 * 1024].into_iter().enumerate() {
        let identity = crate::request_history::HistoryIdentity {
            source: "event".into(),
            request_id: Uuid::now_v7(),
            tenant_id: Uuid::now_v7(),
            key_id: Uuid::now_v7(),
            created_at: 42,
        };
        let revision = 100 + index as i64;
        let value = envelope(&identity, revision, size);
        let frozen = serde_json::to_string(&value).unwrap();
        let mut tx = db.pool.begin().await.unwrap();
        insert_envelope_in_transaction(&mut tx, &identity, revision, &value)
            .await
            .unwrap();
        insert_envelope_in_transaction(&mut tx, &identity, revision, &value)
            .await
            .unwrap();
        // Exercise the exact helper SQL through the protocol executor. A large
        // snapshot must yield a command result and no returned DataRow.
        let statement = immutable_envelope_upsert_sql(tx.as_mut().backend_name() == "PostgreSQL");
        assert!(!statement.contains("RETURNING"));
        let mut output = sqlx::Executor::fetch_many(
            &mut *tx,
            sqlx::query(sqlx::AssertSqlSafe(statement))
                .bind(revision)
                .bind(&identity.source)
                .bind(identity.request_id.to_string())
                .bind(identity.tenant_id.to_string())
                .bind(identity.key_id.to_string())
                .bind(identity.created_at)
                .bind(&frozen),
        );
        let mut commands = 0;
        while let Some(result) = output.try_next().await.unwrap() {
            match result {
                sqlx::Either::Left(done) => {
                    assert_eq!(done.rows_affected(), 1);
                    commands += 1;
                }
                sqlx::Either::Right(_) => panic!("immutable capture returned a snapshot DataRow"),
            }
        }
        drop(output);
        assert_eq!(commands, 1);
        tx.commit().await.unwrap();
        for field in [
            "source",
            "request_id",
            "tenant_id",
            "key_id",
            "created_at",
            "snapshot",
        ] {
            let mut tx = db.pool.begin().await.unwrap();
            let mut wrong = identity.clone();
            let mut changed = value.clone();
            match field {
                "source" => wrong.source = "native".into(),
                "request_id" => wrong.request_id = Uuid::now_v7(),
                "tenant_id" => wrong.tenant_id = Uuid::now_v7(),
                "key_id" => wrong.key_id = Uuid::now_v7(),
                "created_at" => wrong.created_at += 1,
                _ => changed["different_bytes"] = Value::Bool(true),
            };
            assert!(
                insert_envelope_in_transaction(&mut tx, &wrong, revision, &changed)
                    .await
                    .is_err(),
                "{field}"
            );
            tx.rollback().await.unwrap();
            let stored: String = sqlx::query_scalar(
                "SELECT snapshot FROM request_history_immutable_outbox WHERE revision=$1",
            )
            .bind(revision)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert_eq!(stored, frozen);
        }
        // JSON-equivalent text with different bytes must retain the old Rust
        // String equality contract, regardless of database collation.
        sqlx::query("UPDATE request_history_immutable_outbox SET snapshot=$1 WHERE revision=$2")
            .bind(format!(" {frozen}"))
            .bind(revision)
            .execute(&db.pool)
            .await
            .unwrap();
        let mut tx = db.pool.begin().await.unwrap();
        assert!(
            insert_envelope_in_transaction(&mut tx, &identity, revision, &value)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
        let stored: String = sqlx::query_scalar(
            "SELECT snapshot FROM request_history_immutable_outbox WHERE revision=$1",
        )
        .bind(revision)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(stored, format!(" {frozen}"));
        println!(
            "IMMUTABLE_COMMAND_ONLY bytes={} data_rows=0 commands=1 identity_and_byte_conflicts_rejected=true",
            frozen.len()
        );
    }
}

#[tokio::test]
async fn immutable_capture_command_only_keeps_identity_and_exact_bytes() {
    let root = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        root.path().join("command.db").display()
    ))
    .await
    .unwrap();
    command_only_contract(&db).await;
}

#[tokio::test]
#[ignore = "requires the CI PostgreSQL service"]
async fn postgres_immutable_capture_command_only_keeps_identity_and_exact_bytes() {
    let postgres = std::env::var("MTC_TEST_POSTGRES_URL").unwrap();
    let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
    let schema = format!("history_command_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut url = url::Url::parse(&postgres).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let db = Database::connect(url.as_str()).await.unwrap();
    command_only_contract(&db).await;
    // Concurrent retries share the existing compact identity lock and never
    // emit a large DataRow while the first transaction holds its revision.
    let identity = crate::request_history::HistoryIdentity {
        source: "event".into(),
        request_id: Uuid::now_v7(),
        tenant_id: Uuid::now_v7(),
        key_id: Uuid::now_v7(),
        created_at: 42,
    };
    let value = envelope(&identity, 999, 2 * 1024 * 1024);
    let mut first = db.pool.begin().await.unwrap();
    insert_envelope_in_transaction(&mut first, &identity, 999, &value)
        .await
        .unwrap();
    let retry_db = db.clone();
    let retry_id = identity.clone();
    let retry_value = value.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let retry = tokio::spawn(async move {
        let mut tx = retry_db.pool.begin().await.unwrap();
        started_tx.send(()).unwrap();
        insert_envelope_in_transaction(&mut tx, &retry_id, 999, &retry_value)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    });
    started_rx.await.unwrap();
    tokio::task::yield_now().await;
    assert!(!retry.is_finished());
    first.commit().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), retry)
        .await
        .unwrap()
        .unwrap();
    let stored: String = sqlx::query_scalar(
        "SELECT snapshot FROM request_history_immutable_outbox WHERE revision=999",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(stored, serde_json::to_string(&value).unwrap());
    db.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
